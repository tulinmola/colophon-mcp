use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

use rmcp::handler::server::tool::{IntoCallToolResult, ToolCallContext, schema_for_output};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, MetaObject, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::keys::Key;
use crate::machine::{
    COUNT, Condition, EXECUTE, GATHER, IN, Kind, List, Match, OUT, READ, Run, UNTIL, WRITE,
};
use crate::machines::{
    self, Built, DISCS, Entry, FileRead, MACHINES, Machine, TAPES, Watched, Watching,
};
use crate::picture;
use crate::screen::{Asked, MOST_HEIGHT, MOST_RASTERS, MOST_WIDTH, Reading};

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
// A place counted takes some 30 characters, so these come to about twice what
// MOST_BYTES does.
const MOST_COUNTED: u32 = 512;
// Each is weighed against every access a run makes.
const MOST_CONDITIONS: usize = 16;
// An instruction listed takes some 50 characters, so these come to about half
// as many again as MOST_BYTES does.
const MOST_INSTRUCTIONS_LISTED: u16 = 256;
// A state holds the machine's struct, its RAM and its picture, some 450K on a
// 6128, and a disc's own copy only where the disc changed since it was last
// kept or restored, so these come to some 30MB of a machine whose discs stand
// still; a client walking a game's rooms held thirty.
const MOST_STATES: usize = 64;
// The hexadecimal digits of MOST_BYTES, as many characters as peek answers.
const MOST_JOURNAL_CHARACTERS: usize = 2 * MOST_BYTES as usize;

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
}

// The machines standing, in the order they were opened, and the states kept
// from them, each beside the handle of its machine, in the order they were
// kept; each has its number in the order of all that were ever opened or
// kept. Each state is held on its machine's thread, and named here as well,
// by that thread as it keeps it, until that thread lets it go or machine_close
// puts the machine away, so that machine_list answers without waiting behind
// a machine that runs.
#[derive(Default)]
struct Threads {
    opened: u64,
    standing: Vec<MachineThread>,
    kept: u64,
    held: Vec<(String, StateHeld)>,
}

impl Threads {
    fn listed(&self, thread: &MachineThread) -> Standing {
        Standing {
            handle: thread.handle.clone(),
            machine: thread.machine.name.to_owned(),
            states: self
                .held
                .iter()
                .filter(|(handle, _)| *handle == thread.handle)
                .map(|(_, held)| held.clone())
                .collect(),
        }
    }

    fn states_of(&self, handle: &str) -> Vec<&str> {
        self.held
            .iter()
            .filter(|(held, _)| held == handle)
            .map(|(_, held)| held.state.as_str())
            .collect()
    }

    // Whether a state of that name is held, kept from this machine, and if
    // not what would do.
    fn held_of(&self, handle: &str, state: &str) -> Result<(), String> {
        match self.held.iter().find(|(_, held)| held.state == state) {
            Some((held, _)) if held == handle => Ok(()),
            Some((held, _)) => Err(format!(
                "{state} was kept from {held}, and a state is restored into the machine it was \
                 kept from and dropped from it alone"
            )),
            None => {
                let states = self.states_of(handle);
                Err(if states.is_empty() {
                    format!(
                        "no state is called {state}; no state is held of {handle}, and \
                         state_keep keeps one"
                    )
                } else {
                    format!(
                        "no state is called {state}; the states kept from {handle} are {}",
                        states.join(", ")
                    )
                })
            }
        }
    }

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
    threads: Arc<Mutex<Threads>>,
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

pub(crate) fn values_schema(_: &mut SchemaGenerator) -> Schema {
    Schema::try_from(json!({ "type": "object", "additionalProperties": any_value() }))
        .expect("an object is a schema")
}

// An argument that may be left out is not one that may be null, or one whose
// default is null, and a type that admits null does not carry to every
// client.
pub(crate) fn absent_not_null(schema: &mut Schema) {
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

// Hexadecimal of up to so many digits, after the &, #, $ or 0x that marks it.
fn hexadecimal_number(value: &Value, most_digits: usize) -> Option<u32> {
    let digits = value.as_str().and_then(|text| {
        ["&", "#", "$", "0x", "0X"]
            .iter()
            .find_map(|prefix| text.strip_prefix(prefix))
    });
    digits
        .filter(|digits| {
            (1..=most_digits).contains(&digits.len())
                && digits.chars().all(|digit| digit.is_ascii_hexdigit())
        })
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
}

fn address(value: &Value) -> Result<u16, String> {
    if let Some(number) = value.as_u64() {
        return u16::try_from(number)
            .map_err(|_| format!("{value} is past &FFFF; {ADDRESS_FORMS}"));
    }
    hexadecimal_number(value, 4)
        .and_then(|number| u16::try_from(number).ok())
        .ok_or_else(|| format!("{value} is no address; {ADDRESS_FORMS}"))
}

const RAM_ADDRESS_FORMS: &str = "an address in the RAM is a number from 0 to 131071, or \
                                 hexadecimal from &00000 to &1FFFF, written with &, #, $ or 0x \
                                 before it";

fn ram_address_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "anyOf": [
            { "type": "integer", "minimum": 0, "maximum": 0x1FFFF },
            { "type": "string", "pattern": "^(&|#|\\$|0x|0X)[0-9A-Fa-f]{1,5}$" },
        ]
    })
}

// An address counted in the machine's own RAM, which a 6128 has 128K of.
fn ram_address(value: &Value) -> Result<u32, String> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .or_else(|| hexadecimal_number(value, 5))
        .filter(|address| *address <= 0x1FFFF)
        .ok_or_else(|| format!("{value} is no address in the RAM; {RAM_ADDRESS_FORMS}"))
}

const CONDITION_FORMS: &str = "a condition is {\"execute\": address}, {\"read\": address} or \
                               {\"write\": address}, each with \"to\" and the last address if it \
                               is a range, or {\"in\": port, \"mask\": bits} or {\"out\": port, \
                               \"mask\": bits}, the mask being the bits of the port's address that \
                               its device decodes";

// A condition, kept once in a tool's $defs, as until, gather and count all
// take lists of them.
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

fn condition(value: &Value, list: List) -> Result<Condition, String> {
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
        list,
    })
}

// The conditions of until, gather or count, added to those of the others.
fn conditions(
    field: &str,
    listed: Option<&Value>,
    list: List,
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
        let condition = condition(value, list)
            .map_err(|problem| format!("{field}[{index}]: {problem}; {CONDITION_FORMS}"))?;
        into.push(condition);
    }
    Ok(())
}

fn watched_for(
    until: Option<&Value>,
    gather: Option<&Value>,
    count: Option<&Value>,
) -> Result<Vec<Condition>, String> {
    let mut watched = Vec::new();
    conditions("until", until, UNTIL, &mut watched)?;
    conditions("gather", gather, GATHER, &mut watched)?;
    conditions("count", count, COUNT, &mut watched)?;
    Ok(watched)
}

fn watching(conditions: &[Condition]) -> Watching<'_> {
    Watching {
        conditions,
        most_gathered: MOST_GATHERED,
        most_counted: MOST_COUNTED,
    }
}

fn keys_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "array",
        "items": {
            "anyOf": [
                { "type": "string" },
                { "type": "integer", "minimum": 0, "maximum": 79 },
            ]
        },
    })
}

// The keys a request holds down, each found on the machine.
fn keys(machine: &Machine, given: Option<&Vec<Value>>) -> Result<Vec<Key>, String> {
    given
        .into_iter()
        .flatten()
        .map(|key| machine.key(key))
        .collect()
}

// A request's arguments as the journal keeps them: as they were given, save
// the handle, which names the machine whose journal it is.
fn journaled<T: Serialize>(request: &T) -> Value {
    serde_json::to_value(request).expect("a request's arguments serialize")
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

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// How many times the monitor is to retrace, from 1 to 1500.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<i64>,
    /// How many ticks to run, from 1 to the ticks of 1500 standard frames.
    #[schemars(range(min = 1))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    ticks: Option<i64>,
    /// Conditions that stop the run at their first match. {"execute": address} stops before the instruction there begins; {"read": address} stops at the end of an instruction that reads the byte there as data, and {"write": address} at the end of one that writes it; each takes "to" and a last address for a range. {"in": port, "mask": bits} and {"out": port, "mask": bits} stop at the end of an instruction that reads or writes a port whose address agrees with the port on the bits of the mask, which are the bits its device decodes: {"out": "&7F00", "mask": "&C000"} is a CPC's Gate Array, and {"out": "&FE", "mask": "&01"} a Spectrum's ULA. An instruction's own bytes are never read as data. A run that begins where an execute condition matches runs that instruction first. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<Value>,
    /// Conditions whose matches are gathered as the run goes, written as until's are. The run stops at the end of the instruction that makes the 128th match, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too; the next run gathers on from there. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// Conditions whose matches are counted as the run goes, by the place each matched: an instruction begun at an address, a byte read or written there, a port read or written, by the whole of its address, whatever bits its condition's mask decodes. Written as until's are, they count every instruction begun in a range with {"execute": "&4000", "to": "&BFFF"}. The run stops at the end of the instruction that counts at the 512th place, counting the rest of that instruction, and of the response to an interrupt taken at its end, too; the next run counts afresh from there. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
    /// Keys held down from the run's start to its end, on top of whatever the keyboard holds, which it holds again afterwards. Each is named as colophon-player names it, as the key is printed: on a CPC "SPACE", "Q", "RETURN", "COPY", "f5", "↑", or "(FIRE 2)" for joystick 0's main button, whose others are "(UP)", "(DOWN)", "(LEFT)", "(RIGHT)", "(FIRE 1)" and "(SPARE)", joystick 1 being wired to keys that carry digits and letters; on a Spectrum "CAPS", "SYM", "ENTER", "SPACE", "Q". A CPC's key may also be given by its number, 0 to 79, as the firmware numbers it, its line of the matrix times eight and its bit. An unknown key is refused with the names there are.
    #[schemars(schema_with = "keys_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<Vec<Value>>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// Where the routine begins.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// Values to write first, by their paths, as set writes them: {"cpu.a": 1, "cpu.h": 64, "cpu.l": 0}.
    #[schemars(schema_with = "values_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Map<String, Value>>,
    /// How many times the monitor may retrace before the call stops waiting for the routine to return, from 1 to 1500; 1500 if not given.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<i64>,
    /// Conditions that stop the call at their first match, written as run's until takes them. The routine's first instruction is never stopped before. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<Value>,
    /// Conditions whose matches are gathered as the call goes, written as run's until takes them. The call stops at the end of the instruction that makes the 128th match, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// Conditions whose matches are counted as the call goes, by the place each matched, as run's count counts them. The call stops at the end of the instruction that counts at the 512th place, counting the rest of that instruction, and of the response to an interrupt taken at its end, too. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// What to type: "PRINT 2+2\n" on a CPC, "p2+2\n" on a Spectrum, whose ROM reads a letter typed where the cursor shows K as a keyword.
    text: String,
}

// A CPC's two drives, as AMSDOS names them.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
enum Drive {
    /// The machine's own drive, of one head.
    A,
    /// A second drive, of two heads.
    B,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// The disc image: its path on the file system the server runs on, a relative one read from the server's working directory.
    path: PathBuf,
    /// The drive the disc goes into; A if not given.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    drive: Option<Drive>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TapeRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// The tape image: its path on the file system the server runs on, a relative one read from the server's working directory.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Where to write the snapshot: a path on the file system the server runs on that names no file yet, a relative one from the server's working directory.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KeepRequest {
    /// The handle machine_open gave the machine.
    handle: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StateRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The state, by the name state_keep gave it.
    state: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JournalRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The first entry to answer, counting from 0; 0 if not given.
    #[schemars(range(min = 0))]
    #[schemars(transform = absent_not_null)]
    from: Option<i64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StepRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// How many instructions to run, from 1 to 100000; 1 if not given.
    #[schemars(range(min = 1, max = MOST_INSTRUCTIONS))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
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

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PokeRequest {
    /// The handle machine_open gave the machine.
    #[serde(skip_serializing)]
    handle: String,
    /// The first address to write.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// The bytes to write, as pairs of hexadecimal digits: 3E05C9, or 3E 05 C9. From 1 to 4096 of them, none past &FFFF.
    bytes: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MonitorRequest {
    /// The handle machine_open gave the machine.
    handle: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Where the bytes of the picture are; video if not given.
    #[schemars(transform = absent_not_null)]
    reading: Option<Reading>,
    /// The address the first byte is read from, in the RAM: for the video reading a display start, at the start of a 16K page or up to 2K into it, and for linear and columns any address in the machine's RAM. The 6845's start address, from R12 and R13, if not given. A base given is an address and cannot carry what R12's bits 3 and 2 tell the 6845, which carry a row on past its 2K into the next 16K, as a screen of 32K does; left out, the base is read with them.
    #[schemars(schema_with = "ram_address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    base: Option<Value>,
    /// Bytes across, from 1 to 128. Twice the characters the 6845 displays across, R1, if not given. In the video reading a row of characters is this many bytes on from the last, as R1 sets it on the machine, so a width other than the machine's is no crop of its screen.
    #[schemars(range(min = 1, max = MOST_WIDTH))]
    #[schemars(transform = absent_not_null)]
    width: Option<i64>,
    /// Lines down, from 1 to 512. The rows the 6845 displays, R6, times the lines of a row, rasters, if not given.
    #[schemars(range(min = 1, max = MOST_HEIGHT))]
    #[schemars(transform = absent_not_null)]
    height: Option<i64>,
    /// Lines in a character row, from 1 to 32, read by the video reading alone. R9 and one if not given.
    #[schemars(range(min = 1, max = MOST_RASTERS))]
    #[schemars(transform = absent_not_null)]
    rasters: Option<i64>,
    /// How a byte becomes pixels: 0 for two of sixteen pens, 1 for four of four, 2 for eight of two, 3 for two of four. The mode the Gate Array is drawing in if not given.
    #[schemars(range(min = 0, max = 3))]
    #[schemars(transform = absent_not_null)]
    mode: Option<i64>,
    /// Sixteen hardware colour codes, 0 to 31, one for each pen in order, as inspect reads the first sixteen of gate_array.inks. The sixteen inks the Gate Array holds if not given.
    #[schemars(length(equal = 16), inner(range(min = 0, max = 31)))]
    #[schemars(transform = absent_not_null)]
    palette: Option<Vec<i64>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DisassembleRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Where the first instruction begins.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// How many instructions to list, from 1 to 256; 16 if not given.
    #[schemars(range(min = 1, max = MOST_INSTRUCTIONS_LISTED))]
    #[schemars(transform = absent_not_null)]
    instructions: Option<i64>,
}

#[derive(Serialize, JsonSchema)]
struct Standing {
    /// The handle the server gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The states kept from it, in the order they were kept, if any are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    states: Vec<StateHeld>,
}

#[derive(Clone, Serialize, JsonSchema)]
struct StateHeld {
    /// The state's name.
    state: String,
    /// The machine's tick as it was kept.
    tick: u64,
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
struct Inserted {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The drive the disc went into.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    drive: Option<Drive>,
    /// The machine's tick as it went in.
    tick: u64,
    #[serde(flatten)]
    file: FileRead,
}

#[derive(Serialize, JsonSchema)]
struct Saved {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The machine's tick as it was saved.
    tick: u64,
    /// Where the snapshot was written.
    path: PathBuf,
    /// The SHA-256 of what was written, in hexadecimal.
    sha256: String,
}

#[derive(Serialize, JsonSchema)]
struct Kept {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The state's name, which no other state of this server's is given.
    state: String,
    /// The machine's tick as it was kept.
    tick: u64,
}

#[derive(Serialize, JsonSchema)]
struct Restored {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The state restored.
    state: String,
    /// The machine's tick, the state's.
    tick: u64,
    /// The program counter: where the next instruction is fetched.
    pc: u16,
}

#[derive(Serialize, JsonSchema)]
struct Dropped {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The state dropped, whose name names nothing now.
    state: String,
}

#[derive(Serialize, JsonSchema)]
struct Journal {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The build the machine ran on, as the server names itself.
    build: String,
    /// Every file the machine was built from, firmware first and the snapshot last, each named by what it held.
    files: Vec<FileRead>,
    /// How many entries the journal holds.
    count: usize,
    /// The first entry answered, counting from 0.
    from: usize,
    /// The entries, in the order their requests were made.
    entries: Vec<Entry>,
    /// Where the next page begins, if the journal runs on past this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    next: Option<usize>,
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
    /// The text was typed, each key held and let go.
    Typed,
    /// An instruction did not end in the longest any instruction takes, as a chain of prefixes does not, and the machine stands inside it.
    Unsettled,
    /// A condition of until matched.
    Matched,
    /// The gathering came to 128 matches, or the count to 512 places, and holds the rest of the instruction that filled it as well, and of the response to an interrupt taken at its end.
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
struct Counted {
    /// Where instructions began.
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
    /// How many times it matched.
    count: u32,
}

impl Counted {
    fn of(&(kind, address, count): &(Kind, u16, u32)) -> Counted {
        let at = |listed| (kind == listed).then_some(address);
        Counted {
            execute: at(EXECUTE),
            read: at(READ),
            write: at(WRITE),
            r#in: at(IN),
            out: at(OUT),
            count,
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
    /// Each place that matched the conditions of count, and how many times: executes first, then reads, writes, port reads and port writes, each by address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    counted: Option<Vec<Counted>>,
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
            counted: None,
        }
    }

    // What the run's watch saw, and why it stopped the run if it did.
    fn watched(mut self, watched: &Watched, gathering: bool, counting: bool) -> Ran {
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
        self.counted = counting.then(|| watched.counts.iter().map(Counted::of).collect());
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

#[derive(Serialize, JsonSchema)]
struct Monitored {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The machine's tick as the picture was taken.
    tick: u64,
    /// The picture's width in pixels.
    width: u32,
    /// The picture's height in pixels.
    height: u32,
    /// The monitor's beam across its line, as monitor.beam_x counts it, in samples of the whole raster, of which the picture is a window.
    beam_x: u16,
    /// The monitor's beam down its frame, as monitor.beam_y counts it, in lines of the whole raster, of which the picture is a window.
    beam_y: u16,
}

#[derive(Serialize, JsonSchema)]
struct Drawn {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The machine's tick as the memory was read.
    tick: u64,
    /// Where the bytes of the picture were read.
    reading: Reading,
    /// The address the first byte was read from, in the RAM.
    base: u32,
    /// Bytes across.
    width: u32,
    /// Lines down.
    height: u32,
    /// Lines in a character row, given for the video reading alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rasters: Option<u32>,
    /// The mode the bytes were read in; the Gate Array reads a mode past 3, which only set can write, as 3, and so does this.
    mode: u8,
    /// The colour code given each pen, in order, in the five bits a colour code has.
    palette: [u8; 16],
    /// The picture's width in pixels, eight to a byte.
    picture_width: u32,
    /// The picture's height in pixels, two to a line.
    picture_height: u32,
}

#[derive(Serialize, JsonSchema)]
struct Listed {
    /// Where the instruction begins.
    address: u16,
    /// Its bytes, as pairs of hexadecimal digits.
    bytes: String,
    /// What it says.
    text: String,
}

#[derive(Serialize, JsonSchema)]
struct Disassembled {
    /// The instructions, in the order they stand in memory.
    instructions: Vec<Listed>,
}

// An answer with a picture beside its JSON, which the protocol carries as an
// image after the text: "Image Content" (2026-07-28, server/tools).
struct Pictured<T> {
    answer: T,
    png: Vec<u8>,
}

impl<T: Serialize> IntoCallToolResult for Pictured<T> {
    fn into_call_tool_result(self) -> Result<CallToolResponse, ErrorData> {
        let value = serde_json::to_value(self.answer).map_err(|error| {
            ErrorData::internal_error(format!("an answer did not serialize: {error}"), None)
        })?;
        let mut result = CallToolResult::structured(value);
        result
            .content
            .push(ContentBlock::image(picture::base64(&self.png), "image/png"));
        Ok(result.into())
    }
}

// A count given to screen, held to what it reads.
fn screen_count(name: &str, value: Option<i64>, most: u32) -> Result<Option<u32>, String> {
    value
        .map(|value| {
            u32::try_from(value)
                .ok()
                .filter(|value| (1..=most).contains(value))
                .ok_or_else(|| format!("{name} is {value}, and a screen reads from 1 to {most}"))
        })
        .transpose()
}

fn asked(request: &ScreenRequest) -> Result<Asked, String> {
    let mode = request
        .mode
        .map(|mode| {
            u8::try_from(mode)
                .ok()
                .filter(|mode| *mode <= 3)
                .ok_or_else(|| format!("mode is {mode}, and the Gate Array's modes are 0 to 3"))
        })
        .transpose()?;
    let palette = request
        .palette
        .as_ref()
        .map(|palette| {
            let codes: Option<Vec<u8>> = palette
                .iter()
                .map(|code| u8::try_from(*code).ok().filter(|code| *code <= 31))
                .collect();
            codes
                .and_then(|codes| <[u8; 16]>::try_from(codes).ok())
                .ok_or_else(|| {
                    format!(
                        "palette is {palette:?}, and a palette is sixteen hardware colour codes, \
                         one for each pen, each from 0 to 31"
                    )
                })
        })
        .transpose()?;
    Ok(Asked {
        reading: request.reading.unwrap_or(Reading::Video),
        base: request.base.as_ref().map(ram_address).transpose()?,
        width: screen_count("width", request.width, MOST_WIDTH)?,
        height: screen_count("height", request.height, MOST_HEIGHT)?,
        rasters: screen_count("rasters", request.rasters, MOST_RASTERS)?,
        mode,
        palette,
    })
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

    // Does work on files where it may block, as reading a pipe can, without
    // holding up the server; `files` names them if it cannot be done.
    async fn blocking<T: Send + 'static>(
        files: String,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        tokio::task::spawn_blocking(work).await.map_err(|error| {
            format!("{files} could not be reached: {error}; the server's error output says why")
        })?
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
            threads: Arc::default(),
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
        let roms = self.roms.clone();
        let parts = Server::blocking(
            format!("the files {} is built from", machine.name),
            move || machine.parts(&roms, request.snapshot.as_deref()),
        )
        .await?;
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

    /// The machines standing, each by its handle and its name, in the order they were opened, and the states kept from each, by name and tick, in the order they were kept.
    #[tool(input_schema = no_arguments(), annotations(read_only_hint = true, open_world_hint = false))]
    fn machine_list(&self, Parameters(_): Parameters<NoArguments>) -> Json<Listing> {
        let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        Json(Listing {
            machines: threads
                .standing
                .iter()
                .map(|thread| threads.listed(thread))
                .collect(),
        })
    }

    /// Put a machine away, once whatever it was asked to do before is done, and every state kept from it with it. Its handle names nothing afterwards, nor do the states' names.
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
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads.held.retain(|(handle, _)| *handle != request.handle);
        Ok(Json(Closed {
            handle: request.handle,
        }))
    }

    /// Run a machine by its frames or by its ticks; give one of the two. Nothing but this tool, step, call and type moves a machine, and state_restore puts one back where it stood; the same requests of the same machine give the same answers. A frame is the monitor's, and ends as its retrace sends the beam to the top-left corner, so a run by frames ends with a whole picture on the monitor. A CPC's frame is whatever its 6845 is programmed to make and may never come, so a run by frames is limited to twice a standard frame for each frame asked, a standard frame being 79872 ticks on a CPC, at 4 MHz, and 69888 on a Spectrum, at 3.5 MHz. A run by ticks runs that many, as the emulator's command line runs its frames. Either then goes on to the end of the instruction standing there, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a run ends between instructions, a few ticks past what was asked, unless it ends inside a chain of prefixes. One request runs at most 1500 frames, or the ticks of 1500 standard frames; run again to go further. A request is not taken back: one a client stops waiting for runs to its end all the same, and the machine's tick in the next answer says how far. until stops a run sooner, where a condition matches, gather lists what matches its conditions on the way, and count counts how many times each place matches its conditions; each match gathered says its tick, what it was and where, the byte, the instruction that made it and where the monitor's beam stood. keys holds keys down through the run, as a player holds a direction and fire for as long as a stretch of the game wants them.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn run(&self, Parameters(request): Parameters<RunRequest>) -> Result<Json<Ran>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let most_ticks = u64::from(MOST_FRAMES) * machine.standard_frame();
        let conditions = watched_for(
            request.until.as_ref(),
            request.gather.as_ref(),
            request.count.as_ref(),
        )?;
        let keys = keys(machine, request.keys.as_ref())?;
        let arguments = journaled(&request);
        let gathering = request.gather.is_some();
        let counting = request.count.is_some();
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
                    let tick = built.tick();
                    let (run, watched) = built.run(frames, limit, &watching(&conditions), &keys);
                    built.record(tick, "run", arguments, None);
                    let stopped = if run.frames >= frames {
                        Stopped::Frames
                    } else {
                        Stopped::Limit
                    };
                    Ran::of(run, built, stopped).watched(&watched, gathering, counting)
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
                    let tick = built.tick();
                    let (run, watched) = built.run(u32::MAX, ticks, &watching(&conditions), &keys);
                    built.record(tick, "run", arguments, None);
                    Ran::of(run, built, Stopped::Ticks).watched(&watched, gathering, counting)
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

    /// Call a routine of the machine's as its own CALL would: the program counter is pushed where the stack pointer stands, and the processor jumps to the address. The call ends when the processor is back at the address pushed with the stack pointer where it stood, which is where the routine's own return leaves it, so a routine that returns leaves the machine standing where it was, with its registers and memory as the routine left them, for inspect and peek to read. The values given are written first, as set writes them, so they can hand the routine its arguments, and they may move the stack pointer. Interrupts stay as they stand, and their handlers run within the call. A processor halted on HALT is woken for the routine, and halted again if the routine returns within the call. A routine that has not returned when the call stops, at its frames, at a condition of until or with its gathering or its count full, is left running and awake; a later run carries it on and past its return, and stops there only if until asks for the address the call was made from. Refused, with the machine as it stood, inside a chain of prefixes, or where the address pushed would not read back. The answer is a run's.
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
        let conditions = watched_for(
            request.until.as_ref(),
            request.gather.as_ref(),
            request.count.as_ref(),
        )?;
        let gathering = request.gather.is_some();
        let counting = request.count.is_some();
        let arguments = journaled(&request);
        let values = request.values.unwrap_or_default();
        let (machine, orders) = self.thread(&request.handle)?;
        let limit = 2 * u64::from(frames) * machine.standard_frame();
        let ran = Server::ask(&request.handle, orders, move |built| {
            let tick = built.tick();
            let (run, watched) =
                built.call(address, &values, frames, limit, &watching(&conditions))?;
            built.record(tick, "call", arguments, None);
            let stopped = if run.frames >= frames {
                Stopped::Frames
            } else {
                Stopped::Limit
            };
            Ok::<_, String>(Ran::of(run, built, stopped).watched(&watched, gathering, counting))
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
        let arguments = journaled(&request);
        let ran = Server::ask(&request.handle, orders, move |built| {
            let tick = built.tick();
            let run = built.step(instructions);
            built.record(tick, "step", arguments, None);
            Ran::of(run, built, Stopped::Instructions)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Type text at a machine as the emulator's command line types it with --type: each character's key held down, with the shift it needs, for 3 standard frames, then let go for 3 on a CPC and 6 on a Spectrum, long enough for each machine's firmware to see a key go down and come up, and to take the same key twice. Keys are pressed, not characters handed over, so what arrives is what the firmware makes of them: on a Spectrum a letter typed where the cursor shows K is a keyword, so "p" at the start of a line is PRINT, and "j" then two of "\"" is LOAD "". Keys the keyboard holds already, as set writes them, stay held through the typing, where the command line lets every key go. A newline is RETURN on a CPC and ENTER on a Spectrum; on a CPC a tab is TAB, an escape, \u001b, ESC, and a backspace, \b, DEL. Keys that type no character, such as COPY, the function keys and the joysticks, and a character outside ASCII, such as a CPC's £, SHIFT with ^, are held by run's keys. A text is ASCII, and one holding a character no key types, alone or with a shift, is refused before a key goes down. The firmware takes no key until it has started, and the command line waits 78 frames for a CPC's and 128 for a Spectrum's, whose ROM takes none before frame 85; a program reading the keyboard only now and then loses the keys pressed between, so type at it a character at a time, with runs between that stop where it reads the keyboard. At most 250 characters on a CPC and 166 on a Spectrum, 1500 frames' worth. Each key goes down and comes up between instructions, a few ticks at most from where the command line puts it, counting from where the typing began, and the typing ends at the end of the instruction standing as its frames' ticks run out, so the answer is a run's that took those ticks and a few more. A stretch that ends inside a chain of prefixes ends the typing there, unsettled, as it ends a run.
    #[tool(
        name = "type",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn r#type(
        &self,
        Parameters(request): Parameters<TypeRequest>,
    ) -> Result<Json<Ran>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let most = machine.most_typed(MOST_FRAMES);
        let characters = request.text.chars().count();
        if !(1..=most).contains(&characters) {
            let rest = if characters == 0 {
                "give a character or more"
            } else {
                "type the rest in another request"
            };
            return Err(format!(
                "text holds {characters} characters, and from 1 to {most} are typed at a {} at \
                 once, {MOST_FRAMES} frames' worth; {rest}",
                machine.name
            ));
        }
        let keystrokes = machine.keystrokes(&request.text)?;
        let (held, released) = machine.typing();
        let arguments = journaled(&request);
        let ran = Server::ask(&request.handle, orders, move |built| {
            let tick = built.tick();
            let run = built.r#type(&keystrokes, held, released);
            built.record(tick, "type", arguments, None);
            Ran::of(run, built, Stopped::Typed)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Put a disc in a CPC's drive: a DSK image, of the original layout or the extended one, read whole from its path on the file system the server runs on and copied into the machine, with room for every track to be formatted again, as the emulator's command line gives a disc with --disc. What the machine writes lands in its copy and never in the file, and is gone once another disc takes the copy's place, save what a state kept while it stood in the drive brings back as it stood then; nothing here writes a disc out. The disc takes the place of whatever disc the drive held, as a game's second side does when the game asks for it. A cpc664 and a cpc6128 take discs; a cpc464 is built here as the command line builds one given no disc, without the disc interface, and nothing here fits one afterwards, so its discs go to a cpc664 or a cpc6128; a Spectrum has no drive. An image the emulator cannot read is refused, with what is wrong with it, and the drive keeps the disc it had. The answer names the file by its SHA-256, with the machine's tick as the disc went in.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn disc_insert(
        &self,
        Parameters(request): Parameters<DiscRequest>,
    ) -> Result<Json<Inserted>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let arguments = journaled(&request);
        let drive = request.drive.unwrap_or(Drive::A);
        let path = request.path;
        let read = path.clone();
        let (contents, file) = Server::blocking(path.display().to_string(), move || {
            let contents = machine.disc(&read)?;
            let file = FileRead::of(&read, &contents);
            Ok((contents, file))
        })
        .await?;
        let number = match drive {
            Drive::A => 0,
            Drive::B => 1,
        };
        let given = file.clone();
        let tick = Server::ask(&request.handle, orders, move |built| {
            built.insert_disc(number, &contents)?;
            built.record(built.tick(), "disc_insert", arguments, Some(given));
            Ok::<_, String>(built.tick())
        })
        .await?
        .map_err(|problem| format!("{}: {problem}; {DISCS}", path.display()))?;
        Ok(Json(Inserted {
            handle: request.handle,
            drive: Some(drive),
            tick,
            file,
        }))
    }

    /// Put a tape in a Spectrum's deck and press PLAY: a .tap, bare blocks at the ROM's own timings, or a .tzx, read whole from its path on the file system the server runs on and copied into the machine, as the emulator's command line gives a tape with --tape. A Spectrum has no motor line, so the tape plays from its start as the machine runs, whatever the machine does: put it in just before the program asks for it, as LOAD "" does, since a header's pilot tone lasts 5 seconds and the ROM listens for only part of it. A tape stops where a block marks it to stop, as a multiload's does between its parts, and nothing here presses PLAY again, so such a tape loads its first part and waits, as it does on the command line. Blocks the emulator's reader cannot play, a waveform recorded sample by sample, data built from a table of symbols, blocks visited out of order, or a block whose ID it does not know, are refused as the tape goes in, and the deck keeps the tape it had. The tape takes the place of whatever tape the deck held. A CPC is given no tape here: the emulator has never loaded one through a CPC's firmware, so nothing could hold such a loading to what the machine did. The answer names the file by its SHA-256, with the machine's tick as the tape went in.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn tape_insert(
        &self,
        Parameters(request): Parameters<TapeRequest>,
    ) -> Result<Json<Inserted>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let arguments = journaled(&request);
        let path = request.path;
        let read = path.clone();
        let (contents, file) = Server::blocking(path.display().to_string(), move || {
            let contents = machine.tape(&read)?;
            let file = FileRead::of(&read, &contents);
            Ok((contents, file))
        })
        .await?;
        let given = file.clone();
        let tick = Server::ask(&request.handle, orders, move |built| {
            built.insert_tape(&contents)?;
            built.record(built.tick(), "tape_insert", arguments, Some(given));
            Ok::<_, String>(built.tick())
        })
        .await?
        .map_err(|problem| format!("{} {problem}; {TAPES}", path.display()))?;
        Ok(Json(Inserted {
            handle: request.handle,
            drive: None,
            tick,
            file,
        }))
    }

    /// Write a machine to a new file as a snapshot, in the format the emulator's command line writes with --save and machine_open reads: an SNA of version 1 for a CPC, the 48K .sna for a Spectrum. The file is written only where none is, at its path on the file system the server runs on, from the server's working directory if relative, so that nothing is ever written over: a path that names a file already is refused. A snapshot keeps less than the machine. Neither keeps whether the processor is halted, so a machine saved on HALT, waiting for an interrupt, runs on past the HALT as soon as it is read. A CPC's keeps no counters of the 6845's, so the machine read from it starts its frame from the top, and neither the disc controller nor the discs, so one saved inside a disc access does not go on with it. A Spectrum's keeps no beam position and no tape, and keeps the program counter in the two bytes below the stack pointer, in the file and not in the machine, so a machine read from it has lost what those two bytes held; a Spectrum whose stack pointer is below &4002, 0 among them, is refused. Refused inside an instruction, as a run or a step ending in a chain of prefixes leaves the machine. The machine is left as it stands. The answer names the file by its SHA-256, with the machine's tick.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn snapshot_save(
        &self,
        Parameters(request): Parameters<SaveRequest>,
    ) -> Result<Json<Saved>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        let handle = request.handle.clone();
        let (bytes, tick) = Server::ask(&request.handle, orders, |built| {
            Ok::<_, String>((built.snapshot_save()?, built.tick()))
        })
        .await?
        .map_err(|problem| format!("{handle} {problem}"))?;
        let path = request.path;
        let written = Server::blocking(path.display().to_string(), move || {
            machines::write_new(&path, &bytes)
        })
        .await?;
        Ok(Json(Saved {
            handle: request.handle,
            tick,
            path: written.path,
            sha256: written.sha256,
        }))
    }

    /// Keep a machine exactly as it stands, to be put back as it was with state_restore: everything it holds, inside an instruction if a run left it in a chain of prefixes, its picture, each disc as it has been written, the tape where it has played to, its tick and its journal. Nothing the machine does afterwards changes the state, and nothing of the machine is left out of it, where a snapshot keeps only what its format has room for. The answer names the state. A state is held in the server's memory, beside its machine, until state_drop lets it go or machine_close puts the machine away; the server stopping takes every state with it, and no state is written to a file, since it holds the machine's own pointers. At most 64 are held of one machine. A state is no record of how the machine came to stand there: its journal is, and a finding names the journal, which names the files and the build that make the machine again.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn state_keep(
        &self,
        Parameters(request): Parameters<KeepRequest>,
    ) -> Result<Json<Kept>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        let state = {
            let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.kept += 1;
            format!("state-{}", threads.kept)
        };
        let threads = Arc::clone(&self.threads);
        let (handle, keeping) = (request.handle.clone(), state.clone());
        let tick = Server::ask(&request.handle, orders, move |built| {
            let full = {
                let threads = threads.lock().unwrap_or_else(PoisonError::into_inner);
                let states = threads.states_of(&handle);
                (states.len() >= MOST_STATES).then(|| {
                    format!(
                        "{handle} has {MOST_STATES} states kept, the most one machine keeps; \
                         state_drop lets one of them go: {}",
                        states.join(", ")
                    )
                })
            };
            if let Some(full) = full {
                return Err(full);
            }
            built.state_keep(keeping.clone());
            let tick = built.tick();
            let mut threads = threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.held.push((
                handle,
                StateHeld {
                    state: keeping,
                    tick,
                },
            ));
            Ok(tick)
        })
        .await??;
        Ok(Json(Kept {
            handle: request.handle,
            state,
            tick,
        }))
    }

    /// Put a machine back exactly as a state kept from it stood: everything it held, its picture, the discs it held then as they were, though another disc has taken a drive since or the program has written to them, a drive empty then emptied again, and its tape where it had played to. Its tick and its journal are the state's, and what is done from there is journaled after them. A state is restored into the machine it was kept from and no other. Restoring one lets no other state go: each is a branch to come back to, and this one stays, to restore again.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn state_restore(
        &self,
        Parameters(request): Parameters<StateRequest>,
    ) -> Result<Json<Restored>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        {
            let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.held_of(&request.handle, &request.state)?;
        }
        let state = request.state.clone();
        let (tick, pc) = Server::ask(&request.handle, orders, move |built| {
            built.state_restore(&state)?;
            Ok::<_, String>((built.tick(), built.pc()))
        })
        .await??;
        Ok(Json(Restored {
            handle: request.handle,
            state: request.state,
            tick,
            pc,
        }))
    }

    /// Let a state kept from a machine go, freeing the memory it held. Its name names nothing afterwards.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn state_drop(
        &self,
        Parameters(request): Parameters<StateRequest>,
    ) -> Result<Json<Dropped>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        {
            let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.held_of(&request.handle, &request.state)?;
        }
        let threads = Arc::clone(&self.threads);
        let state = request.state.clone();
        Server::ask(&request.handle, orders, move |built| {
            built.state_drop(&state)?;
            let mut threads = threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.held.retain(|(_, held)| held.state != state);
            Ok::<_, String>(())
        })
        .await??;
        Ok(Json(Dropped {
            handle: request.handle,
            state: request.state,
        }))
    }

    /// What has been done to a machine, on the branch it stands on: the build that runs it, the files it was built from, each named by its SHA-256, and every request that stood it up or changed it, in the order made, each with the machine's tick as it began and the arguments it was given, the first of them the machine_open that stood it up, and a snapshot, a disc or a tape read named by its SHA-256 as well. Reads change nothing and are not entered, nor are states kept or let go; restoring a state makes the journal the state's. The same requests made in this order, on the same build and with the same files, make a machine that stands as this one stands, which is how a finding is made again. Entries are answered from the one asked for, 0 if none is, as many as fit in 8192 characters, and at least one while any remain, however long; next says where the following page begins.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn journal(
        &self,
        Parameters(request): Parameters<JournalRequest>,
    ) -> Result<Json<Journal>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let from = request.from.unwrap_or(0);
        let handle = request.handle.clone();
        let (files, count, from, entries) = Server::ask(&request.handle, orders, move |built| {
            let count = built.journal_length();
            let from = usize::try_from(from)
                .ok()
                .filter(|from| *from <= count)
                .ok_or_else(|| {
                    format!(
                        "from is {from}, and the journal of {handle} holds {count} entries, so it \
                         is answered from 0 to {count}"
                    )
                })?;
            let mut characters = 0;
            let mut entries = Vec::new();
            for entry in built.journal().skip(from) {
                characters += serde_json::to_string(entry)
                    .expect("an entry serializes")
                    .chars()
                    .count();
                if characters > MOST_JOURNAL_CHARACTERS && !entries.is_empty() {
                    break;
                }
                entries.push(entry.clone());
            }
            Ok::<_, String>((built.files().to_vec(), count, from, entries))
        })
        .await??;
        let next = Some(from + entries.len()).filter(|next| *next < count);
        Ok(Json(Journal {
            handle: request.handle,
            machine: machine.name.to_owned(),
            build: env!("COLOPHON_MCP_BUILD").to_owned(),
            files,
            count,
            from,
            entries,
            next,
        }))
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
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn set(
        &self,
        Parameters(request): Parameters<SetRequest>,
    ) -> Result<Json<Written>, String> {
        let (_, orders) = self.thread(&request.handle)?;
        let arguments = journaled(&request);
        let values = request.values;
        let written = Server::ask(&request.handle, orders, move |built| {
            let written = built.set(&values)?;
            built.record(built.tick(), "set", arguments, None);
            Ok::<_, String>(written)
        })
        .await??;
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

    /// Show what a machine's monitor shows, as a PNG beside the answer: its picture as the emulator's command line crops and draws its screenshots, which a picture written there with --screenshot matches pixel for pixel. A CPC's is 768 by 544 pixels, a sample of the raster to each across and every line drawn twice, cut from samples 208 to 975 of the 1024 a line has and lines 34 to 305 of 312; a Spectrum's is 352 by 264, from samples 96 to 447 of 448 and lines 20 to 283 of 312. Each pixel's index in the PNG's palette is what the machine put on the cable: a CPC's hardware colour code, 0 to 31, or a Spectrum's sample, 0 to 15, its bits brightness, green, red and blue from the highest. The beam paints the picture as the machine runs, so it is whole after a run by frames that ran all its frames, which ends as the retrace sends the beam to the top-left corner; after any other run, a step, a call or typing, the lines above the beam, and its own line to its left, are this frame's, and the rest are the frame's before. A machine not yet run shows a picture never painted.
    #[tool(
        output_schema = schema_for_output::<Monitored>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn monitor(
        &self,
        Parameters(request): Parameters<MonitorRequest>,
    ) -> Result<Pictured<Monitored>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let (shown, tick) = Server::ask(&request.handle, orders, |built| {
            (built.monitor(), built.tick())
        })
        .await?;
        let answer = Monitored {
            handle: request.handle,
            machine: machine.name.to_owned(),
            tick,
            width: shown.picture.width,
            height: shown.picture.height,
            beam_x: shown.beam_x,
            beam_y: shown.beam_y,
        };
        Ok(Pictured {
            answer,
            png: shown.picture.png(),
        })
    }

    /// Read a CPC's memory as a picture, as colophon-player's <colophon-cpc-screen> reads it, so that a sprite sheet, a font or a buffer built off the screen shows its shape when read in the layout it was written in: as the video hardware reads the screen, as a flat buffer, or in columns. The bytes are read from the RAM as the video hardware reads them, whatever ROM the processor sees over them: the video reading reads the base 64K, and linear and columns all of the machine's RAM, a 6128's second 64K at &10000 to &1FFFF. Whatever is not given is read from the machine as it stands: the base from R12 and R13, the width from R1, the height from R6 and the rasters, the rasters from R9, and the mode in force and the sixteen inks from the Gate Array, so that a screen asked nothing draws the memory the 6845 is displaying. The PNG beside the answer has eight pixels across to a byte and two down to a line, the scale at which monitor draws a CPC's raster; each pixel's index in its palette is the pen, and the palette is the colours of the pens' colour codes. It is drawn from memory as it stands, in one mode and one palette, so it is not what the monitor showed where a program changes either partway down a frame, or changes the memory as the beam passes; monitor shows that. Refused on a Spectrum, whose picture monitor shows.
    #[tool(
        output_schema = schema_for_output::<Drawn>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn screen(
        &self,
        Parameters(request): Parameters<ScreenRequest>,
    ) -> Result<Pictured<Drawn>, String> {
        let asked = asked(&request)?;
        let (machine, orders) = self.thread(&request.handle)?;
        let (screen, picture, tick) = Server::ask(&request.handle, orders, move |built| {
            let (screen, picture) = built.screen(&asked)?;
            Ok::<_, String>((screen, picture, built.tick()))
        })
        .await??;
        let answer = Drawn {
            handle: request.handle,
            machine: machine.name.to_owned(),
            tick,
            reading: screen.reading,
            base: screen.base,
            width: screen.width,
            height: screen.height,
            rasters: (screen.reading == Reading::Video).then_some(screen.rasters),
            mode: screen.mode,
            palette: screen.palette,
            picture_width: picture.width,
            picture_height: picture.height,
        };
        Ok(Pictured {
            answer,
            png: picture.png(),
        })
    }

    /// Read a machine's memory as the instructions its processor would run, from an address on, each with its address, its bytes and what they say: Zilog's mnemonics, numbers in hexadecimal after &, and a relative jump's target worked out. Memory is read as peek reads it, through the banking and the ROMs, and an instruction that runs past &FFFF goes on at &0000, as the program counter does. An instruction is what step runs, so a chain of DD and FD prefixes is one, ruled by its last prefix, even one so long that a step stops inside it. Undocumented instructions are named as colophon-player's disassembly names them: IXH and IXL, SLL, RLC (IX+&05),B and its like, which copy their result into a register, IN (C), OUT (C),0, IM 0/1, and NOP for ED followed by a byte that begins no instruction, save ED FF, which is BRK, after WinAPE, whose mark it is. Nothing here tells code from data: bytes are read as instructions from wherever the listing begins. From 1 to 256 instructions, 16 if not given; a listing reads at most 4096 bytes, so only a chain of prefixes thousands long cuts it short.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn disassemble(
        &self,
        Parameters(request): Parameters<DisassembleRequest>,
    ) -> Result<Json<Disassembled>, String> {
        let address = address(&request.address)?;
        let instructions = request.instructions.unwrap_or(16);
        let instructions = u16::try_from(instructions)
            .ok()
            .filter(|instructions| (1..=MOST_INSTRUCTIONS_LISTED).contains(instructions))
            .ok_or_else(|| {
                format!(
                    "instructions is {instructions}, and a listing takes from 1 to \
                     {MOST_INSTRUCTIONS_LISTED}; disassemble again from where it ends to go \
                     further"
                )
            })?;
        let (_, orders) = self.thread(&request.handle)?;
        let listed = Server::ask(&request.handle, orders, move |built| {
            built.disassemble(address, instructions, usize::from(MOST_BYTES))
        })
        .await??;
        Ok(Json(Disassembled {
            instructions: listed
                .into_iter()
                .map(|(address, bytes, text)| Listed {
                    address,
                    bytes: hexadecimal(&bytes),
                    text,
                })
                .collect(),
        }))
    }

    /// Write a machine's memory as its processor would: a write under a CPC's ROMs lands in the RAM beneath them, and one into a Spectrum's ROM lands nowhere. From 1 to 4096 bytes, none past &FFFF.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
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
        let arguments = journaled(&request);
        Server::ask(&request.handle, orders, move |built| {
            built.poke(address, &bytes);
            built.record(built.tick(), "poke", arguments, None);
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
