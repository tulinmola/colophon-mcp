use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, JsonObject, MetaObject,
    ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::machine::{Condition, EXECUTE, IN, Match, OUT, READ, Run, WRITE};
use crate::machines::{self, Built, FileRead, MACHINES, Machine, Watched};

// Each is spoken by a test; those before 2026-07-28 open with initialize.
static REVISIONS: [ProtocolVersion; 3] = [
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];

// How far one request may run a machine: half a minute of either machine's time,
// a minute if its frames never come. 1500 of a CPC's standard frames ran in
// about 6 seconds where this was written, inside the 60 seconds a client of
// the protocol's TypeScript SDK waits for an answer by default:
// DEFAULT_REQUEST_TIMEOUT_MSEC, at
// https://github.com/modelcontextprotocol/typescript-sdk/blob/v2.3.1/packages/core-internal/src/shared/protocol.ts#L103
const MOST_FRAMES: u32 = 1500;
// The longest instruction takes a few microseconds, so these come to under a
// second of either machine's time.
const MOST_INSTRUCTIONS: u32 = 100_000;
// 8192 hexadecimal digits, well under the 10000 tokens of an answer at which
// Claude Code warns: https://code.claude.com/docs/en/mcp
const MOST_BYTES: u16 = 4096;
// A match takes some 80 characters, so these come to about as many as
// MOST_BYTES does.
const MOST_GATHERED: u32 = 128;
// Each is weighed against every access a run makes.
const MOST_CONDITIONS: usize = 16;

// A machine holds raw pointers, a CPC's into itself, so it is not Send, and
// the server must be. Each is built on a thread of its own, never leaves it,
// and is reached by the work sent there.
type Order = Box<dyn FnOnce(&mut Built) + Send>;

struct MachineThread {
    opened: u64,
    handle: String,
    machine: &'static Machine,
    orders: mpsc::Sender<Order>,
    thread: JoinHandle<()>,
}

impl MachineThread {
    fn close(self) {
        drop(self.orders);
        let _ = self.thread.join();
    }

    fn standing(&self) -> Standing {
        Standing {
            handle: self.handle.clone(),
            machine: self.machine.name.to_owned(),
        }
    }
}

#[derive(Default)]
struct Threads {
    opened: u64,
    standing: Vec<MachineThread>,
}

impl Threads {
    fn unknown(&self, handle: &str) -> String {
        let handles: Vec<&str> = self
            .standing
            .iter()
            .map(|standing| standing.handle.as_str())
            .collect();
        if handles.is_empty() {
            format!(
                "no machine has the handle {handle}; no machine is standing, and machine_open \
                 stands one up"
            )
        } else {
            format!(
                "no machine has the handle {handle}; the machines standing have the handles {}",
                handles.join(", ")
            )
        }
    }
}

pub struct Server {
    roms: PathBuf,
    threads: Mutex<Threads>,
}

fn identity() -> Implementation {
    Implementation::new(env!("CARGO_PKG_NAME"), env!("COLOPHON_MCP_BUILD"))
}

// The 2026-07-28 revision asks that every result name the server in its
// _meta, and rmcp names it only in the answer to server/discover:
// https://modelcontextprotocol.io/specification/2026-07-28/basic
fn identified() -> MetaObject {
    let mut meta = JsonObject::new();
    meta.insert(
        "io.modelcontextprotocol/serverInfo".to_owned(),
        serde_json::to_value(identity()).expect("a name and a version serialize"),
    );
    MetaObject(meta)
}

fn machine_names(_: &mut SchemaGenerator) -> Schema {
    let names: Vec<&str> = MACHINES.iter().map(|machine| machine.name).collect();
    json_schema!({ "type": "string", "enum": names })
}

// The schema the specification recommends for a tool that takes no arguments:
// https://modelcontextprotocol.io/specification/2026-07-28/server/tools
fn no_arguments() -> Arc<JsonObject> {
    let mut schema = JsonObject::new();
    schema.insert("type".into(), "object".into());
    schema.insert("additionalProperties".into(), false.into());
    Arc::new(schema)
}

// Any value a machine holds: a number, a truth, an enumerator's name, or the
// arrays and structures made of them. Each kind is spelled out, as a bare
// `true` would not carry to every client.
fn any_value() -> Value {
    json!({
        "anyOf": [
            { "type": "integer" },
            { "type": "boolean" },
            { "type": "string" },
            { "type": "array" },
            { "type": "object" },
        ]
    })
}

fn any_value_schema(_: &mut SchemaGenerator) -> Schema {
    Schema::try_from(any_value()).expect("an object is a schema")
}

fn values_schema(_: &mut SchemaGenerator) -> Schema {
    Schema::try_from(json!({ "type": "object", "additionalProperties": any_value() }))
        .expect("an object is a schema")
}

// An argument that may be left out is not one that may be null, or one whose
// default is null, and a type that admits null does not carry to every
// client.
fn absent_not_null(schema: &mut Schema) {
    if schema.get("default") == Some(&Value::Null) {
        schema.remove("default");
    }
    if let Some(Value::Array(types)) = schema.get_mut("type") {
        types.retain(|kind| kind != "null");
        if let [only] = types.as_slice() {
            let only = only.clone();
            schema.insert("type".to_owned(), only);
        }
    }
}

const ADDRESS_FORMS: &str = "an address is a number from 0 to 65535, or hexadecimal from &0000 to \
                             &FFFF, written with &, #, $ or 0x before it";

fn address_value() -> Value {
    json!({
        "anyOf": [
            { "type": "integer", "minimum": 0, "maximum": 65535 },
            { "type": "string", "pattern": "^(&|#|\\$|0x|0X)[0-9A-Fa-f]{1,4}$" },
        ]
    })
}

// An address, kept once in a tool's $defs and named from wherever it is
// taken.
struct AddressForms;

impl JsonSchema for AddressForms {
    fn schema_name() -> Cow<'static, str> {
        "Address".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        Schema::try_from(address_value()).expect("an object is a schema")
    }
}

fn address_schema(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<AddressForms>()
}

fn address(value: &Value) -> Result<u16, String> {
    if let Some(number) = value.as_u64() {
        return u16::try_from(number)
            .map_err(|_| format!("{value} is past &FFFF; {ADDRESS_FORMS}"));
    }
    let digits = value.as_str().and_then(|text| {
        ["&", "#", "$", "0x", "0X"]
            .iter()
            .find_map(|prefix| text.strip_prefix(prefix))
    });
    digits
        .filter(|digits| {
            (1..=4).contains(&digits.len()) && digits.chars().all(|digit| digit.is_ascii_hexdigit())
        })
        .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        .ok_or_else(|| format!("{value} is no address; {ADDRESS_FORMS}"))
}

const CONDITION_FORMS: &str = "a condition is {\"execute\": address}, {\"read\": address} or \
                               {\"write\": address}, each with \"to\" and the last address if it \
                               is a range, or {\"in\": port, \"mask\": bits} or {\"out\": port, \
                               \"mask\": bits}, the mask being the bits of the port's address that \
                               its device decodes";

// A condition, kept once in a tool's $defs, as until and gather both take
// lists of them.
struct ConditionForms;

impl JsonSchema for ConditionForms {
    fn schema_name() -> Cow<'static, str> {
        "Condition".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let address = generator.subschema_for::<AddressForms>().to_value();
        let condition = |kind: &str, other: &str, required: &[&str]| {
            json!({
                "type": "object",
                "properties": { kind: address, other: address },
                "required": required,
                "additionalProperties": false,
            })
        };
        let branches = [
            condition("execute", "to", &["execute"]),
            condition("read", "to", &["read"]),
            condition("write", "to", &["write"]),
            condition("in", "mask", &["in", "mask"]),
            condition("out", "mask", &["out", "mask"]),
        ];
        Schema::try_from(json!({ "oneOf": branches })).expect("an object is a schema")
    }
}

fn conditions_schema(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "array",
        "maxItems": MOST_CONDITIONS,
        "items": generator.subschema_for::<ConditionForms>(),
    })
}

fn condition(value: &Value, stops: bool) -> Result<Condition, String> {
    let fields = value
        .as_object()
        .ok_or_else(|| format!("{value} is not an object"))?;
    let kinds = [
        ("execute", EXECUTE),
        ("read", READ),
        ("write", WRITE),
        ("in", IN),
        ("out", OUT),
    ];
    let named: Vec<_> = kinds
        .iter()
        .filter(|(name, _)| fields.contains_key(*name))
        .collect();
    let &[&(name, kind)] = named.as_slice() else {
        let names: Vec<&str> = named.iter().map(|(name, _)| *name).collect();
        return Err(match names.split_last() {
            None => format!("{value} names no kind"),
            Some((last, rest)) => format!(
                "{value} names {} and {last}, and a condition names one",
                rest.join(", ")
            ),
        });
    };
    let first = address(&fields[name]).map_err(|problem| format!("{name} {problem}"))?;
    let port = kind == IN || kind == OUT;
    let other = if port { "mask" } else { "to" };
    if let Some(unknown) = fields.keys().find(|key| *key != name && *key != other) {
        return Err(format!("{value} has {unknown}, which {name} does not take"));
    }
    let (last, mask) = if port {
        let mask = fields
            .get("mask")
            .ok_or_else(|| format!("{value} has no mask"))?;
        let mask = address(mask).map_err(|problem| format!("mask {problem}"))?;
        (first, mask)
    } else {
        let last = fields
            .get("to")
            .map(|to| address(to).map_err(|problem| format!("to {problem}")))
            .transpose()?
            .unwrap_or(first);
        if last < first {
            return Err(format!(
                "{value} runs back from &{first:04X} to &{last:04X}, and a range runs upward"
            ));
        }
        (last, 0)
    };
    Ok(Condition {
        kind,
        first,
        last,
        mask,
        stops,
    })
}

// The conditions of until or of gather, added to those of the other.
fn conditions(
    field: &str,
    listed: Option<&Value>,
    stops: bool,
    into: &mut Vec<Condition>,
) -> Result<(), String> {
    let Some(listed) = listed else {
        return Ok(());
    };
    let listed = listed.as_array().ok_or_else(|| {
        format!(
            "{field} is a list of conditions, as [{{\"execute\": \"&0038\"}}]; {CONDITION_FORMS}"
        )
    })?;
    if listed.len() > MOST_CONDITIONS {
        return Err(format!(
            "{field} has {} conditions, and takes at most {MOST_CONDITIONS}",
            listed.len()
        ));
    }
    for (index, value) in listed.iter().enumerate() {
        let condition = condition(value, stops)
            .map_err(|problem| format!("{field}[{index}]: {problem}; {CONDITION_FORMS}"))?;
        into.push(condition);
    }
    Ok(())
}

fn watched_for(until: Option<&Value>, gather: Option<&Value>) -> Result<Vec<Condition>, String> {
    let mut watched = Vec::new();
    conditions("until", until, true, &mut watched)?;
    conditions("gather", gather, false, &mut watched)?;
    Ok(watched)
}

fn hexadecimal(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn bytes(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<char> = text
        .chars()
        .filter(|digit| !digit.is_whitespace())
        .collect();
    let pairs = digits.chunks(2).map(|pair| {
        let hexadecimal = pair.len() == 2 && pair.iter().all(char::is_ascii_hexdigit);
        let pair: String = pair.iter().collect();
        u8::from_str_radix(&pair, 16).ok().filter(|_| hexadecimal)
    });
    pairs.collect::<Option<Vec<u8>>>().ok_or_else(|| {
        format!(
            "{text} is not bytes: bytes are written as pairs of hexadecimal digits, as 3E05C9 or \
             3E 05 C9"
        )
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    /// The machine, by the name the emulator's command line gives it.
    #[schemars(schema_with = "machine_names")]
    machine: String,
    /// A snapshot to load into the machine once it is built: its path on the file system the server runs on, a relative one read from the server's working directory.
    #[schemars(transform = absent_not_null)]
    snapshot: Option<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    /// The handle machine_open gave the machine.
    handle: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// How many times the monitor is to retrace, from 1 to 1500.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    frames: Option<i64>,
    /// How many ticks to run, from 1 to the ticks of 1500 standard frames.
    #[schemars(range(min = 1))]
    #[schemars(transform = absent_not_null)]
    ticks: Option<i64>,
    /// Conditions that stop the run at their first match. {"execute": address} stops before the instruction there begins; {"read": address} stops at the end of an instruction that reads the byte there as data, and {"write": address} at the end of one that writes it; each takes "to" and a last address for a range. {"in": port, "mask": bits} and {"out": port, "mask": bits} stop at the end of an instruction that reads or writes a port whose address agrees with the port on the bits of the mask, which are the bits its device decodes: {"out": "&7F00", "mask": "&C000"} is a CPC's Gate Array, and {"out": "&FE", "mask": "&01"} a Spectrum's ULA. An instruction's own bytes are never read as data. A run that begins where an execute condition matches runs that instruction first. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    until: Option<Value>,
    /// Conditions whose matches are gathered as the run goes, written as until's are. The run stops at the end of the instruction that makes the 128th match, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too; the next run gathers on from there. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    gather: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Where the routine begins.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// Values to write first, by their paths, as set writes them: {"cpu.a": 1, "cpu.h": 64, "cpu.l": 0}.
    #[schemars(schema_with = "values_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    values: Option<Map<String, Value>>,
    /// How many times the monitor may retrace before the call stops waiting for the routine to return, from 1 to 1500; 1500 if not given.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    frames: Option<i64>,
    /// Conditions that stop the call at their first match, written as run's until takes them. The routine's first instruction is never stopped before. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    until: Option<Value>,
    /// Conditions whose matches are gathered as the call goes, written as run's until takes them. The call stops at the end of the instruction that makes the 128th match, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    gather: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StepRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// How many instructions to run, from 1 to 100000; 1 if not given.
    #[schemars(range(min = 1, max = MOST_INSTRUCTIONS))]
    #[schemars(transform = absent_not_null)]
    instructions: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InspectRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// What to read, as a C expression names it on the machine's struct: cpu.pc, crtc.registers[12], fdc.units[0]. The whole machine if not given.
    #[schemars(transform = absent_not_null)]
    path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Each value to write, by its path: {"cpu.pc": 16384, "gate_array.inks[16]": 20}. A structure takes an object of some of its fields and an array an array of all its elements.
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PeekRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The first address to read.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// How many bytes to read, from 1 to 4096, none of them past &FFFF; 1 if not given.
    #[schemars(range(min = 1, max = MOST_BYTES))]
    #[schemars(transform = absent_not_null)]
    length: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PokeRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The first address to write.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// The bytes to write, as pairs of hexadecimal digits: 3E05C9, or 3E 05 C9. From 1 to 4096 of them, none past &FFFF.
    bytes: String,
}

#[derive(Serialize, JsonSchema)]
struct Standing {
    /// The handle the server gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
}

#[derive(Serialize, JsonSchema)]
struct Opened {
    /// The handle the server gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// Every file the machine was built from, firmware first and the snapshot last, each named by what it held.
    files: Vec<FileRead>,
}

#[derive(Serialize, JsonSchema)]
struct Listing {
    /// The machines standing, in the order they were opened.
    machines: Vec<Standing>,
}

#[derive(Serialize, JsonSchema)]
struct Closed {
    /// The handle the machine had, which names nothing now.
    handle: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Stopped {
    /// The monitor retraced as many times as was asked.
    Frames,
    /// The limit came before the frames did.
    Limit,
    /// The ticks asked for were run.
    Ticks,
    /// The instructions asked for were run.
    Instructions,
    /// An instruction did not end in the longest any instruction takes, as a chain of prefixes does not, and the machine stands inside it.
    Unsettled,
    /// A condition of until matched.
    Matched,
    /// The gathering came to 128 matches, and holds the rest of the instruction that made the 128th as well, and of the response to an interrupt taken at its end.
    Full,
    /// The routine called returned.
    Returned,
}

#[derive(Serialize, JsonSchema)]
struct Matched {
    /// The machine's tick: as an instruction is about to begin, or once the tick that put an access on the bus has run.
    tick: u64,
    /// An instruction about to begin, by its address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    execute: Option<u16>,
    /// A byte the program read as data, by its address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    read: Option<u16>,
    /// A byte the processor wrote, by its address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    write: Option<u16>,
    /// A port the processor read, by its address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    r#in: Option<u16>,
    /// A port the processor wrote, by its address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    out: Option<u16>,
    /// The byte read or written; for a port read, the byte the processor took.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    value: Option<u8>,
    /// Where the instruction that made the access begins; not given for an instruction about to begin, whose address is its own, or when the response to an interrupt made the access.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc: Option<u16>,
    /// The monitor's beam across its line, as monitor.beam_x counts it, in samples: a line's picture is monitor.width of them, and the count runs on past it until a sync ends the line.
    beam_x: u16,
    /// The monitor's beam down its frame, as monitor.beam_y counts it, in lines, of which monitor.height make a frame.
    beam_y: u16,
}

impl Matched {
    fn of(seen: &Match) -> Matched {
        let at = |kind| (seen.kind == kind).then_some(seen.address);
        let access = seen.kind != EXECUTE;
        Matched {
            tick: seen.tick,
            execute: at(EXECUTE),
            read: at(READ),
            write: at(WRITE),
            r#in: at(IN),
            out: at(OUT),
            value: access.then_some(seen.value),
            pc: (access && !seen.response).then_some(seen.pc),
            beam_x: seen.beam_x,
            beam_y: seen.beam_y,
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct Ran {
    #[serde(flatten)]
    run: Run,
    /// The ticks the machine has run since it was stood up, from power-on or from its snapshot.
    tick: u64,
    /// The program counter: where the next instruction is fetched.
    pc: u16,
    /// Why the run ended.
    stopped: Stopped,
    /// What matched the conditions of until as the run stopped: the matching accesses of the instruction it stopped after, with those of an interrupt's response taken at its end, and the instruction it then stands before, if that matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    matched: Option<Vec<Matched>>,
    /// What matched the conditions of gather, in the order it happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    gathered: Option<Vec<Matched>>,
}

impl Ran {
    fn of(run: Run, machine: &Built, stopped: Stopped) -> Ran {
        Ran {
            stopped: if run.settled {
                stopped
            } else {
                Stopped::Unsettled
            },
            run,
            tick: machine.tick(),
            pc: machine.pc(),
            matched: None,
            gathered: None,
        }
    }

    // What the run's watch saw, and why it stopped the run if it did.
    fn watched(mut self, watched: &Watched, gathering: bool) -> Ran {
        if self.run.settled {
            if watched.returned {
                self.stopped = Stopped::Returned;
            } else if watched.matched {
                self.stopped = Stopped::Matched;
            } else if watched.full {
                self.stopped = Stopped::Full;
            }
        }
        let listed = |chosen: fn(&Match) -> bool| {
            watched
                .matches
                .iter()
                .filter(|seen| chosen(seen))
                .map(Matched::of)
                .collect()
        };
        self.matched = watched.matched.then(|| listed(|seen| seen.stops));
        self.gathered = gathering.then(|| listed(|seen| seen.gathers));
        self
    }
}

#[derive(Serialize, JsonSchema)]
struct Inspected {
    /// The path read; empty for the whole machine.
    path: String,
    /// What the machine holds there.
    #[schemars(schema_with = "any_value_schema")]
    value: Value,
}

#[derive(Serialize, JsonSchema)]
struct Written {
    /// Each path written, and what it now holds.
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
}

#[derive(Serialize, JsonSchema)]
struct Peeked {
    /// The first address read.
    address: u16,
    /// The bytes, as pairs of hexadecimal digits.
    bytes: String,
}

#[derive(Serialize, JsonSchema)]
struct Poked {
    /// The first address written.
    address: u16,
    /// How many bytes were written.
    length: u16,
}

// A span of memory that ends at &FFFF or before it.
fn span(address: u16, length: i64) -> Result<u16, String> {
    let length = u16::try_from(length)
        .ok()
        .filter(|length| (1..=MOST_BYTES).contains(length))
        .ok_or_else(|| format!("{length} bytes is not from 1 to {MOST_BYTES}"))?;
    let room = 0x10000 - u32::from(address);
    if u32::from(length) > room {
        return Err(format!(
            "{length} bytes from &{address:04X} run past &FFFF, where the processor's memory \
             ends; {} fit from there",
            room.min(u32::from(MOST_BYTES))
        ));
    }
    Ok(length)
}

impl Server {
    fn thread(&self, handle: &str) -> Result<(&'static Machine, mpsc::Sender<Order>), String> {
        let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads
            .standing
            .iter()
            .find(|standing| standing.handle == handle)
            .map(|standing| (standing.machine, standing.orders.clone()))
            .ok_or_else(|| threads.unknown(handle))
    }

    // Has the machine's own thread do the work, and waits for it without
    // holding up the server, so that machines run side by side.
    async fn ask<T: Send + 'static>(
        handle: &str,
        orders: mpsc::Sender<Order>,
        work: impl FnOnce(&mut Built) -> T + Send + 'static,
    ) -> Result<T, String> {
        let (answer, answered) = oneshot::channel();
        let stopped = || {
            format!(
                "{handle} stopped; the server's error output says why, and machine_close puts it \
                 away"
            )
        };
        let sent = orders.send(Box::new(move |machine| {
            let _ = answer.send(work(machine));
        }));
        drop(orders);
        sent.map_err(|_| stopped())?;
        answered.await.map_err(|_| stopped())
    }
}

#[tool_router]
impl Server {
    pub fn new(roms: PathBuf) -> Server {
        Server {
            roms,
            threads: Mutex::default(),
        }
    }

    /// Build a machine, powered on and standing before its first instruction, or standing where a snapshot was taken, and give it a handle. It stands until machine_close puts it away or the server stops, and this server never gives its handle to another machine. The answer names every file the machine was built from by its SHA-256. A snapshot is loaded as the emulator's command line loads one, into the machine named, after its firmware is fitted. A CPC's is an SNA of version 1, 2 or 3, of which only the fields of version 1 are read: none keeps the disc controller, and the 6845's counters a version 3 carries are not read, so the machine starts its frame from the top and must not have been taken inside a disc access. A Spectrum's is the 48K .sna, which keeps no beam position either. The machine's tick starts at 0 from either.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn machine_open(
        &self,
        Parameters(request): Parameters<OpenRequest>,
    ) -> Result<Json<Opened>, String> {
        let machine = machines::named(&request.machine).ok_or_else(|| {
            let names: Vec<&str> = MACHINES.iter().map(|machine| machine.name).collect();
            format!(
                "no machine is called {}; the machines are {}",
                request.machine,
                names.join(", ")
            )
        })?;
        // Numbered in the order asked, before anything is read.
        let opened = {
            let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.opened += 1;
            threads.opened
        };
        // Read where it may block, as a pipe can, without holding up the
        // server.
        let roms = self.roms.clone();
        let parts =
            tokio::task::spawn_blocking(move || machine.parts(&roms, request.snapshot.as_deref()))
                .await
                .map_err(|error| {
                    format!(
                        "the files {} is built from could not be read: {error}; the server's \
                         error output says why",
                        machine.name
                    )
                })??;
        let files = parts.files.clone();
        let handle = format!("{}-{opened}", machine.name);
        // A CPC is built only while no other runs, and the waiting is its
        // thread's, so that the server answers what else it is asked
        // meanwhile. The other CPCs may wait with it: std's lock on Linux lets
        // no run, step or peek past a build waiting for it.
        let (built, ready) = oneshot::channel::<Result<(), String>>();
        let (orders, received) = mpsc::channel::<Order>();
        let thread = thread::Builder::new()
            .name(handle.clone())
            .spawn(move || {
                let mut standing = match parts.build(machine) {
                    Ok(standing) => standing,
                    Err(problem) => {
                        let _ = built.send(Err(problem));
                        return;
                    }
                };
                let _ = built.send(Ok(()));
                for order in received {
                    order(&mut standing);
                }
            })
            .map_err(|error| {
                format!(
                    "cannot start a thread for the machine: {error}; machine_close frees the \
                     thread of a machine standing"
                )
            })?;
        let problem = match ready.await {
            Ok(Ok(())) => None,
            Ok(Err(problem)) => Some(problem),
            Err(_) => Some(format!(
                "{} could not be built; the server's error output says why",
                machine.name
            )),
        };
        if let Some(problem) = problem {
            let _ = thread.join();
            return Err(problem);
        }
        let answer = Opened {
            handle: handle.clone(),
            machine: machine.name.to_owned(),
            files,
        };
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        let at = threads
            .standing
            .partition_point(|standing| standing.opened < opened);
        threads.standing.insert(
            at,
            MachineThread {
                opened,
                handle,
                machine,
                orders,
                thread,
            },
        );
        Ok(Json(answer))
    }

    /// The machines standing, each by its handle and its name, in the order they were opened.
    #[tool(input_schema = no_arguments(), annotations(read_only_hint = true, open_world_hint = false))]
    fn machine_list(&self, Parameters(_): Parameters<NoArguments>) -> Json<Listing> {
        let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        Json(Listing {
            machines: threads
                .standing
                .iter()
                .map(MachineThread::standing)
                .collect(),
        })
    }

    /// Put a machine away, once whatever it was asked to do before is done. Its handle names nothing afterwards.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn machine_close(
        &self,
        Parameters(request): Parameters<CloseRequest>,
    ) -> Result<Json<Closed>, String> {
        let standing = {
            let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(index) = threads
                .standing
                .iter()
                .position(|standing| standing.handle == request.handle)
            else {
                return Err(threads.unknown(&request.handle));
            };
            threads.standing.remove(index)
        };
        // A machine does its work in the order it was sent, so once this is
        // done, so is everything sent before, and its thread ends at once.
        let _ = Server::ask(&request.handle, standing.orders.clone(), |_| ()).await;
        standing.close();
        Ok(Json(Closed {
            handle: request.handle,
        }))
    }

    /// Run a machine by its frames or by its ticks; give one of the two. Nothing but this tool, step and call moves a machine, and the same requests of the same machine give the same answers. A frame is the monitor's, and ends as its retrace sends the beam to the top-left corner, so a run by frames ends with a whole picture on the monitor. A CPC's frame is whatever its 6845 is programmed to make and may never come, so a run by frames is limited to twice a standard frame for each frame asked, a standard frame being 79872 ticks on a CPC, at 4 MHz, and 69888 on a Spectrum, at 3.5 MHz. A run by ticks runs that many, as the emulator's command line runs its frames. Either then goes on to the end of the instruction standing there, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a run ends between instructions, a few ticks past what was asked, unless it ends inside a chain of prefixes. One request runs at most 1500 frames, or the ticks of 1500 standard frames; run again to go further. A request is not taken back: one a client stops waiting for runs to its end all the same, and the machine's tick in the next answer says how far. until stops a run sooner, where a condition matches, and gather lists what matches its conditions on the way; each match says its tick, what it was and where, the byte, the instruction that made it and where the monitor's beam stood.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn run(&self, Parameters(request): Parameters<RunRequest>) -> Result<Json<Ran>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let most_ticks = u64::from(MOST_FRAMES) * machine.standard_frame();
        let conditions = watched_for(request.until.as_ref(), request.gather.as_ref())?;
        let gathering = request.gather.is_some();
        let ran = match (request.frames, request.ticks) {
            (Some(frames), None) => {
                let frames = u32::try_from(frames)
                    .ok()
                    .filter(|frames| (1..=MOST_FRAMES).contains(frames))
                    .ok_or_else(|| {
                        format!(
                            "frames is {frames}, and a run takes from 1 to {MOST_FRAMES} frames; \
                             run again to go further"
                        )
                    })?;
                let limit = 2 * u64::from(frames) * machine.standard_frame();
                Server::ask(&request.handle, orders, move |built| {
                    let (run, watched) = built.run(frames, limit, &conditions, MOST_GATHERED);
                    let stopped = if run.frames >= frames {
                        Stopped::Frames
                    } else {
                        Stopped::Limit
                    };
                    Ran::of(run, built, stopped).watched(&watched, gathering)
                })
                .await?
            }
            (None, Some(ticks)) => {
                let ticks = u64::try_from(ticks)
                    .ok()
                    .filter(|ticks| (1..=most_ticks).contains(ticks))
                    .ok_or_else(|| {
                        format!(
                            "ticks is {ticks}, and a run on a {} takes from 1 to {most_ticks} \
                             ticks, the ticks of {MOST_FRAMES} standard frames; run again to go \
                             further",
                            machine.name
                        )
                    })?;
                Server::ask(&request.handle, orders, move |built| {
                    let (run, watched) = built.run(u32::MAX, ticks, &conditions, MOST_GATHERED);
                    Ran::of(run, built, Stopped::Ticks).watched(&watched, gathering)
                })
                .await?
            }
            _ => {
                return Err(
                    "a run takes frames or ticks, one of the two: {\"frames\": 50} runs to the \
                     50th retrace, {\"ticks\": 3993600} runs that many ticks"
                        .to_owned(),
                );
            }
        };
        Ok(Json(ran))
    }

    /// Call a routine of the machine's as its own CALL would: the program counter is pushed where the stack pointer stands, and the processor jumps to the address. The call ends when the processor is back at the address pushed with the stack pointer where it stood, which is where the routine's own return leaves it, so a routine that returns leaves the machine standing where it was, with its registers and memory as the routine left them, for inspect and peek to read. The values given are written first, as set writes them, so they can hand the routine its arguments, and they may move the stack pointer. Interrupts stay as they stand, and their handlers run within the call. A processor halted on HALT is woken for the routine, and halted again if the routine returns within the call. A routine that has not returned when the call stops, at its frames, at a condition of until or with its gathering full, is left running and awake; a later run carries it on and past its return, and stops there only if until asks for the address the call was made from. Refused, with the machine as it stood, inside a chain of prefixes, or where the address pushed would not read back. The answer is a run's.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn call(
        &self,
        Parameters(request): Parameters<CallRequest>,
    ) -> Result<Json<Ran>, String> {
        let address = address(&request.address)?;
        let frames = request.frames.unwrap_or(i64::from(MOST_FRAMES));
        let frames = u32::try_from(frames)
            .ok()
            .filter(|frames| (1..=MOST_FRAMES).contains(frames))
            .ok_or_else(|| {
                format!(
                    "frames is {frames}, and a call waits from 1 to {MOST_FRAMES} frames; run \
                     again to carry the routine further"
                )
            })?;
        let conditions = watched_for(request.until.as_ref(), request.gather.as_ref())?;
        let gathering = request.gather.is_some();
        let values = request.values.unwrap_or_default();
        let (machine, orders) = self.thread(&request.handle)?;
        let limit = 2 * u64::from(frames) * machine.standard_frame();
        let ran = Server::ask(&request.handle, orders, move |built| {
            let (run, watched) =
                built.call(address, &values, frames, limit, &conditions, MOST_GATHERED)?;
            let stopped = if run.frames >= frames {
                Stopped::Frames
            } else {
                Stopped::Limit
            };
            Ok::<_, String>(Ran::of(run, built, stopped).watched(&watched, gathering))
        })
        .await??;
        Ok(Json(ran))
    }

    /// Step a machine by its instructions. Each instruction is run to its end, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a step can land in an interrupt's handler. A processor halted on HALT idles on fetches that run nothing, and each of those is an instruction here. A chain of prefixes is no instruction until it ends, so a step caught in one long enough stops there, unsettled. One request runs at most 100000 instructions, and is not taken back, as a run's is not. The answer is a run's.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn step(
        &self,
        Parameters(request): Parameters<StepRequest>,
    ) -> Result<Json<Ran>, String> {
        let instructions = request.instructions.unwrap_or(1);
        let instructions = u32::try_from(instructions)
            .ok()
            .filter(|instructions| (1..=MOST_INSTRUCTIONS).contains(instructions))
            .ok_or_else(|| {
                format!(
                    "instructions is {instructions}, and a step takes from 1 to \
                     {MOST_INSTRUCTIONS} instructions; step again to go further"
                )
            })?;
        let (_, orders) = self.thread(&request.handle)?;
        let ran = Server::ask(&request.handle, orders, move |built| {
            let run = built.step(instructions);
            Ran::of(run, built, Stopped::Instructions)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Read what a machine holds, by its path: the whole machine, or the part a path names, as a C expression names it on the emulator's struct for the machine (cpc_t or spectrum_t, in its headers): cpu.pc, crtc.registers[12], gate_array.inks, fdc.units[0].head. Fields keep the names their headers give them, numbers are numbers, and a C enum's value is the name of its enumerator. Pointers are not followed; peek reads memory. Some fields are the chips' registers and counters, and some are the emulator's own machinery for stepping them a tick at a time, which no silicon holds as such: the headers say which.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn inspect(
        &self,
        Parameters(request): Parameters<InspectRequest>,
    ) -> Result<Json<Inspected>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        let path = request.path.unwrap_or_default();
        let read = path.clone();
        let value =
            Server::ask(&request.handle, orders, move |built| built.inspect(&read)).await??;
        Ok(Json(Inspected { path, value }))
    }

    /// Write what a machine holds, by the paths inspect reads. Every value is written, or none: a value of the wrong kind, or one the emulator could not run from, such as a pen past the seventeen the Gate Array has, is refused, and the machine is left as it stood. A CPC's memory map is worked out again afterwards, as it is when its own processor writes the registers behind it, and a Spectrum's ULA stood at the tick of the frame its frame_tick names, from which its beam is reckoned. The answer gives each path as it now reads. Writing the emulator's machinery, rather than the chips' registers, can leave a machine in a state no silicon could reach.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn set(
        &self,
        Parameters(request): Parameters<SetRequest>,
    ) -> Result<Json<Written>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        let values = request.values;
        let written =
            Server::ask(&request.handle, orders, move |built| built.set(&values)).await??;
        Ok(Json(Written { values: written }))
    }

    /// Read a machine's memory as its processor would, through the banking and the ROMs standing in front of the RAM: on a CPC with its lower ROM enabled, &0000 reads the ROM. From 1 to 4096 bytes, none past &FFFF.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn peek(
        &self,
        Parameters(request): Parameters<PeekRequest>,
    ) -> Result<Json<Peeked>, String> {
        let address = address(&request.address)?;
        let length = span(address, request.length.unwrap_or(1))?;
        let (_, orders) = self.thread(&request.handle)?;
        let bytes = Server::ask(&request.handle, orders, move |built| {
            built.peek(address, length)
        })
        .await?;
        Ok(Json(Peeked {
            address,
            bytes: hexadecimal(&bytes),
        }))
    }

    /// Write a machine's memory as its processor would: a write under a CPC's ROMs lands in the RAM beneath them, and one into a Spectrum's ROM lands nowhere. From 1 to 4096 bytes, none past &FFFF.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn poke(
        &self,
        Parameters(request): Parameters<PokeRequest>,
    ) -> Result<Json<Poked>, String> {
        let address = address(&request.address)?;
        let bytes = bytes(&request.bytes)?;
        let length = span(address, i64::try_from(bytes.len()).unwrap_or(i64::MAX))?;
        let (_, orders) = self.thread(&request.handle)?;
        Server::ask(&request.handle, orders, move |built| {
            built.poke(address, &bytes)
        })
        .await?;
        Ok(Json(Poked { address, length }))
    }
}

#[tool_handler(meta = identified())]
impl ServerHandler for Server {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let router = Server::tool_router();
        if !router.has_route(&request.name) {
            let tools: Vec<String> = router
                .list_all()
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect();
            return Err(ErrorData::invalid_params(
                format!(
                    "no tool is called {}; the tools are {}",
                    request.name,
                    tools.join(", ")
                ),
                None,
            ));
        }
        let call = ToolCallContext::new(self, request, context);
        Ok(match router.call(call).await? {
            CallToolResponse::Complete(mut result) => {
                result.meta.get_or_insert_default().0.extend(identified().0);
                CallToolResponse::Complete(result)
            }
            response => response,
        })
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(identity())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&REVISIONS)
    }
}
