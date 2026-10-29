use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

use rmcp::handler::server::common::FromContextPart;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::{IntoCallToolResult, ToolCallContext, schema_for_output};
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, MetaObject, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::transform::{RecursiveTransform, Transform};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use crate::emulator::z80_t;
use crate::fields::{self, Fields, Segment, hexadecimal_number};
use crate::keys::Key;
use crate::machine::{
    ANY_MEMORY, CAPTURED_BYTES, COUNT, CRTC, Capture, Condition, EXECUTE, FIRST_ROM_LOCATION,
    GATHER, IN, Kind, List, Match, Memory, OUT, PSG, Place, RAM, READ, ROM, ROM_BYTES, Run,
    START_ADDRESS, START_PC, START_SP, Span, Start, UNTIL, WRITE,
};
use crate::machines::{
    self, Built, DISCS, Entry, FileRead, MACHINES, Machine, NewFile, TAPES, Watched, Watching,
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
// A match that captures as much as a request may name, MOST_CAPTURED spans
// holding CAPTURED_BYTES, takes some 750 characters, so these come to some
// 24000, and with the ROOM_PAST_FULL kept past the fill and until's matches
// listed again in matched, to some 37500 at the most: past the 10000 tokens at
// which Claude Code warns, and under the 50000 characters past which it saves
// an answer to a file, https://code.claude.com/docs/en/mcp. A match capturing
// a few registers and bytes takes some 200.
const MOST_GATHERED_CAPTURING: u32 = 32;
// A match written to a file takes a line of some 100 characters, or 750 where
// it captures all a request may name, so these come to some 100MB, or 750MB,
// and the room a run keeps for them to some 56MB, and some 140MB more for what
// they capture, taken up only where a match captures.
const MOST_GATHERED_TO_A_FILE: u32 = 1 << 20;
// A place counted takes some 60 characters, so these come to about twice what
// MOST_BYTES does.
const MOST_COUNTED: u32 = 256;
// Each is weighed against every access a run makes.
const MOST_CONDITIONS: usize = 16;
// The values and the spans of memory a request's captures name together,
// which hold a match that captures to some 750 characters.
const MOST_CAPTURED: usize = 16;
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
// An address found takes some 8 characters, so these come to about a quarter
// of what MOST_BYTES does.
const MOST_FOUND: usize = 256;
// A search of all 64K for this many bytes, matched at every address, took
// some 14 milliseconds in a release build where this was written, and some
// 420 in a debug one.
const MOST_SOUGHT: usize = 256;

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
                "{state} was kept from {held}; restore, save or drop it through that machine's \
                 handle"
            )),
            None => {
                let states = self.states_of(handle);
                Err(if states.is_empty() {
                    format!(
                        "no state is named {state}; there are no states kept from {handle}; \
                         state_keep keeps one"
                    )
                } else {
                    format!(
                        "no state is named {state}; states kept from {handle}: {}",
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
                "no machine has the handle {handle}; no machine is open, and machine_open opens one"
            )
        } else {
            format!(
                "no machine has the handle {handle}; open handles: {}",
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

// A tool's arguments, taken as rmcp's own Parameters takes them, save that
// one of the wrong kind is named by its path, which serde leaves out of
// "invalid type: sequence, expected a boolean". rmcp's tool macro takes a
// tool's schema from the argument whose type has this name, and its router
// answers an invalid_params error beginning "failed to deserialize
// parameters:" as the tool's refusal.
struct Parameters<T>(T);

impl<T: JsonSchema> JsonSchema for Parameters<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        T::json_schema(generator)
    }
}

impl<S, T: DeserializeOwned> FromContextPart<ToolCallContext<'_, S>> for Parameters<T> {
    fn from_context_part(context: &mut ToolCallContext<S>) -> Result<Parameters<T>, ErrorData> {
        let arguments = Value::Object(context.arguments.take().unwrap_or_default());
        serde_path_to_error::deserialize(arguments)
            .map(Parameters)
            .map_err(|error| {
                let path = error.path().to_string();
                let problem = error.into_inner();
                let named = if path == "." {
                    problem.to_string()
                } else {
                    format!("{path}: {problem}")
                };
                ErrorData::invalid_params(
                    format!("failed to deserialize parameters: {named}"),
                    None,
                )
            })
    }
}

// The tools, their schemas without the formats schemars gives each integer
// by its Rust type, uint64, int64 and the rest. Every format JSON Schema
// defines is of strings, so these are custom formats, and "Save for
// agreement between parties, schema authors SHALL NOT expect a peer
// implementation to support such custom format attributes" (JSON Schema
// Validation 2020-12, 7.1 and 7.2.3,
// https://json-schema.org/draft/2020-12/json-schema-validation).
fn router_without_formats() -> ToolRouter<Server> {
    let formatless = |schema: &JsonObject| {
        let mut schema = Schema::from(schema.clone());
        RecursiveTransform(|schema: &mut Schema| {
            schema.remove("format");
        })
        .transform(&mut schema);
        match Value::from(schema) {
            Value::Object(object) => Arc::new(object),
            _ => unreachable!("an object's schema stays an object"),
        }
    };
    let mut router = Server::tool_router();
    for route in router.map.values_mut() {
        let tool = &mut route.attr;
        tool.input_schema = formatless(&tool.input_schema);
        tool.output_schema = tool.output_schema.as_deref().map(formatless);
    }
    router
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
    // schemars makes an optional structure or enum, which it names in its
    // $defs, one of it or null.
    let null = json!({ "type": "null" });
    if let Some(Value::Array(branches)) = schema.get_mut("anyOf") {
        branches.retain(|branch| *branch != null);
        if let [Value::Object(only)] = branches.as_slice() {
            let only = only.clone();
            schema.remove("anyOf");
            for (keyword, value) in only {
                if schema.get(&keyword).is_none() {
                    schema.insert(keyword, value);
                }
            }
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
    hexadecimal_number(value, 4)
        .and_then(|number| u16::try_from(number).ok())
        .ok_or_else(|| format!("{value} is not an address; {ADDRESS_FORMS}"))
}

// The processor's registers a read may start where they point, by inspect's
// paths.
const REGISTERS: [(&str, Start); 2] = [("cpu.sp", START_SP), ("cpu.pc", START_PC)];

const REGISTER_FORMS: &str = "\"cpu.sp\" or \"cpu.pc\" stands for the address the register holds";

// An address, or a register that holds one, kept once in a tool's $defs and
// named from wherever it is taken.
struct AddressOrRegisterForms;

impl JsonSchema for AddressOrRegisterForms {
    fn schema_name() -> Cow<'static, str> {
        "AddressOrRegister".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let mut forms = address_value();
        let names: Vec<&str> = REGISTERS.iter().map(|(name, _)| *name).collect();
        forms["anyOf"]
            .as_array_mut()
            .expect("the forms of an address")
            .push(json!({ "enum": names }));
        Schema::try_from(forms).expect("an object is a schema")
    }
}

fn address_or_register_schema(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<AddressOrRegisterForms>()
}

fn start(value: &Value) -> Result<(Start, u16), String> {
    match REGISTERS
        .iter()
        .find(|(name, _)| value.as_str() == Some(*name))
    {
        Some((_, start)) => Ok((*start, 0)),
        None => address(value)
            .map(|address| (START_ADDRESS, address))
            .map_err(|problem| format!("{problem}; {REGISTER_FORMS}")),
    }
}

fn start_written(start: Start, address: u16) -> Value {
    REGISTERS
        .iter()
        .find(|(_, register)| *register == start)
        .map_or_else(
            || json!(Hexadecimal::address(address)),
            |(name, _)| json!(name),
        )
}

const RAM_ADDRESS_FORMS: &str = "an address in the RAM is a number from 0 to 131071, or \
                                 hexadecimal from &0000 to &1FFFF, written with &, #, $ or 0x \
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
        .or_else(|| hexadecimal_number(value, 5))
        .and_then(|number| u32::try_from(number).ok())
        .filter(|address| *address <= 0x1FFFF)
        .ok_or_else(|| format!("{value} is not an address in the RAM; {RAM_ADDRESS_FORMS}"))
}

const BYTE_FORMS: &str = "a byte is a number from 0 to 255, or hexadecimal from &00 to &FF, \
                          written with &, #, $ or 0x before it";

fn byte_value() -> Value {
    json!({
        "anyOf": [
            { "type": "integer", "minimum": 0, "maximum": 255 },
            { "type": "string", "pattern": "^(&|#|\\$|0x|0X)[0-9A-Fa-f]{1,2}$" },
        ]
    })
}

// A byte, kept once in a tool's $defs and named from wherever it is taken.
struct ByteForms;

impl JsonSchema for ByteForms {
    fn schema_name() -> Cow<'static, str> {
        "Byte".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        Schema::try_from(byte_value()).expect("an object is a schema")
    }
}

fn byte(value: &Value) -> Result<u8, String> {
    if let Some(number) = value.as_u64() {
        return u8::try_from(number).map_err(|_| format!("{value} is past &FF; {BYTE_FORMS}"));
    }
    hexadecimal_number(value, 2)
        .and_then(|number| u8::try_from(number).ok())
        .ok_or_else(|| format!("{value} is not a byte; {BYTE_FORMS}"))
}

const CONDITION_FORMS: &str = "a condition is one of {\"execute\": address}, {\"read\": address}, \
                               {\"write\": address}, {\"in\": port, \"mask\": bits}, {\"out\": \
                               port, \"mask\": bits}, or on a CPC {\"psg\": register} or \
                               {\"crtc\": register}; port and mask are addresses, mask's set bits \
                               being those the port's device decodes; all but in and out may add \
                               \"to\", the last address or register of a range; execute and read \
                               may add \"memory\": \"ram\" or \"rom\"; all but execute and in may \
                               add \"value\", a byte, and with it \"value_to\", the last byte of a \
                               range; write, psg and crtc may add \"changes\": true; in until and \
                               gather, any may add \"capture\"";

const CAPTURE_FORMS: &str = "capture is {\"values\": paths, \"memory\": spans}, with at least \
                             one of the two: a path names one of the processor's values under \
                             cpu, such as \"cpu.a\", \"cpu.h\" or \"cpu.ixh\", and a span is \
                             {\"address\": address, \"length\": bytes}, 1 to 64 bytes, 1 if \
                             length is not given, none past &FFFF; the address may be \
                             \"cpu.sp\" or \"cpu.pc\", where the register points at the \
                             capture, and the span then wraps past &FFFF to &0000";

const PSG_REGISTER_FORMS: &str = "a PSG register is a number from 0 to 15";

const CRTC_REGISTER_FORMS: &str = "a 6845 register is a number from 0 to 31, of which the chip has \
                                   0 to 17";

// "The PSG has 16 internal registers" ("AY-3-8912 PSG", Kevin Thacker's
// cpctech,
// https://web.archive.org/web/20220626135348/https://cpctech.cpcwiki.de/docs/psg.html).
const MOST_PSG_REGISTER: u16 = 15;

// The 6845's address register "is a 5-bit register used to select 18
// internal control registers (R0--R17) [...] Programming the data from 18 to
// 31 produces no results" (Hitachi's HD6845S datasheet, as cpctech transcribes
// it,
// https://web.archive.org/web/20220626023757id_/https://cpctech.cpcwiki.de/docs/hd6845s/hd6845sp.htm).
const MOST_CRTC_REGISTER: u16 = 31;

fn register(value: &Value, most: u16, forms: &str) -> Result<u16, String> {
    value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .filter(|number| *number <= most)
        .ok_or_else(|| format!("{value} is not a register; {forms}"))
}

fn psg_register(value: &Value) -> Result<u16, String> {
    register(value, MOST_PSG_REGISTER, PSG_REGISTER_FORMS)
}

fn crtc_register(value: &Value) -> Result<u16, String> {
    register(value, MOST_CRTC_REGISTER, CRTC_REGISTER_FORMS)
}

// What a condition's matches capture: the processor's values, each by its
// path, kept as given and as it reaches into z80_t, and spans of memory.
struct Capturing {
    values: Vec<(String, Vec<Segment>)>,
    memory: Vec<Span>,
}

fn same_span(one: &Span, other: &Span) -> bool {
    (one.start, one.address, one.length) == (other.start, other.address, other.length)
}

// A value of the processor's, by inspect's path, which reaches no structure
// or array but one value.
fn captured_value(given: &Value) -> Result<(String, Vec<Segment>), String> {
    let path = given
        .as_str()
        .ok_or_else(|| format!("values holds {given}, which is not a path"))?;
    let mut segments = fields::parse(path)?;
    if segments.first() != Some(&Segment::Field("cpu".to_owned())) {
        return Err(format!("{path} is not under cpu"));
    }
    segments.remove(0);
    // SAFETY: bindgen renders the processor as integers, bools, and arrays and
    // structures of them, and all-zero bytes are a value of each; it is only
    // read by path.
    let processor: z80_t = unsafe { std::mem::zeroed() };
    match processor.get(&segments) {
        Ok(Value::Object(_) | Value::Array(_)) => Err(format!(
            "{path} holds more than one value; capture each with a path of its own"
        )),
        Ok(_) => Ok((path.to_owned(), segments)),
        Err(problem) => Err(format!("{path}: {problem}")),
    }
}

fn captured_span(given: &Value) -> Result<Span, String> {
    let fields = given
        .as_object()
        .ok_or_else(|| format!("memory holds {given}, which is not a span"))?;
    if let Some(unknown) = fields
        .keys()
        .find(|key| *key != "address" && *key != "length")
    {
        return Err(format!(
            "{given} has {unknown}; a span takes only address and length"
        ));
    }
    let first = fields
        .get("address")
        .ok_or_else(|| format!("{given} has no address"))?;
    let (start, first) = start(first).map_err(|problem| format!("address {problem}"))?;
    let length = fields
        .get("length")
        .map_or(Some(1), Value::as_i64)
        .filter(|length| (1..=i64::from(CAPTURED_BYTES)).contains(length))
        .ok_or_else(|| {
            format!("{given} has a length that is not an integer from 1 to {CAPTURED_BYTES}")
        })?;
    let length = match start {
        START_ADDRESS => span(first, length)?,
        _ => u16::try_from(length).expect("a length of 1 to CAPTURED_BYTES"),
    };
    Ok(Span {
        start,
        address: first,
        length,
    })
}

fn capturing(given: &Value) -> Result<Capturing, String> {
    let fields = given
        .as_object()
        .ok_or_else(|| format!("{given} is not an object"))?;
    if let Some(unknown) = fields
        .keys()
        .find(|key| *key != "values" && *key != "memory")
    {
        return Err(format!(
            "{given} has {unknown}; capture takes only values and memory"
        ));
    }
    let list = |name: &str| {
        let given = fields.get(name).map_or(Ok(&[][..]), |given| {
            given
                .as_array()
                .map(Vec::as_slice)
                .ok_or_else(|| format!("{name} {given} is not a list"))
        })?;
        if given.len() > MOST_CAPTURED {
            return Err(format!(
                "{name} names {} things to capture; give at most {MOST_CAPTURED}",
                given.len()
            ));
        }
        Ok(given)
    };
    let values = list("values")?
        .iter()
        .map(captured_value)
        .collect::<Result<Vec<_>, _>>()?;
    let memory = list("memory")?
        .iter()
        .map(captured_span)
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() && memory.is_empty() {
        return Err(format!("{given} captures nothing; give values or memory"));
    }
    Ok(Capturing { values, memory })
}

// A capture, kept once in a tool's $defs and named from each condition that
// takes one.
struct CaptureForms;

impl JsonSchema for CaptureForms {
    fn schema_name() -> Cow<'static, str> {
        "Capture".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let span = json!({
            "type": "object",
            "properties": {
                "address": generator.subschema_for::<AddressOrRegisterForms>(),
                "length": { "type": "integer", "minimum": 1, "maximum": CAPTURED_BYTES },
            },
            "required": ["address"],
            "additionalProperties": false,
        });
        let named =
            |list: &str| json!({ "properties": { list: { "minItems": 1 } }, "required": [list] });
        Schema::try_from(json!({
            "type": "object",
            "properties": {
                "values": { "type": "array", "maxItems": MOST_CAPTURED, "items": { "type": "string" } },
                "memory": { "type": "array", "maxItems": MOST_CAPTURED, "items": span },
            },
            "additionalProperties": false,
            "anyOf": [named("values"), named("memory")],
        }))
        .expect("an object is a schema")
    }
}

// A condition, kept once in a tool's $defs, as until, gather and count all
// take lists of them; count's take no capture.
struct ConditionForms<const CAPTURING: bool>;

impl<const CAPTURING: bool> JsonSchema for ConditionForms<CAPTURING> {
    fn schema_name() -> Cow<'static, str> {
        if CAPTURING {
            "Condition".into()
        } else {
            "CountCondition".into()
        }
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let address = generator.subschema_for::<AddressForms>().to_value();
        let byte = generator.subschema_for::<ByteForms>().to_value();
        let register = |most: u16| json!({ "type": "integer", "minimum": 0, "maximum": most });
        let capture = CAPTURING.then(|| generator.subschema_for::<CaptureForms>().to_value());
        let condition = |kind: &str, other: &str, schema: &Value, required: &[&str]| {
            let mut branch = json!({
                "type": "object",
                "properties": { kind: schema, other: schema },
                "required": required,
                "additionalProperties": false,
            });
            if let Some(capture) = &capture {
                branch["properties"]["capture"] = capture.clone();
            }
            branch
        };
        let with_memory = |mut branch: Value| {
            let names: Vec<&str> = MEMORY_NAMES.iter().map(|(name, _)| *name).collect();
            branch["properties"]["memory"] = json!({ "enum": names });
            branch
        };
        let with_value = |mut branch: Value| {
            branch["properties"]["value"] = byte.clone();
            branch["properties"]["value_to"] = byte.clone();
            branch
        };
        let with_changes = |mut branch: Value| {
            branch["properties"]["changes"] = json!({ "type": "boolean" });
            branch
        };
        let branches = [
            with_memory(condition("execute", "to", &address, &["execute"])),
            with_value(with_memory(condition("read", "to", &address, &["read"]))),
            with_changes(with_value(condition("write", "to", &address, &["write"]))),
            condition("in", "mask", &address, &["in", "mask"]),
            with_value(condition("out", "mask", &address, &["out", "mask"])),
            with_changes(with_value(condition(
                "psg",
                "to",
                &register(MOST_PSG_REGISTER),
                &["psg"],
            ))),
            with_changes(with_value(condition(
                "crtc",
                "to",
                &register(MOST_CRTC_REGISTER),
                &["crtc"],
            ))),
        ];
        Schema::try_from(json!({ "oneOf": branches })).expect("an object is a schema")
    }
}

fn conditions_schema(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "array",
        "maxItems": MOST_CONDITIONS,
        "items": generator.subschema_for::<ConditionForms<true>>(),
    })
}

fn count_conditions_schema(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "array",
        "maxItems": MOST_CONDITIONS,
        "items": generator.subschema_for::<ConditionForms<false>>(),
    })
}

const MEMORY_NAMES: [(&str, Memory); 2] = [("ram", RAM), ("rom", ROM)];

const KIND_NAMES: [(&str, Kind); 7] = [
    ("execute", EXECUTE),
    ("read", READ),
    ("write", WRITE),
    ("in", IN),
    ("out", OUT),
    ("psg", PSG),
    ("crtc", CRTC),
];

// The kinds whose matches carry a byte a value tests: all but an instruction,
// which carries none, and a port read, which a CPC's Gate Array holds over
// several ticks, the processor taking the byte the last of them leaves on the
// bus (colophon-emulator's cpc_tick and z80.c's IO_READ_T4, at 7a23101), where
// a condition is decided at the first; a Spectrum's is refused alike, so that
// a condition means the same on either machine.
fn takes_value(kind: Kind) -> bool {
    kind != EXECUTE && kind != IN
}

// The kinds that write a byte that is then held, which changes compares.
fn takes_changes(kind: Kind) -> bool {
    kind == WRITE || kind == PSG || kind == CRTC
}

fn written_as(kind: Kind, number: u16) -> Value {
    if kind == PSG || kind == CRTC {
        json!(number)
    } else {
        json!(Hexadecimal::address(number))
    }
}

fn condition(value: &Value, list: List) -> Result<(Condition, Option<Capturing>), String> {
    let fields = value
        .as_object()
        .ok_or_else(|| format!("{value} is not an object"))?;
    let named: Vec<_> = KIND_NAMES
        .iter()
        .filter(|(name, _)| fields.contains_key(*name))
        .collect();
    let &[&(name, kind)] = named.as_slice() else {
        let names: Vec<&str> = named.iter().map(|(name, _)| *name).collect();
        return Err(match names.split_last() {
            None => format!("{value} has no kind"),
            Some((last, rest)) => format!(
                "{value} has more than one kind, {} and {last}; give one",
                rest.join(", ")
            ),
        });
    };
    let parse: fn(&Value) -> Result<u16, String> = match kind {
        PSG => psg_register,
        CRTC => crtc_register,
        _ => address,
    };
    let first = parse(&fields[name]).map_err(|problem| format!("{name} {problem}"))?;
    let port = kind == IN || kind == OUT;
    let other = if port { "mask" } else { "to" };
    let takes_memory = kind == EXECUTE || kind == READ;
    if let Some(unknown) = fields.keys().find(|key| {
        *key != name
            && *key != other
            && *key != "capture"
            && !(takes_memory && *key == "memory")
            && !(takes_value(kind) && (*key == "value" || *key == "value_to"))
            && !(takes_changes(kind) && *key == "changes")
    }) {
        return Err(format!(
            "{value} has {unknown}, which {name} conditions do not take"
        ));
    }
    let first_value = fields
        .get("value")
        .map(|given| byte(given).map_err(|problem| format!("value {problem}")))
        .transpose()?;
    let last_value = fields
        .get("value_to")
        .map(|given| byte(given).map_err(|problem| format!("value_to {problem}")))
        .transpose()?;
    let (first_value, last_value) = match (first_value, last_value) {
        (None, None) => (0, 0xFF),
        (None, Some(_)) => {
            return Err(format!(
                "{value} has value_to without value; give value too, or leave value_to out"
            ));
        }
        (Some(first), last) => (first, last.unwrap_or(first)),
    };
    if last_value < first_value {
        return Err(format!(
            "{value} runs from value {} down to {}; a range must run upward",
            Hexadecimal::byte(first_value),
            Hexadecimal::byte(last_value)
        ));
    }
    let changes = fields
        .get("changes")
        .map(|given| {
            given
                .as_bool()
                .ok_or_else(|| format!("changes is {given}; give true or false"))
        })
        .transpose()?
        .unwrap_or(false);
    let capture = fields
        .get("capture")
        .map(|given| {
            if list == COUNT {
                return Err(
                    "count conditions take no capture; capture in until or gather".to_owned(),
                );
            }
            capturing(given).map_err(|problem| format!("capture: {problem}; {CAPTURE_FORMS}"))
        })
        .transpose()?;
    let memory = fields
        .get("memory")
        .map(|given| {
            MEMORY_NAMES
                .iter()
                .find(|(name, _)| given.as_str() == Some(name))
                .map(|(_, memory)| *memory)
                .ok_or_else(|| format!("memory is {given}; give \"ram\" or \"rom\""))
        })
        .transpose()?
        .unwrap_or(ANY_MEMORY);
    let (last, mask) = if port {
        let mask = fields
            .get("mask")
            .ok_or_else(|| format!("{value} has no mask"))?;
        let mask = address(mask).map_err(|problem| format!("mask {problem}"))?;
        (first, mask)
    } else {
        let last = fields
            .get("to")
            .map(|to| parse(to).map_err(|problem| format!("to {problem}")))
            .transpose()?
            .unwrap_or(first);
        if last < first {
            let shown = |at: u16| match written_as(kind, at) {
                Value::String(text) => text,
                number => number.to_string(),
            };
            return Err(format!(
                "{value} runs from {} down to {}; a range must run upward",
                shown(first),
                shown(last)
            ));
        }
        (last, 0)
    };
    let condition = Condition {
        kind,
        first,
        last,
        mask,
        first_value,
        last_value,
        changes,
        memory,
        list,
        captures: capture.is_some(),
    };
    Ok((condition, capture))
}

// The conditions of until, gather or count, added to those of the others.
fn conditions(
    field: &str,
    listed: Option<&Value>,
    list: List,
    into: &mut WatchedFor,
) -> Result<(), String> {
    let Some(listed) = listed else {
        return Ok(());
    };
    let listed = listed.as_array().ok_or_else(|| {
        format!(
            "{field} must be a list of conditions, such as [{{\"execute\": \"&0038\"}}]; \
             {CONDITION_FORMS}"
        )
    })?;
    if listed.len() > MOST_CONDITIONS {
        return Err(format!(
            "{field} has {} conditions; give at most {MOST_CONDITIONS}",
            listed.len()
        ));
    }
    for (index, value) in listed.iter().enumerate() {
        let (condition, capture) = condition(value, list)
            .map_err(|problem| format!("{field}[{index}]: {problem}; {CONDITION_FORMS}"))?;
        into.conditions.push(condition);
        into.capturing.push(capture);
    }
    Ok(())
}

// The conditions of until, gather and count, in that order, with what each
// captures beside it, as given; the values and the spans of memory every
// capture takes, each once, in the order the conditions first name them; and
// which of those each condition names, by their places among them.
struct WatchedFor {
    conditions: Vec<Condition>,
    capturing: Vec<Option<Capturing>>,
    values: Vec<(String, Vec<Segment>)>,
    spans: Vec<Span>,
    named: Vec<Named>,
}

// A bit to each of the values and the spans a request's captures name,
// MOST_CAPTURED at most.
#[derive(Clone, Copy, Default)]
struct Named {
    values: u16,
    spans: u16,
}

fn watched_for(
    until: Option<&Value>,
    gather: Option<&Value>,
    count: Option<&Value>,
) -> Result<WatchedFor, String> {
    let mut watched = WatchedFor {
        conditions: Vec::new(),
        capturing: Vec::new(),
        values: Vec::new(),
        spans: Vec::new(),
        named: Vec::new(),
    };
    conditions("until", until, UNTIL, &mut watched)?;
    conditions("gather", gather, GATHER, &mut watched)?;
    conditions("count", count, COUNT, &mut watched)?;
    for capture in watched.capturing.iter().flatten() {
        for value in &capture.values {
            if !watched.values.iter().any(|(path, _)| *path == value.0) {
                watched.values.push(value.clone());
            }
        }
        for span in &capture.memory {
            if !watched.spans.iter().any(|kept| same_span(kept, span)) {
                watched.spans.push(*span);
            }
        }
    }
    let named = watched.values.len() + watched.spans.len();
    if named > MOST_CAPTURED {
        return Err(format!(
            "the captures of until and gather name {named} distinct values and spans; give at most \
             {MOST_CAPTURED} together"
        ));
    }
    let bytes: u32 = watched
        .spans
        .iter()
        .map(|span| u32::from(span.length))
        .sum();
    if bytes > CAPTURED_BYTES {
        return Err(format!(
            "the captures of until and gather read {bytes} bytes, each distinct span counted once; \
             give at most {CAPTURED_BYTES} together"
        ));
    }
    let bits = |chosen: &mut dyn Iterator<Item = bool>| {
        chosen
            .enumerate()
            .filter(|(_, named)| *named)
            .fold(0, |bits, (index, _)| bits | 1 << index)
    };
    watched.named = watched
        .capturing
        .iter()
        .map(|capture| {
            capture.as_ref().map_or(Named::default(), |capture| Named {
                values: bits(
                    &mut watched
                        .values
                        .iter()
                        .map(|(path, _)| capture.values.iter().any(|(named, _)| named == path)),
                ),
                spans: bits(
                    &mut watched
                        .spans
                        .iter()
                        .map(|kept| capture.memory.iter().any(|span| same_span(kept, span))),
                ),
            })
        })
        .collect();
    Ok(watched)
}

// gather_last, count_by and path say how gather and count keep what matches
// their conditions, and mean nothing without them.
fn kept_as_asked(
    gather: Option<&Value>,
    gather_last: Option<bool>,
    count: Option<&Value>,
    count_by: Option<CountBy>,
    path: Option<&PathBuf>,
) -> Result<(), String> {
    if gather.is_none() && count.is_none() && path.is_some() {
        return Err(
            "path is given without gather or count; give gather or count, or leave path out"
                .to_owned(),
        );
    }
    if gather.is_none() && gather_last.is_some() {
        return Err(
            "gather_last is given without gather; give gather, or leave gather_last out".to_owned(),
        );
    }
    if count.is_none() && count_by.is_some() {
        return Err(
            "count_by is given without count; give count, or leave count_by out".to_owned(),
        );
    }
    Ok(())
}

fn watching(
    watched_for: &WatchedFor,
    gather_last: Option<bool>,
    count_by: Option<CountBy>,
    filing: bool,
) -> Watching<'_> {
    let conditions = &watched_for.conditions;
    let gathering = conditions.iter().any(|condition| condition.list == GATHER);
    let gathering_captures = conditions
        .iter()
        .any(|condition| condition.list == GATHER && condition.captures);
    Watching {
        conditions,
        spans: &watched_for.spans,
        most_gathered: if filing && gathering {
            MOST_GATHERED_TO_A_FILE
        } else if gathering_captures {
            MOST_GATHERED_CAPTURING
        } else {
            MOST_GATHERED
        },
        gathers_last: gather_last == Some(true),
        most_counted: (!filing).then_some(MOST_COUNTED),
        counts_by_pc: count_by == Some(CountBy::Pc),
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

// Each list of conditions a request gave, as the journal keeps it, whatever
// form it was given in: its addresses and its bytes after &, its registers by
// their numbers, a range of one address, register or byte as that one, the
// bytes from 0 to &FF as none, changes where it is true, and a capture's spans
// each with its length.
fn journal_conditions(arguments: &mut Value, watched_for: &WatchedFor) {
    let written = |(condition, capture): (&Condition, &Option<Capturing>)| {
        let (name, _) = KIND_NAMES
            .iter()
            .find(|(_, kind)| *kind == condition.kind)
            .expect("every kind is named");
        let mut fields = Map::new();
        fields.insert(
            (*name).to_owned(),
            written_as(condition.kind, condition.first),
        );
        if condition.kind == IN || condition.kind == OUT {
            fields.insert(
                "mask".to_owned(),
                written_as(condition.kind, condition.mask),
            );
        } else if condition.last != condition.first {
            fields.insert("to".to_owned(), written_as(condition.kind, condition.last));
        }
        if (condition.first_value, condition.last_value) != (0, 0xFF) {
            fields.insert(
                "value".to_owned(),
                json!(Hexadecimal::byte(condition.first_value)),
            );
            if condition.last_value != condition.first_value {
                fields.insert(
                    "value_to".to_owned(),
                    json!(Hexadecimal::byte(condition.last_value)),
                );
            }
        }
        if condition.changes {
            fields.insert("changes".to_owned(), json!(true));
        }
        if let Some((name, _)) = MEMORY_NAMES
            .iter()
            .find(|(_, memory)| *memory == condition.memory)
        {
            fields.insert("memory".to_owned(), json!(name));
        }
        if let Some(capture) = capture {
            let mut captured = Map::new();
            if !capture.values.is_empty() {
                let values: Vec<&String> = capture.values.iter().map(|(path, _)| path).collect();
                captured.insert("values".to_owned(), json!(values));
            }
            if !capture.memory.is_empty() {
                let memory: Vec<Value> = capture
                    .memory
                    .iter()
                    .map(|span| {
                        json!({ "address": start_written(span.start, span.address), "length": span.length })
                    })
                    .collect();
                captured.insert("memory".to_owned(), json!(memory));
            }
            fields.insert("capture".to_owned(), Value::Object(captured));
        }
        Value::Object(fields)
    };
    for (field, list) in [("until", UNTIL), ("gather", GATHER), ("count", COUNT)] {
        if let Some(listed) = arguments.get_mut(field) {
            *listed = watched_for
                .conditions
                .iter()
                .zip(&watched_for.capturing)
                .filter(|(condition, _)| condition.list == list)
                .map(written)
                .collect();
        }
    }
}

// A number as a CPC's own documentation writes it, in hexadecimal after &:
// an address or a port in four digits, a byte in two, and an address in a
// 6128's second 64K in five.
struct Hexadecimal {
    value: u32,
    digits: usize,
}

impl Hexadecimal {
    fn address(address: u16) -> Hexadecimal {
        Hexadecimal {
            value: address.into(),
            digits: 4,
        }
    }

    fn byte(byte: u8) -> Hexadecimal {
        Hexadecimal {
            value: byte.into(),
            digits: 2,
        }
    }

    fn ram(address: u32) -> Hexadecimal {
        Hexadecimal {
            value: address,
            digits: if address > 0xFFFF { 5 } else { 4 },
        }
    }
}

impl fmt::Display for Hexadecimal {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "&{:0digits$X}", self.value, digits = self.digits)
    }
}

impl Serialize for Hexadecimal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

// A string, which each field's description says is hexadecimal after &. No
// answer's schema carries a pattern: the tests hold every answer to its
// schema by the keywords they read, and a pattern is not among them.
impl JsonSchema for Hexadecimal {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "Hexadecimal".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string" })
    }
}

fn digit_pairs(bytes: &[u8]) -> String {
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
            "{text} is not bytes; write bytes as pairs of hexadecimal digits, such as 3E05C9 or 3E \
             05 C9"
        )
    })
}

// What a search looks for, a byte at a time: a value, and the mask of the
// bits a byte must share with it.
fn sought(pattern: Vec<(u8, u8)>, field: &str, units: &str) -> Result<Vec<(u8, u8)>, String> {
    if !(1..=MOST_SOUGHT).contains(&pattern.len()) {
        return Err(format!(
            "{field} holds {} {units}; give 1 to {MOST_SOUGHT}",
            pattern.len()
        ));
    }
    Ok(pattern)
}

fn pattern(text: &str) -> Result<Vec<(u8, u8)>, String> {
    let digits: Vec<char> = text
        .chars()
        .filter(|digit| !digit.is_whitespace())
        .collect();
    let pattern = digits.chunks(2).map(|pair| match pair {
        ['?', '?'] => Some((0, 0)),
        [high, low] if high.is_ascii_hexdigit() && low.is_ascii_hexdigit() => {
            u8::from_str_radix(&format!("{high}{low}"), 16)
                .ok()
                .map(|byte| (byte, 0xFF))
        }
        _ => None,
    });
    let pattern = pattern.collect::<Option<Vec<_>>>().ok_or_else(|| {
        format!(
            "{text} is not bytes; write bytes as pairs of hexadecimal digits, with ?? for any \
             byte, such as CD ?? 14"
        )
    })?;
    sought(pattern, "bytes", "bytes")
}

fn text_pattern(text: &str) -> Result<Vec<(u8, u8)>, String> {
    if let Some(other) = text.chars().find(|character| !character.is_ascii()) {
        return Err(format!(
            "text holds {other}, which is not ASCII; give ASCII characters only"
        ));
    }
    sought(
        text.bytes().map(|code| (code, 0x7F)).collect(),
        "text",
        "characters",
    )
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    /// The machine, by the name the emulator's command line gives it.
    #[schemars(schema_with = "machine_names")]
    machine: String,
    /// A snapshot to load once the machine is built: a path on the server's file system. A relative path starts from the server's working directory.
    #[schemars(transform = absent_not_null)]
    snapshot: Option<PathBuf>,
    /// A state file that `state_save` wrote, to build the machine as the state was: a path on the server's file system. A relative path starts from the server's working directory. Give `snapshot` or `state`, not both.
    #[schemars(transform = absent_not_null)]
    state: Option<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    /// The handle `machine_open` returned.
    handle: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// How many monitor retraces to run, 1 to 1500.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<i64>,
    /// How many ticks to run, from 1 to the ticks of 1500 standard frames.
    #[schemars(range(min = 1))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    ticks: Option<i64>,
    /// Conditions that stop the run at the first match; at most 16. The kinds:
    /// - `{"execute": address}` stops before an instruction that starts at that address. It matches only where an instruction starts: an address the processor passes only as part of a longer instruction does not match. A run that starts at that address runs that instruction first.
    /// - `{"read": address}` and `{"write": address}` stop at the end of an instruction that reads the byte there as data, or writes it. An instruction's own bytes are never data.
    /// - `{"in": port, "mask": bits}` and `{"out": port, "mask": bits}` stop at the end of an instruction that reads or writes a port whose address matches `port` on the bits set in `mask`, the bits the device decodes: `{"out": "&7F00", "mask": "&C000"}` is a CPC's Gate Array, `{"out": "&FE", "mask": "&01"}` a Spectrum's ULA.
    /// - `{"psg": register}`, CPC only, 0 to 15, stops at the end of an instruction that writes that PSG register. The emulator writes the selected register at every access to the PPI while port C gives the PSG the function WRITE.
    /// - `{"crtc": register}`, CPC only, 0 to 31 (the 6845 has 0 to 17), stops at the end of an instruction that writes that 6845 register. Every access to a port with A14 and A9 low and A8 high, such as &BDxx, writes the selected register, an IN included, which gives it the top half of the port's address.
    ///
    /// The options:
    /// - `"to"`: a last address or register, for a range (execute, read, write, psg, crtc).
    /// - `"memory": "ram"` or `"rom"` (execute, read): match only bytes the RAM holds, or only bytes a ROM holds. On a CPC the lower ROM covers &0000 to &3FFF and an upper ROM &C000 to &FFFF, each while the Gate Array enables it; on a Spectrum the ROM covers &0000 to &3FFF.
    /// - `"value"`, with `"value_to"` for a range (all but execute and in): match only when the byte read, written or given to the register is in that range. `{"out": "&7F00", "mask": "&C000", "value": "&80", "value_to": "&BF"}` matches the writes that set the Gate Array's mode, ROMs and interrupt counter.
    /// - `"changes": true` (write, psg, crtc): match only a write that changes the byte, or the bits the register keeps. `{"write": "&C7EC", "value": 6, "changes": true}` is the write that makes that byte 6.
    /// - `"capture": {"values": paths, "memory": spans}` (until and gather): each match then has a `captured` field with what the machine held. A path names one of the processor's values under `cpu`, as `inspect` names it: HL is `cpu.h` and `cpu.l`, IX is `cpu.ixh` and `cpu.ixl`, and the alternate set is held in pairs, as `cpu.hl_`. A span is `{"address": address, "length": bytes}`, 1 to 64 bytes (1 if `length` is omitted), none past &FFFF; with `"address": "cpu.sp"` or `"cpu.pc"` it starts where that register points at the capture, and wraps past &FFFF to &0000. An execute captures before its instruction runs. An access captures at the end of the instruction that made it, before any interrupt response there (IFF1 is already clear then); an access made by an interrupt response captures at the response's end. A request's captures may name at most 16 distinct values and spans together, reading at most 64 bytes.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<Value>,
    /// Conditions, written like `until`'s, whose matches are listed as the run goes; a match at the instruction the run starts on is included. At most 16.
    /// - On a CPC a match says which memory held its byte or instruction where the address alone does not: `"rom": "lower"` or `"upper 7"`, or `"ram"`, the RAM's own address. `"pc_rom"` and `"pc_ram"` say the same for the instruction that made an access.
    /// - The run stops at the end of the instruction that makes the 128th match, keeping that instruction's other matches. The limit is 32 when one of gather's conditions captures, and 1048576 with `path`, capturing or not. The next run continues from there.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// `true` keeps the last matches instead of the first, as many as the limit, in order, and runs on past the limit, so a run stopped by `until`, or at its end, shows what led there. Default `false`; refused without `gather`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather_last: Option<bool>,
    /// Conditions, written like `until`'s but without `capture`, whose matches are counted by place; at most 16. Each place is listed once with its count and its first match, and an instruction also with the bytes it first ran with. Each of these is a place of its own:
    /// - an instruction started at a byte of memory,
    /// - a byte of memory read,
    /// - a byte of memory written,
    /// - a port read, by its whole address,
    /// - a port written, by its whole address,
    /// - a register of the PSG or the 6845 written.
    ///
    /// On a CPC a place in memory is a byte of RAM or ROM, not an address: a ROM's byte and the RAM's byte at the same address are 2 places, and a byte of RAM a 6128 pages in at 2 addresses is 1 place, listed at the address the processor first reached it at. `{"execute": "&4000", "to": "&BFFF"}` counts every instruction run in that range. The run stops at the end of the instruction that adds the 256th place; the next run counts afresh. With `path`, the number of places never stops the run.
    #[schemars(schema_with = "count_conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
    /// `"address"` (the default) counts an access by its place; `"pc"` counts it by the byte of memory where the instruction that made it starts, so a count of writes to the screen shows which instructions wrote it and how often. Instructions run are counted by address either way. With `"pc"`, accesses made by an interrupt response are not counted. Refused without `count`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count_by: Option<CountBy>,
    /// Keys held down from the start of the run to its end, on top of what the keyboard already holds; afterwards the keyboard goes back to what it held. Names are colophon-player's, as printed on the keys; for example:
    /// - CPC: "SPACE", "Q", "RETURN", "SHIFT", "CTRL", "COPY", "f5", "↑", and for joystick 0 "(UP)", "(DOWN)", "(LEFT)", "(RIGHT)", "(FIRE 1)", "(SPARE)" and "(FIRE 2)", its main button. A CPC key may also be given by its firmware number, 0 to 79: its matrix line times 8 plus its bit.
    /// - Spectrum: "CAPS", "SYM", "ENTER", "SPACE", "Q".
    ///
    /// An unknown key is refused with the list of names. A program that reads the keyboard only now and then can miss a key held for less than the time between its reads.
    #[schemars(schema_with = "keys_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<Vec<Value>>,
    /// A new file to receive what `gather` and `count` find, instead of the answer's lists, for the client to read with its own tools: a path on the server's file system, a relative one starting from the server's working directory. Each line is a JSON object, first the matches, then the places, each as the answer would list it, starting with `"list": "gather"` or `"count"`. The file is complete once the answer gives its SHA-256. Refused if the file exists or its directory does not, and without `gather` or `count`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// Where the routine starts.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// Values to write first, by path, as `set` writes them: `{"cpu.a": 1, "cpu.h": 64, "cpu.l": 0}`.
    #[schemars(schema_with = "values_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<Map<String, Value>>,
    /// The most frames the call waits for the routine to return, 1 to 1500. Default 1500.
    #[schemars(range(min = 1, max = MOST_FRAMES))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<i64>,
    /// Like `run`'s `until`. The routine's first instruction is never stopped before. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<Value>,
    /// Like `run`'s `gather`. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// Like `run`'s `gather_last`. Default `false`; refused without `gather`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather_last: Option<bool>,
    /// Like `run`'s `count`. At most 16.
    #[schemars(schema_with = "count_conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
    /// Like `run`'s `count_by`. Default `"address"`; refused without `count`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count_by: Option<CountBy>,
    /// Like `run`'s `path`.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// What to type: `"PRINT 2+2\n"` on a CPC, `"p2+2\n"` on a Spectrum.
    text: String,
}

// What a count counts an access by.
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CountBy {
    Address,
    Pc,
}

// A CPC's two drives, as AMSDOS names them.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
enum Drive {
    /// The machine's own drive, single-headed.
    A,
    /// A second drive, double-headed.
    B,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// The disc image: a path on the server's file system. A relative path starts from the server's working directory.
    path: PathBuf,
    /// The drive. Default A.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    drive: Option<Drive>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TapeRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// The tape image: a path on the server's file system. A relative path starts from the server's working directory.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// Where to write the snapshot: a path on the server's file system that names no file yet. A relative path starts from the server's working directory.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StateSaveRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The state's name, as `state_keep` returned it.
    state: String,
    /// Where to write the state file: a path on the server's file system that names no file yet. A relative path starts from the server's working directory.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KeepRequest {
    /// The handle `machine_open` returned.
    handle: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StateRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The state's name, as `state_keep` returned it.
    state: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JournalRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The first entry to return, counting from 0. Default 0.
    #[schemars(range(min = 0))]
    #[schemars(transform = absent_not_null)]
    from: Option<i64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StepRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// How many instructions to run, 1 to 100000. Default 1.
    #[schemars(range(min = 1, max = MOST_INSTRUCTIONS))]
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InspectRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// What to read. Default: the whole machine.
    #[schemars(transform = absent_not_null)]
    path: Option<String>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// Each value to write, by path: `{"cpu.pc": "&4000", "gate_array.inks[16]": 20}`. A structure takes an object with some of its fields; an array takes an array of all its elements.
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PeekRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The first address, as the processor reads it, or `"cpu.sp"` or `"cpu.pc"` for the address that register holds. Give this or `ram`.
    #[schemars(schema_with = "address_or_register_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    address: Option<Value>,
    /// The first address in a CPC's RAM, by the RAM's own addresses. Give this or `address`.
    #[schemars(schema_with = "ram_address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    ram: Option<Value>,
    /// How many bytes, 1 to 4096. From an address, none past &FFFF; from `cpu.sp` or `cpu.pc`, the bytes wrap past &FFFF to &0000. With `ram`, none past the end of the RAM. Default 1.
    #[schemars(range(min = 1, max = MOST_BYTES))]
    #[schemars(transform = absent_not_null)]
    length: Option<i64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PokeRequest {
    /// The handle `machine_open` returned.
    #[serde(skip_serializing)]
    handle: String,
    /// The first address to write.
    #[schemars(schema_with = "address_schema")]
    address: Value,
    /// The bytes, as pairs of hexadecimal digits: `3E05C9` or `3E 05 C9`. 1 to 4096 bytes, none past &FFFF.
    bytes: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FindRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The bytes to find, as pairs of hexadecimal digits, with `??` for any byte: `CD ?? 14` or `CD??14`. 1 to 256 bytes. Give this or `text`.
    #[schemars(transform = absent_not_null)]
    bytes: Option<String>,
    /// The text to find: 1 to 256 ASCII characters, each matching a byte whose low 7 bits are its code. Case matters. Give this or `bytes`.
    #[schemars(transform = absent_not_null)]
    text: Option<String>,
    /// The first address searched. Default &0000.
    #[schemars(schema_with = "address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    address: Option<Value>,
    /// The last address searched; a match lies wholly between `address` and `to`. Default &FFFF.
    #[schemars(schema_with = "address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    to: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MonitorRequest {
    /// The handle `machine_open` returned.
    handle: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// The layout of the bytes. Default `video`.
    #[schemars(transform = absent_not_null)]
    reading: Option<Reading>,
    /// The RAM address of the first byte. For `video`, a display start at the start of a 16K page or up to 2K into it, in the base 64K; for `linear` and `columns`, any RAM address, a 6128's second 64K being &10000 to &1FFFF. Default: the 6845's start, from R12 and R13. A base given reads as if R12's bits 3 and 2 were clear, so only the default carries a 32K screen on into the next 16K.
    #[schemars(schema_with = "ram_address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    base: Option<Value>,
    /// Bytes across, 1 to 128. Default: twice R1. In `video` it also spaces the character rows, so a width other than the machine's does not crop its screen.
    #[schemars(range(min = 1, max = MOST_WIDTH))]
    #[schemars(transform = absent_not_null)]
    width: Option<i64>,
    /// Lines down, 1 to 512. Default: R6 times `rasters`.
    #[schemars(range(min = 1, max = MOST_HEIGHT))]
    #[schemars(transform = absent_not_null)]
    height: Option<i64>,
    /// Lines per character row, 1 to 32, for `video` only. Default: R9 plus 1.
    #[schemars(range(min = 1, max = MOST_RASTERS))]
    #[schemars(transform = absent_not_null)]
    rasters: Option<i64>,
    /// How a byte becomes pixels: 0 for 2 pixels of 16 pens, 1 for 4 of 4, 2 for 8 of 2, 3 for 2 of 4. Default: the mode the Gate Array is drawing in, `gate_array.mode`.
    #[schemars(range(min = 0, max = 3))]
    #[schemars(transform = absent_not_null)]
    mode: Option<i64>,
    /// 16 hardware colour codes, 0 to 31, pen by pen, as `gate_array.inks` holds them. Default: the Gate Array's inks.
    #[schemars(length(equal = 16), inner(range(min = 0, max = 31)))]
    #[schemars(transform = absent_not_null)]
    palette: Option<Vec<i64>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DisassembleRequest {
    /// The handle `machine_open` returned.
    handle: String,
    /// Where the first instruction starts: an address, or `"cpu.pc"` or `"cpu.sp"` for the address that register holds.
    #[schemars(schema_with = "address_or_register_schema")]
    address: Value,
    /// How many instructions, 1 to 256. Default 16.
    #[schemars(range(min = 1, max = MOST_INSTRUCTIONS_LISTED))]
    #[schemars(transform = absent_not_null)]
    instructions: Option<i64>,
}

#[derive(Serialize, JsonSchema)]
struct Standing {
    /// The handle `machine_open` returned.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The states kept from the machine, in the order they were kept. Not given when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    states: Vec<StateHeld>,
}

#[derive(Clone, Serialize, JsonSchema)]
struct StateHeld {
    /// The state's name.
    state: String,
    /// The machine's tick when the state was kept.
    tick: u64,
}

#[derive(Serialize, JsonSchema)]
struct Opened {
    /// The handle the server gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// Every file read to build the machine, with the SHA-256 of what it held: the firmware first, the snapshot or the state file last.
    files: Vec<FileRead>,
    /// The machine's tick: 0, or the state's when built from a state file.
    tick: u64,
}

#[derive(Serialize, JsonSchema)]
struct Listing {
    /// The open machines, in the order they were opened.
    machines: Vec<Standing>,
}

#[derive(Serialize, JsonSchema)]
struct Inserted {
    /// The handle `machine_open` returned.
    handle: String,
    /// The drive the disc went into. Not given for a tape.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    drive: Option<Drive>,
    /// The machine's tick when the disc or tape went in.
    tick: u64,
    #[serde(flatten)]
    file: FileRead,
}

#[derive(Serialize, JsonSchema)]
struct Saved {
    /// The handle `machine_open` returned.
    handle: String,
    /// The machine's tick when the snapshot was saved.
    tick: u64,
    /// The absolute path the snapshot was written to.
    path: PathBuf,
    /// The SHA-256 of the file written, in hexadecimal.
    sha256: String,
}

#[derive(Serialize, JsonSchema)]
struct StateSaved {
    /// The handle `machine_open` returned.
    handle: String,
    /// The name of the state written.
    state: String,
    /// The machine's tick, as the state holds it.
    tick: u64,
    /// The absolute path the state file was written to.
    path: PathBuf,
    /// The SHA-256 of the file written, in hexadecimal.
    sha256: String,
}

#[derive(Serialize, JsonSchema)]
struct Kept {
    /// The handle `machine_open` returned.
    handle: String,
    /// The state's name. No other state on this server is ever given it.
    state: String,
    /// The machine's tick when the state was kept.
    tick: u64,
}

#[derive(Serialize, JsonSchema)]
struct Restored {
    /// The handle `machine_open` returned.
    handle: String,
    /// The name of the state restored.
    state: String,
    /// The machine's tick, as the state holds it.
    tick: u64,
    /// The program counter, in hexadecimal after &: the address the next instruction is fetched from.
    pc: Hexadecimal,
}

#[derive(Serialize, JsonSchema)]
struct Dropped {
    /// The handle `machine_open` returned.
    handle: String,
    /// The name of the state dropped. It no longer names a state.
    state: String,
}

#[derive(Serialize, JsonSchema)]
struct Journal {
    /// The handle `machine_open` returned.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The build the machine ran on, as the server's `serverInfo.version` names it.
    build: String,
    /// Every file the machine was built from, with the SHA-256 of what it held: the firmware first, the snapshot last. For a machine built from a state file, the files the state's machine was built from.
    files: Vec<FileRead>,
    /// The number of entries in the journal.
    count: usize,
    /// The index of the first entry given, counting from 0.
    from: usize,
    /// The entries, in the order their requests were made.
    entries: Vec<Entry>,
    /// The `from` of the next page. Not given on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    next: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
struct Closed {
    /// The handle the machine had. It no longer names a machine.
    handle: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Stopped {
    /// The monitor retraced `frames` times.
    Frames,
    /// The monitor retraced fewer than `frames` times within the limit: twice a standard frame's ticks for each frame asked.
    Limit,
    /// All the ticks asked for were run.
    Ticks,
    /// All the instructions asked for were run.
    Instructions,
    /// The text was typed: each key was pressed and released.
    Typed,
    /// The run ended inside an instruction, and `settled` is false: the instruction did not end within the longest time any instruction takes. Only a chain of prefixes runs that long.
    Unsettled,
    /// One of `until`'s conditions matched. `matched` lists the matches.
    Matched,
    /// `gather` or `count` reached its limit. The run stopped at the end of the instruction that reached it, keeping that instruction's other matches and those of an interrupt response taken at its end. With `gather_last`, `gather` never fills; with `path`, `count` never fills. The limits:
    /// - `gather`: 128 matches, 32 when one of its conditions captures, or 1048576 with `path`, capturing or not.
    /// - `count`: 256 places.
    Full,
    /// The processor executed a HALT with interrupts disabled and would wait there forever: IFF1 is clear, so INT is ignored, and neither machine raises an NMI. From here:
    /// - Another run runs as asked, halted throughout, unless `set` has written in an NMI.
    /// - `call` wakes the processor for its routine, and halts it again if the routine returns within the call.
    /// - `set` takes it past the HALT by writing `cpu.halted` as false, and so does `state_restore` of a state kept before the HALT.
    Halted,
    /// The called routine returned.
    Returned,
}

#[derive(Serialize, JsonSchema)]
struct Matched {
    /// The machine's tick: for an `execute` match, just before the instruction begins; for an access, just after the tick that put it on the bus.
    tick: u64,
    /// The address of an instruction about to begin, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    execute: Option<Hexadecimal>,
    /// The address of a byte the program read as data, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    read: Option<Hexadecimal>,
    /// The address of a byte the processor wrote, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    write: Option<Hexadecimal>,
    /// The address of a port the processor read, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    r#in: Option<Hexadecimal>,
    /// The address of a port the processor wrote, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    out: Option<Hexadecimal>,
    /// The number of a CPC's PSG register written, 0 to 15.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    psg: Option<u16>,
    /// The number of a CPC's 6845 register written, 0 to 31 (the 6845 has 0 to 17), as its address register holds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    crtc: Option<u16>,
    /// On a CPC, the ROM that held the instruction or answered the read: `"lower"`, or `"upper N"` for upper ROM N, such as `"upper 7"`. When the selected upper ROM is not fitted, ROM 0 answers, named `"upper 0"`. Not given when the RAM held the byte, nor for a write, which reaches the RAM beneath an enabled ROM.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rom: Option<String>,
    /// On a CPC, the byte's address in the RAM, as `peek`'s `ram` counts it. Given only when the RAM held the byte at an address other than the one the processor reached it at. That happens only on a 6128, when its RAM configuration pages in 16K blocks of its second 64K, or puts its base 64K's &C000 to &FFFF at &4000.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    ram: Option<Hexadecimal>,
    /// The byte read or written, in hexadecimal after &. Not given for an `execute` match. For these kinds it is:
    /// - A port read: the byte the processor took.
    /// - A PSG register: the byte the PPI's port A gave it. `psg.registers` keeps only as many low bits as the register has.
    /// - A 6845 register: the byte on the bus, or for an IN the top half of the port's address. `crtc.registers` keeps as many bits as the register has.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    value: Option<Hexadecimal>,
    /// The address of the instruction that made the access, in hexadecimal after &. Not given for an `execute` match, whose `execute` is that address, nor for an access made by an interrupt response.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc: Option<Hexadecimal>,
    /// On a CPC, the ROM that held the instruction at `pc`, in the same form as `rom`. Not given when the RAM held the instruction, or when `pc` is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_rom: Option<String>,
    /// On a CPC, the address in the RAM of the instruction at `pc`, as `ram` gives it. Given only when `pc` is, and the RAM held the instruction at an address other than `pc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_ram: Option<Hexadecimal>,
    /// The beam's position across its line, as `monitor.beam_x` counts it, in samples. Samples 0 to `monitor.width` minus 1 make the line in the raster, which `monitor`'s picture crops. The count continues past them until a sync ends the line.
    beam_x: u16,
    /// The beam's line, as `monitor.beam_y` counts it, 0 to `monitor.height` minus 1 unless `set` has written it higher. In a frame longer than that, the beam stays on the last line until the retrace.
    beam_y: u16,
    /// What the `capture` options of the matched conditions name, each once, read at the moment `run`'s `until` describes for `capture`. Not given when none of those conditions captures. Not given for an access made by an instruction that had not ended when the run did (`settled` false). Only an instruction that `set` wrote into the emulator's machinery for the processor can make such an access.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    captured: Option<Captured>,
}

#[derive(Serialize, JsonSchema)]
struct Captured {
    /// Each of the processor's values named, by its path, as `inspect` reads it. In the order the conditions first name them, `until`'s before `gather`'s. Not given when none is named.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
    /// Each span of memory named, as `peek` reads it. In the order the conditions first name them, `until`'s before `gather`'s. Not given when none is named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    memory: Vec<CapturedSpan>,
}

#[derive(Serialize, JsonSchema)]
struct CapturedSpan {
    /// The address of the span's first byte, as the processor reads it, in hexadecimal after &. For a span at `cpu.sp` or `cpu.pc`, the address the register held.
    address: Hexadecimal,
    /// The bytes, as pairs of hexadecimal digits.
    bytes: String,
}

impl Captured {
    // What a match captured: the values and the spans the conditions it
    // matched name, each once.
    fn of(seen: &Match, capture: Option<&Capture>, watched_for: &WatchedFor) -> Option<Captured> {
        let capture = capture?;
        let named = watched_for
            .named
            .iter()
            .enumerate()
            .filter(|(index, _)| seen.agreed & (1 << index) != 0)
            .fold(Named::default(), |all, (_, named)| Named {
                values: all.values | named.values,
                spans: all.spans | named.spans,
            });
        let values = watched_for
            .values
            .iter()
            .enumerate()
            .filter(|(index, _)| named.values & (1 << index) != 0)
            .map(|(_, (path, segments))| {
                let value = capture
                    .cpu
                    .get(segments)
                    .expect("a path read from the processor as it was checked");
                (path.clone(), value)
            })
            .collect();
        let mut memory = Vec::new();
        let mut offset = 0;
        for (index, span) in watched_for.spans.iter().enumerate() {
            let length = usize::from(span.length);
            if named.spans & (1 << index) != 0 {
                memory.push(CapturedSpan {
                    address: Hexadecimal::address(capture.cpu.start(span.start, span.address)),
                    bytes: digit_pairs(&capture.memory[offset..offset + length]),
                });
            }
            offset += length;
        }
        Some(Captured { values, memory })
    }
}

// Where a CPC holds a byte of memory, by its location, when the address the
// processor reached it at does not say: the ROM that holds it, the lower ROM
// or an upper ROM by its number, as WinAPE shows a breakpoint "at #C006 in an
// Upper ROM [...] as 0006:Upper n"
// (https://web.archive.org/web/20160525234943/http://www.winape.net/help/breakpoints.html),
// or the RAM's own address.
fn holder(location: u32, address: u16) -> (Option<String>, Option<Hexadecimal>) {
    if location >= FIRST_ROM_LOCATION {
        let rom = match (location - FIRST_ROM_LOCATION) / ROM_BYTES {
            0 => "lower".to_owned(),
            upper => format!("upper {}", upper - 1),
        };
        (Some(rom), None)
    } else if location != u32::from(address) {
        (None, Some(Hexadecimal::ram(location)))
    } else {
        (None, None)
    }
}

impl Matched {
    // Each match a run kept that the list chose, with what it captured.
    fn listed<'a>(
        watched: &'a Watched,
        watched_for: &'a WatchedFor,
        chosen: fn(&Match) -> bool,
    ) -> impl Iterator<Item = Matched> + 'a {
        watched
            .matches
            .iter()
            .enumerate()
            .filter(move |(_, seen)| chosen(seen))
            .map(|(index, seen)| {
                let capture = watched
                    .slots
                    .get(index)
                    .filter(|_| seen.captured)
                    .map(|slot| &watched.captures[*slot as usize]);
                let captured = Captured::of(seen, capture, watched_for);
                Matched::of(seen, captured, watched.paged)
            })
    }

    fn of(seen: &Match, captured: Option<Captured>, paged: bool) -> Matched {
        let at = |kind| (seen.kind == kind).then_some(Hexadecimal::address(seen.address));
        let access = seen.kind != EXECUTE;
        let made = access && !seen.response;
        // A port's location and a register's are the port and the register.
        let (rom, ram) = if paged {
            holder(seen.location, seen.address)
        } else {
            (None, None)
        };
        let (pc_rom, pc_ram) = if paged && made {
            holder(seen.pc_location, seen.pc)
        } else {
            (None, None)
        };
        Matched {
            tick: seen.tick,
            execute: at(EXECUTE),
            read: at(READ),
            write: at(WRITE),
            r#in: at(IN),
            out: at(OUT),
            psg: (seen.kind == PSG).then_some(seen.address),
            crtc: (seen.kind == CRTC).then_some(seen.address),
            rom,
            ram,
            value: access.then_some(Hexadecimal::byte(seen.value)),
            pc: made.then_some(Hexadecimal::address(seen.pc)),
            pc_rom,
            pc_ram,
            beam_x: seen.beam_x,
            beam_y: seen.beam_y,
            captured,
        }
    }
}

// A place counted, told by its first match.
#[derive(Serialize, JsonSchema)]
struct Counted {
    /// The machine's tick at the place's first match: for an `execute` place, just before the instruction began; for an access, just after the tick that put it on the bus.
    tick: u64,
    /// The address where the counted instructions began, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    execute: Option<Hexadecimal>,
    /// The address of a byte the program read as data, in hexadecimal after &. With `count_by` `"pc"`, the address at the place's first match. Later matches may have read others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    read: Option<Hexadecimal>,
    /// The address of a byte the processor wrote, in hexadecimal after &. With `count_by` `"pc"`, the address at the place's first match. Later matches may have written others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    write: Option<Hexadecimal>,
    /// The address of a port the processor read, in hexadecimal after &. With `count_by` `"pc"`, the port at the place's first match. Later matches may have read others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    r#in: Option<Hexadecimal>,
    /// The address of a port the processor wrote, in hexadecimal after &. With `count_by` `"pc"`, the port at the place's first match. Later matches may have written others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    out: Option<Hexadecimal>,
    /// The number of a CPC's PSG register written, 0 to 15. With `count_by` `"pc"`, the register at the place's first match. Later matches may have written others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    psg: Option<u16>,
    /// The number of a CPC's 6845 register written, 0 to 31 (the 6845 has 0 to 17), as its address register holds it. With `count_by` `"pc"`, the register at the place's first match. Later matches may have written others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    crtc: Option<u16>,
    /// On a CPC, the ROM that held the instruction or answered the read, in the same form as a match's `rom`. Not given when the RAM held the byte, nor for a write. With `count_by` `"address"`, it is part of the place: the same address in another ROM or in the RAM is another place.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rom: Option<String>,
    /// On a CPC, the byte's address in the RAM, as a match's `ram` gives it. Given only when the RAM held the byte at an address other than the one the processor reached it at. With `count_by` `"address"`, it is part of the place.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    ram: Option<Hexadecimal>,
    /// The address of the instruction that made the place's first match, in hexadecimal after &. With `count_by` `"pc"`, it names the place, with `pc_rom` and `pc_ram`. Not given for an `execute` place, whose `execute` is that address, nor for an access made by an interrupt response.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc: Option<Hexadecimal>,
    /// On a CPC, the ROM that held the instruction at `pc`, in the same form as `rom`. With `count_by` `"pc"`, it is part of the place. Not given when the RAM held the instruction, or when `pc` is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_rom: Option<String>,
    /// On a CPC, the address in the RAM of the instruction at `pc`, as `ram` gives it. Given only when `pc` is, and the RAM held the instruction at an address other than `pc`. With `count_by` `"pc"`, it is part of the place.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_ram: Option<Hexadecimal>,
    /// The instruction's opcodes and operands as the processor first fetched them at this address, as pairs of hexadecimal digits, so that code overwritten since still shows what ran. At most 4 bytes: a longer instruction, which only a chain of prefixes makes, gives its first 4. Given for an `execute` place only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    bytes: Option<String>,
    /// The number of times the place matched.
    count: u32,
}

impl Counted {
    fn of((place, count): &(Place, u32), paged: bool) -> Counted {
        let first = Matched::of(&place.first, None, paged);
        let access = place.first.kind != EXECUTE;
        Counted {
            tick: first.tick,
            execute: first.execute,
            read: first.read,
            write: first.write,
            r#in: first.r#in,
            out: first.out,
            psg: first.psg,
            crtc: first.crtc,
            rom: first.rom,
            ram: first.ram,
            pc: first.pc,
            pc_rom: first.pc_rom,
            pc_ram: first.pc_ram,
            bytes: (!access).then(|| digit_pairs(&place.bytes[..usize::from(place.length)])),
            count: *count,
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct Ran {
    #[serde(flatten)]
    run: Run,
    /// The machine's tick after the run: the ticks it has run since `machine_open` built it, from power-on or from its snapshot.
    tick: u64,
    /// The program counter, in hexadecimal after &: the address the next instruction is fetched from.
    pc: Hexadecimal,
    /// Why the run ended.
    stopped: Stopped,
    /// The matches of `until`'s conditions when the run stopped. Given when one of them matched. The list holds:
    /// - The matching accesses of the instruction the run stopped after, and of an interrupt response taken at its end.
    /// - The instruction at `pc`, which the run stopped before, if it matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    matched: Option<Vec<Matched>>,
    /// The matches of `gather`'s conditions, in the order they happened. Given with `gather`, without `path`. Which matches:
    /// - Without `gather_last`, the first 128, or 32 when one of `gather`'s conditions captures, with the other matches of the instruction that made the last of them and of an interrupt response taken at its end.
    /// - With `gather_last`, the last 128, or 32.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    gathered: Option<Vec<Matched>>,
    /// Each place that matched `count`'s conditions, listed once as its first match, with the number of times it matched. Given with `count`, without `path`. A byte of memory the processor reaches at 2 addresses is 1 place, listed at the address of its first match. The order:
    /// - By kind: `execute`, `read`, `write`, `in`, `out`, `psg`, `crtc`.
    /// - Within a kind, by address or register, or by `pc` with `count_by` `"pc"`.
    /// - At the same address, the RAM's places first, by their addresses in the RAM, then the lower ROM's, then the upper ROMs' by number.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    counted: Option<Vec<Counted>>,
    /// The file written at `path`. It holds what `gathered` and `counted` would have listed, and they are not given. Given with `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    written: Option<Filed>,
}

// The file a run wrote its lists to.
#[derive(Serialize, JsonSchema)]
struct Filed {
    #[serde(flatten)]
    file: FileRead,
    /// The number of the file's lines that are `gather`'s matches, which come first. Given with `gather`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    gathered: Option<usize>,
    /// The number of the file's lines that are `count`'s places, which follow. Given with `count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    counted: Option<usize>,
}

// The lists a run was asked for, and the file that takes them in place of
// the answer, if one was given.
struct Lists {
    gathering: bool,
    counting: bool,
    file: Option<NewFile>,
}

// A line of a run's file: what the answer would list, after the list.
#[derive(Serialize)]
struct Line<T> {
    list: &'static str,
    #[serde(flatten)]
    entry: T,
}

fn line<T: Serialize>(file: &mut NewFile, list: &'static str, entry: T) -> io::Result<()> {
    serde_json::to_writer(&mut *file, &Line { list, entry })?;
    file.write_all(b"\n")
}

fn write_lists(file: &mut NewFile, watched: &Watched, watched_for: &WatchedFor) -> io::Result<()> {
    for matched in Matched::listed(watched, watched_for, |seen| seen.gathers) {
        line(file, "gather", matched)?;
    }
    for place in &watched.counts {
        line(file, "count", Counted::of(place, watched.paged))?;
    }
    Ok(())
}

// The file a run's answer names, for its journal.
fn wrote(ran: &Result<Ran, String>) -> Option<FileRead> {
    let filed = ran.as_ref().ok()?.written.as_ref()?;
    Some(filed.file.clone())
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
            pc: Hexadecimal::address(machine.pc()),
            matched: None,
            gathered: None,
            counted: None,
            written: None,
        }
    }

    // What the run's watch saw, and why the run stopped if the watch or a
    // halt for good stopped it. The lists go to the file, if one was given,
    // and a file that cannot be written takes nothing back: the machine
    // stands where the run left it.
    fn watched(
        mut self,
        watched: &Watched,
        watched_for: &WatchedFor,
        lists: Lists,
    ) -> Result<Ran, String> {
        if self.run.settled {
            if watched.returned {
                self.stopped = Stopped::Returned;
            } else if watched.matched {
                self.stopped = Stopped::Matched;
            } else if watched.halted {
                self.stopped = Stopped::Halted;
            } else if watched.full {
                self.stopped = Stopped::Full;
            }
        }
        self.matched = watched
            .matched
            .then(|| Matched::listed(watched, watched_for, |seen| seen.stops).collect());
        let Some(mut file) = lists.file else {
            self.gathered = lists
                .gathering
                .then(|| Matched::listed(watched, watched_for, |seen| seen.gathers).collect());
            self.counted = lists.counting.then(|| {
                watched
                    .counts
                    .iter()
                    .map(|place| Counted::of(place, watched.paged))
                    .collect()
            });
            return Ok(self);
        };
        let written = write_lists(&mut file, watched, watched_for);
        let file = file.finish(written).map_err(|problem| {
            format!(
                "{problem}. The run still happened: it ran {} ticks, and the machine is at tick {} \
                 with its program counter at {}, the request in its journal. To get the file, \
                 make the request again, from a state kept before it or from the journal's \
                 earlier requests, with a path the file system can take",
                self.run.ticks, self.tick, self.pc
            )
        })?;
        let gathered = watched.matches.iter().filter(|seen| seen.gathers).count();
        self.written = Some(Filed {
            file,
            gathered: lists.gathering.then_some(gathered),
            counted: lists.counting.then_some(watched.counts.len()),
        });
        Ok(self)
    }
}

#[derive(Serialize, JsonSchema)]
struct Inspected {
    /// The path read. Empty for the whole machine.
    path: String,
    /// The value the machine holds there.
    #[schemars(schema_with = "any_value_schema")]
    value: Value,
}

#[derive(Serialize, JsonSchema)]
struct Written {
    /// Each path written, with the value it now holds.
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
}

#[derive(Serialize, JsonSchema)]
struct Peeked {
    /// The first address read, as the processor reads it, in hexadecimal after &: for `cpu.sp` or `cpu.pc`, the address the register held. Given with `address`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    address: Option<Hexadecimal>,
    /// The first address read in the RAM, in hexadecimal after &. Given with `ram`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    ram: Option<Hexadecimal>,
    /// The bytes, as pairs of hexadecimal digits.
    bytes: String,
}

#[derive(Serialize, JsonSchema)]
struct Poked {
    /// The first address written, in hexadecimal after &.
    address: Hexadecimal,
    /// The number of bytes written.
    length: u16,
}

#[derive(Serialize, JsonSchema)]
struct Found {
    /// The addresses where matches start, in hexadecimal after &, lowest first: at most the first 256.
    addresses: Vec<Hexadecimal>,
    /// The number of matches from `address` to `to`, listed or not.
    count: usize,
    /// The address of the first match not listed: search again from it, with the same `to`, for the rest. Not given when every match is listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    next: Option<Hexadecimal>,
}

#[derive(Serialize, JsonSchema)]
struct Monitored {
    /// The handle `machine_open` returned.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The machine's tick when the picture was taken.
    tick: u64,
    /// The picture's width in pixels.
    width: u32,
    /// The picture's height in pixels.
    height: u32,
    /// The beam's position across its line, as `monitor.beam_x` counts it, in samples of the whole raster, which the picture crops.
    beam_x: u16,
    /// The beam's line, as `monitor.beam_y` counts it, in lines of the whole raster, which the picture crops.
    beam_y: u16,
    /// Whether the picture is a whole frame:
    /// - `true`: the beam is at or before the picture's first pixel or past its last, as after a run by frames that ran all its frames.
    /// - `false`: the beam is partway through the picture. The lines above the beam, and the part of its line left of it, are from the frame being painted, and the rest from the frame before.
    /// - Either way, a line that a frame shorter than the picture never reaches keeps what an earlier frame left there.
    whole: bool,
}

#[derive(Serialize, JsonSchema)]
struct Drawn {
    /// The handle `machine_open` returned.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
    /// The machine's tick when the memory was read.
    tick: u64,
    /// The layout the bytes were read in.
    reading: Reading,
    /// The address in the RAM of the first byte read, in hexadecimal after &.
    base: Hexadecimal,
    /// The width in bytes.
    width: u32,
    /// The height in lines.
    height: u32,
    /// The number of lines in a character row. Given for `video` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rasters: Option<u32>,
    /// The mode the bytes were read in, 0 to 3. As on the Gate Array, a mode above 3, which only `set` can write, is drawn as 3.
    mode: u8,
    /// 16 hardware colour codes, 0 to 31, pen by pen.
    palette: [u8; 16],
    /// The picture's width in pixels, 8 per byte.
    picture_width: u32,
    /// The picture's height in pixels, 2 per line.
    picture_height: u32,
}

#[derive(Serialize, JsonSchema)]
struct Listed {
    /// The instruction's address, in hexadecimal after &.
    address: Hexadecimal,
    /// The instruction's bytes, as pairs of hexadecimal digits.
    bytes: String,
    /// The instruction in Zilog's mnemonics, with addresses, bytes and displacements in hexadecimal after &, such as "LD HL,&1234".
    text: String,
    /// On a CPC, the name SOFT 968 gives the firmware entry the instruction jumps or calls to, for the entries `disassemble`'s description lists. Not given for any other instruction, nor on a Spectrum.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    firmware: Option<&'static str>,
}

#[derive(Serialize, JsonSchema)]
struct Disassembled {
    /// The instructions, in memory order.
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
                .ok_or_else(|| format!("{name} is {value}; give 1 to {most}"))
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
                .ok_or_else(|| format!("mode is {mode}; give 0 to 3"))
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
                        "palette is {palette:?}; give 16 hardware colour codes, one per pen, each \
                         0 to 31"
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

fn bytes_asked(length: i64) -> Result<u16, String> {
    u16::try_from(length)
        .ok()
        .filter(|length| (1..=MOST_BYTES).contains(length))
        .ok_or_else(|| format!("give 1 to {MOST_BYTES} bytes, not {length}"))
}

// A span of memory that ends at &FFFF or before it.
fn span(address: u16, length: i64) -> Result<u16, String> {
    let length = bytes_asked(length)?;
    let room = 0x10000 - u32::from(address);
    if u32::from(length) > room {
        return Err(format!(
            "{length} bytes from &{address:04X} run past &FFFF, the end of the processor's memory; \
             give at most {room} from there"
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
            format!(
                "{files} could not be reached: {error}; the server's standard error has the details"
            )
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
                "the thread running {handle} has ended, and the server's standard error has the \
                 details; machine_close closes the machine"
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

    /// Build a machine and return its handle: powered on before its first instruction, loaded from a snapshot, or built from a state file. The handle stays valid until `machine_close` or the server stops, and is never reused. The answer gives the SHA-256 of every file read to build the machine, and the machine's tick, which starts at 0, or at the state's. A state file, which `state_save` writes, gives the machine exactly as the state was, with its tick and its journal; only the build that wrote it opens it, as the same machine, on firmware with the same SHA-256. A snapshot is loaded as the emulator's command line loads it, into the machine named, after the firmware is fitted: on a CPC an SNA of version 1, 2 or 3, of which only the version 1 fields are read; on a Spectrum a 48K .sna. A CPC snapshot's 6845 counters, which only version 3 holds, are not read: the CPC starts its frame from the top, with the Gate Array's interrupt counter and the beam at 0, so its first run by frames ends early, at the retrace that the 6845's first vertical sync brings, some 62000 ticks in on the firmware's screen. Until then `monitor.beam_y` is 1 or 2 lines past the 6845's line; after it, on the firmware's screen, 70 or 71 lines past. A Spectrum snapshot holds no beam position. A CPC snapshot does not hold the disc controller, so the controller starts as at power-on: a disc access the snapshot was taken inside does not go on, and the SPECIFY that AMSDOS sends at boot is missing. To set the controller as SPECIFY would have, write these with `set` (times in microseconds):
    /// - `fdc.non_dma`: `true`. Without it AMSDOS reads nothing.
    /// - `fdc.step_time`: `12000`. Without it AMSDOS fails every seek to another track.
    /// - `fdc.head_load_time`: `4000` and `fdc.head_unload_time`: `32000`, the rest of what SPECIFY sets.
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
                "no machine is named {}; the machines are {}",
                request.machine,
                names.join(", ")
            )
        })?;
        if request.snapshot.is_some() && request.state.is_some() {
            return Err(
                "give snapshot or state, not both: a snapshot is loaded into a machine at \
                 power-on, and a state file replaces the whole machine"
                    .to_owned(),
            );
        }
        // Numbered in the order asked, before anything is read.
        let opened = {
            let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.opened += 1;
            threads.opened
        };
        let snapshot = request
            .snapshot
            .as_deref()
            .map(machines::absolute)
            .transpose()?;
        let state = request
            .state
            .as_deref()
            .map(machines::absolute)
            .transpose()?;
        let roms = self.roms.clone();
        let (parts, state) = Server::blocking(
            format!("the files {} is built from", machine.name),
            move || {
                let parts = machine.parts(&roms, snapshot.as_deref())?;
                let state = state.map(|path| machine.state(&path)).transpose()?;
                Ok((parts, state))
            },
        )
        .await?;
        let mut files = parts.files.clone();
        files.extend(state.as_ref().map(|state| state.read.clone()));
        let handle = format!("{}-{opened}", machine.name);
        // A CPC is built only while no other runs, and the waiting is its
        // thread's, so that the server answers what else it is asked
        // meanwhile. The other CPCs may wait with it: std's lock on Linux lets
        // no run, step or peek past a build waiting for it.
        let (built, ready) = oneshot::channel::<Result<u64, String>>();
        let (orders, received) = mpsc::channel::<Order>();
        let thread = thread::Builder::new()
            .name(handle.clone())
            .spawn(move || {
                let standing = parts.build(machine).and_then(|mut standing| {
                    if let Some(state) = state {
                        let path = state.read.path.clone();
                        standing
                            .state_file_load(state)
                            .map_err(|problem| format!("{} {problem}", path.display()))?;
                    }
                    Ok(standing)
                });
                let mut standing = match standing {
                    Ok(standing) => standing,
                    Err(problem) => {
                        let _ = built.send(Err(problem));
                        return;
                    }
                };
                let _ = built.send(Ok(standing.tick()));
                for order in received {
                    order(&mut standing);
                }
            })
            .map_err(|error| {
                format!(
                    "cannot start a thread for the machine: {error}; closing an open machine with \
                     machine_close frees its thread"
                )
            })?;
        let tick = ready.await.unwrap_or_else(|_| {
            Err(format!(
                "{} could not be built; the server's standard error has the details",
                machine.name
            ))
        });
        let tick = match tick {
            Ok(tick) => tick,
            Err(problem) => {
                let _ = thread.join();
                return Err(problem);
            }
        };
        let answer = Opened {
            handle: handle.clone(),
            machine: machine.name.to_owned(),
            files,
            tick,
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

    /// List the open machines, by handle and name in the order they were opened, with the states kept from each, by name and tick in the order they were kept.
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

    /// Close a machine once its earlier requests have finished, and drop every state kept from it. Its handle and its states' names become invalid.
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

    /// Run a machine by `frames` or by `ticks`; give exactly one. Only `run`, `step`, `call` and `type` advance a machine, and the same requests on the same machine give the same answers.
    /// - A frame ends when the monitor's retrace sends the beam to the top-left corner, so a run by frames ends on a whole picture. A CPC's 6845 may never end a frame, so such a run stops after twice a standard frame's ticks for each frame asked.
    /// - A standard frame is 79872 ticks on a CPC (4 MHz) and 69888 on a Spectrum (3.5 MHz).
    /// - A run ends between instructions, a few ticks past the target, or at the first fetch of an interrupt handler if an interrupt is taken there. A run that ends inside a long chain of DD and FD prefixes answers `"settled": false`.
    /// - A run stops early, answering `"stopped": "halted"`, at a HALT with interrupts disabled, where the processor would wait forever. Unless `set` has written in an NMI, a run that starts on such a HALT runs as asked, halted throughout.
    /// - At most 1500 frames, or the ticks of 1500 standard frames, per request; run again to go further.
    /// - A run cannot be cancelled: if the client stops waiting, it still runs to its end.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn run(
        &self,
        Parameters(mut request): Parameters<RunRequest>,
    ) -> Result<Json<Ran>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let most_ticks = u64::from(MOST_FRAMES) * machine.standard_frame();
        let watched_for = watched_for(
            request.until.as_ref(),
            request.gather.as_ref(),
            request.count.as_ref(),
        )?;
        machine.watchable(&watched_for.conditions)?;
        kept_as_asked(
            request.gather.as_ref(),
            request.gather_last,
            request.count.as_ref(),
            request.count_by,
            request.path.as_ref(),
        )?;
        let keys = keys(machine, request.keys.as_ref())?;
        request.path = request
            .path
            .as_deref()
            .map(machines::absolute)
            .transpose()?;
        let mut arguments = journaled(&request);
        journal_conditions(&mut arguments, &watched_for);
        let gathering = request.gather.is_some();
        let counting = request.count.is_some();
        let (gather_last, count_by) = (request.gather_last, request.count_by);
        let path = request.path;
        let ran = match (request.frames, request.ticks) {
            (Some(frames), None) => {
                let frames = u32::try_from(frames)
                    .ok()
                    .filter(|frames| (1..=MOST_FRAMES).contains(frames))
                    .ok_or_else(|| {
                        format!(
                            "frames is {frames}; give 1 to {MOST_FRAMES}, and run again to go \
                             further"
                        )
                    })?;
                let limit = 2 * u64::from(frames) * machine.standard_frame();
                Server::ask(&request.handle, orders, move |built| {
                    let file = path.as_deref().map(NewFile::create).transpose()?;
                    let tick = built.tick();
                    let watching = watching(&watched_for, gather_last, count_by, file.is_some());
                    let (run, watched) = built.run(frames, limit, &watching, &keys);
                    let stopped = if run.frames >= frames {
                        Stopped::Frames
                    } else {
                        Stopped::Limit
                    };
                    let lists = Lists {
                        gathering,
                        counting,
                        file,
                    };
                    let ran = Ran::of(run, built, stopped).watched(&watched, &watched_for, lists);
                    built.record(tick, "run", arguments, None, wrote(&ran));
                    ran
                })
                .await??
            }
            (None, Some(ticks)) => {
                let ticks = u64::try_from(ticks)
                    .ok()
                    .filter(|ticks| (1..=most_ticks).contains(ticks))
                    .ok_or_else(|| {
                        format!(
                            "ticks is {ticks}; a {} runs from 1 to {most_ticks} ticks a request, \
                             the ticks of {MOST_FRAMES} standard frames, so run again to go \
                             further",
                            machine.name
                        )
                    })?;
                Server::ask(&request.handle, orders, move |built| {
                    let file = path.as_deref().map(NewFile::create).transpose()?;
                    let tick = built.tick();
                    let watching = watching(&watched_for, gather_last, count_by, file.is_some());
                    let (run, watched) = built.run(u32::MAX, ticks, &watching, &keys);
                    let lists = Lists {
                        gathering,
                        counting,
                        file,
                    };
                    let ran =
                        Ran::of(run, built, Stopped::Ticks).watched(&watched, &watched_for, lists);
                    built.record(tick, "run", arguments, None, wrote(&ran));
                    ran
                })
                .await??
            }
            _ => {
                return Err(
                    "give frames or ticks, exactly one: {\"frames\": 50} runs to the 50th \
                     retrace, {\"ticks\": 3993600} runs that many ticks"
                        .to_owned(),
                );
            }
        };
        Ok(Json(ran))
    }

    /// Call a routine the way the machine's own CALL would: push the program counter and jump to `address`. The call ends when the routine returns: the processor is back at the pushed address with the stack pointer where it was, and registers and memory are as the routine left them. The answer has the same form as `run`'s.
    /// - `values` are written first, as `set` writes them, to pass arguments; they may move the stack pointer.
    /// - Interrupts stay as they are. A run by frames leaves either machine's firmware inside its frame interrupt handler with IFF1 clear, so a routine called there runs without interrupts unless it enables them.
    /// - A processor halted on HALT is woken for the routine, and halted again if the routine returns within the call.
    /// - A routine that has not returned when the call stops (its frames run out, `until` matches, or a gather or count is full) is left running. A later run stops at its return only if `until` asks for the address the program counter held when it was called.
    /// - A routine that halts with interrupts disabled stops the call, answering `"stopped": "halted"`.
    /// - Refused, with the machine unchanged, inside a chain of prefixes, and where the pushed address would not read back: under a CPC's enabled ROM, or in a Spectrum's ROM.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn call(
        &self,
        Parameters(mut request): Parameters<CallRequest>,
    ) -> Result<Json<Ran>, String> {
        let address = address(&request.address)?;
        let frames = request.frames.unwrap_or(i64::from(MOST_FRAMES));
        let frames = u32::try_from(frames)
            .ok()
            .filter(|frames| (1..=MOST_FRAMES).contains(frames))
            .ok_or_else(|| {
                format!(
                    "frames is {frames}; give 1 to {MOST_FRAMES}, and run again to carry \
                     the routine further"
                )
            })?;
        let watched_for = watched_for(
            request.until.as_ref(),
            request.gather.as_ref(),
            request.count.as_ref(),
        )?;
        kept_as_asked(
            request.gather.as_ref(),
            request.gather_last,
            request.count.as_ref(),
            request.count_by,
            request.path.as_ref(),
        )?;
        request.path = request
            .path
            .as_deref()
            .map(machines::absolute)
            .transpose()?;
        let gathering = request.gather.is_some();
        let counting = request.count.is_some();
        let (gather_last, count_by) = (request.gather_last, request.count_by);
        let mut arguments = journaled(&request);
        arguments["address"] = json!(Hexadecimal::address(address));
        journal_conditions(&mut arguments, &watched_for);
        let values = request.values.unwrap_or_default();
        let path = request.path;
        let (machine, orders) = self.thread(&request.handle)?;
        machine.watchable(&watched_for.conditions)?;
        let limit = 2 * u64::from(frames) * machine.standard_frame();
        let ran = Server::ask(&request.handle, orders, move |built| {
            let file = path.as_deref().map(NewFile::create).transpose()?;
            let tick = built.tick();
            let watching = watching(&watched_for, gather_last, count_by, file.is_some());
            let (run, watched) = match built.call(address, &values, frames, limit, &watching) {
                Ok(called) => called,
                Err(problem) => {
                    if let Some(file) = file {
                        file.discard();
                    }
                    return Err(problem);
                }
            };
            let stopped = if run.frames >= frames {
                Stopped::Frames
            } else {
                Stopped::Limit
            };
            let lists = Lists {
                gathering,
                counting,
                file,
            };
            let ran = Ran::of(run, built, stopped).watched(&watched, &watched_for, lists);
            built.record(tick, "call", arguments, None, wrote(&ran));
            ran
        })
        .await??;
        Ok(Json(ran))
    }

    /// Run a machine by a number of instructions. Each instruction runs to its end and through the response to any interrupt taken there, up to the handler's first fetch, so a step can land in an interrupt handler. A processor halted on HALT idles on fetches that run nothing, and each counts as an instruction. The answer has the same form as `run`'s.
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
                    "instructions is {instructions}; give 1 to {MOST_INSTRUCTIONS}, and step again \
                     to go further"
                )
            })?;
        let (_, orders) = self.thread(&request.handle)?;
        let arguments = journaled(&request);
        let ran = Server::ask(&request.handle, orders, move |built| {
            let tick = built.tick();
            let run = built.step(instructions);
            built.record(tick, "step", arguments, None, None);
            Ran::of(run, built, Stopped::Instructions)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Type text at a machine the way the emulator's command line does with --type: each character's key is held down, with the shift it needs, for 3 standard frames, then released for 3 on a CPC or 6 on a Spectrum. The answer has the same form as `run`'s.
    /// - Type only once the firmware has started: the command line waits 78 frames on a CPC and 128 on a Spectrum.
    /// - Keys are pressed, not characters handed over, so the firmware decides what they mean. On a Spectrum a letter typed at the K cursor is a keyword: "p" at the start of a line is PRINT, and "j" is LOAD.
    /// - A newline is RETURN on a CPC and ENTER on a Spectrum. On a CPC a tab is TAB, \u001b is ESC and \b is DEL. Keys that type no character, such as COPY, the function keys and the joysticks, are held with `run`'s `keys`.
    /// - ASCII only: at most 250 characters on a CPC and 166 on a Spectrum. A text with a character no key types is refused before any key goes down.
    /// - Keys that `set` wrote into the keyboard stay held while typing.
    /// - A HALT with interrupts disabled does not end the typing.
    /// - A program that reads the keyboard only now and then loses keys pressed in between: type one character at a time, with runs between that stop where it reads the keyboard.
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
                ""
            } else {
                ", and type the rest in another request"
            };
            return Err(format!(
                "text has {characters} characters; give 1 to {most}, {MOST_FRAMES} frames' worth \
                 on a {}{rest}",
                machine.name
            ));
        }
        let keystrokes = machine.keystrokes(&request.text)?;
        let (held, released) = machine.typing();
        let arguments = journaled(&request);
        let ran = Server::ask(&request.handle, orders, move |built| {
            let tick = built.tick();
            let run = built.r#type(&keystrokes, held, released);
            built.record(tick, "type", arguments, None, None);
            Ran::of(run, built, Stopped::Typed)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Put a disc in a CPC's drive: a DSK image, original or extended layout, read whole and copied into the machine, as the emulator's command line does with --disc. The machine writes to its copy, never to the file, and what it wrote is lost when another disc replaces it, except in states kept while it was in the drive. The new disc replaces whatever the drive held, as when a game asks for its second side. Only a cpc664 or a cpc6128 takes discs: a cpc464 is built without the disc interface. An image the emulator cannot read is refused with the reason, and the drive keeps its disc. On a machine opened from a snapshot, AMSDOS reads no disc until `set` writes the disc controller's fields that `machine_open` lists. The answer gives the file's SHA-256 and the machine's tick at insertion.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn disc_insert(
        &self,
        Parameters(mut request): Parameters<DiscRequest>,
    ) -> Result<Json<Inserted>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        request.path = machines::absolute(&request.path)?;
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
            built.record(built.tick(), "disc_insert", arguments, Some(given), None);
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

    /// Put a .tap or .tzx tape in a Spectrum's deck and press PLAY; the file is read whole and copied into the machine, as the emulator's command line does with --tape. A Spectrum has no motor control, so the tape plays from now on whatever the machine does: insert it just before the program asks for it, as LOAD "" does. A tape that stops itself, as a multiload does between parts, waits there; no tool presses PLAY again. A tape is refused, and the deck keeps the tape it had, if it holds a block the emulator cannot play: a waveform recorded sample by sample, data built from a symbol table, a jump, loop, call or menu (which visit blocks out of order), or a block with an unknown ID. A CPC takes no tape. The answer gives the file's SHA-256 and the machine's tick at insertion.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn tape_insert(
        &self,
        Parameters(mut request): Parameters<TapeRequest>,
    ) -> Result<Json<Inserted>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        request.path = machines::absolute(&request.path)?;
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
            built.record(built.tick(), "tape_insert", arguments, Some(given), None);
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

    /// Write a machine to a new file as a snapshot, as the emulator's command line does with --save and `machine_open` reads: an SNA of version 1 for a CPC, a 48K .sna for a Spectrum. Refused if the file exists, inside a chain of prefixes, or on a Spectrum with a stack pointer below &4002. The file holds the whole RAM: a CPC's after its 256-byte header, by the RAM's own addresses as `peek`'s `ram` counts them; a Spectrum's &4000 to &FFFF after its 27-byte header. The answer gives the file's SHA-256 and the machine's tick. `state_keep` keeps all of the machine in memory, and `state_save` writes it to a file that only this build opens. A snapshot holds less than the machine:
    /// - Neither format records a HALT, so a machine saved on HALT runs past it once loaded.
    /// - A CPC snapshot holds no disc, no disc controller and no 6845 counters; see `machine_open`.
    /// - A Spectrum snapshot holds no beam position and no tape, and stores the program counter in the 2 bytes below the stack pointer, in the file only, so a machine loaded from it has lost what those bytes held.
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
        let path = machines::absolute(&request.path)?;
        let handle = request.handle.clone();
        let (bytes, tick) = Server::ask(&request.handle, orders, |built| {
            Ok::<_, String>((built.snapshot_save()?, built.tick()))
        })
        .await?
        .map_err(|problem| format!("{handle} {problem}"))?;
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

    /// Keep a machine exactly as it is, to return to with `state_restore`: everything it holds (mid-instruction if a run left it in a chain of prefixes), its picture, each disc as written, the tape's position, its tick and its journal. A state lives in the server's memory until `state_drop`, `machine_close` or the server stops; `state_save` writes it to a file. At most 64 states per machine. The answer gives the state's name.
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
                        "{handle} already has {MOST_STATES} states, the most a machine \
                         keeps; drop one with state_drop: {}",
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

    /// Write a kept state to a new file. `machine_open` with `state` builds a new machine from it, exactly as the state was: everything it holds, its picture, its discs as written, its tape's position, its tick and its journal. Refused if the file exists. The answer gives the file's SHA-256 and the state's tick.
    /// - Only the build that wrote the file opens it, the build `serverInfo.version` names, on firmware with the same SHA-256.
    /// - The file's first line is JSON: the build, the machine, the tick, the files the machine was built from, the journal, the machine's fields by `inspect`'s names, and its discs' or tape's fields by the names the emulator's headers give them. The machine's RAM, its picture and its disc or tape images follow, as bytes.
    /// - On another build, the journal's requests, made again in order from the same files, rebuild the machine.
    /// - A tape is found again by playing it from its start, so a state whose tape had played more than 268435456 pulses is refused when opened.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn state_save(
        &self,
        Parameters(request): Parameters<StateSaveRequest>,
    ) -> Result<Json<StateSaved>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        {
            let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
            threads.held_of(&request.handle, &request.state)?;
        }
        let path = machines::absolute(&request.path)?;
        let state = request.state.clone();
        let (bytes, tick) = Server::ask(&request.handle, orders, move |built| {
            built.state_file(&state, machine)
        })
        .await??;
        let written = Server::blocking(path.display().to_string(), move || {
            machines::write_new(&path, &bytes)
        })
        .await?;
        Ok(Json(StateSaved {
            handle: request.handle,
            state: request.state,
            tick,
            path: written.path,
            sha256: written.sha256,
        }))
    }

    /// Put a machine back exactly as a state kept from it was: everything it held, its picture, its discs as they were (even if another disc has been inserted or the program has written to them since), and its tape's position. Its tick and journal become the state's. A state can be restored only into the machine it was kept from. Restoring drops no state, this one included, so it can be restored again.
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
            pc: Hexadecimal::address(pc),
        }))
    }

    /// Drop a state kept from a machine, freeing its memory. Its name becomes invalid.
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

    /// List what has been done to a machine: the build running it, the files it was built from (by SHA-256), and every request that built it or changed it on the way to where it stands, in order. Each entry has the machine's tick when the request began and its arguments, with condition addresses, ports and bytes in & hexadecimal (a capture's `cpu.sp` or `cpu.pc` as given), paths absolute, and each file read or written named by its SHA-256.
    /// - Reads are not recorded, nor are states kept, saved or dropped. Restoring a state makes the journal the state's.
    /// - A machine built from a state file takes the state's journal and files: the files the state's machine was built from, as the file names them, not the state file.
    /// - The same requests in the same order, on the same build and with the same files, rebuild a machine in the same state: this is how a finding is reproduced.
    /// - Entries are returned from `from` on, as many as fit in 8192 characters, and at least one while any remain; `next` gives where the next page starts.
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
                        "from is {from}; give 0 to {count}, the number of entries in the journal \
                         of {handle}"
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

    /// Read what a machine holds by path, written as a C expression on the emulator's struct for the machine, `cpc_t` or `spectrum_t`: `cpu.pc`, `crtc.registers[12]`, `gate_array.inks`, `fdc.units[0].head`. Without a path, the whole machine. Fields keep their header names, numbers are JSON numbers, and an enum is given by its enumerator's name. Pointers are not followed; `peek` reads memory. Most fields are the chips' inner workings, the board's links and drives, or the emulator's own machinery for stepping the chips tick by tick. The fields a program works with most, and the monitor's beam, are these:
    /// - Both machines: the processor's `cpu.a`, `cpu.f`, `cpu.b`, `cpu.c`, `cpu.d`, `cpu.e`, `cpu.h`, `cpu.l`, `cpu.ixh`, `cpu.ixl`, `cpu.iyh`, `cpu.iyl`, `cpu.sp`, `cpu.pc`, `cpu.i`, `cpu.r`, `cpu.im`, `cpu.iff1`, `cpu.iff2` and `cpu.halted`, and its alternate set as pairs, `cpu.af_`, `cpu.bc_`, `cpu.de_` and `cpu.hl_`. The main set, IX and IY are read a byte at a time: HL is `cpu.h` and `cpu.l`. The keyboard's `keyboard.lines`, a set bit meaning a key released. Where the monitor's beam stands, `monitor.beam_x` and `monitor.beam_y`.
    /// - CPC: the 6845's `crtc.registers` and `crtc.address_register`, and its counters `crtc.c0` (characters), `crtc.c9` (the line within the row) and `crtc.c4` (rows). The Gate Array's `gate_array.pen`, `gate_array.inks` (the border at 16), `gate_array.mode`, `gate_array.lower_rom_enabled`, `gate_array.upper_rom_enabled` and `gate_array.r52`, its interrupt counter. `upper_rom_number`, the upper ROM selected, and `mmr`, a 6128's RAM configuration. `ppi.output` and `ppi.control`, `psg.registers` and `psg.selected`. The disc controller's `fdc.st0`, `fdc.st1`, `fdc.st2`, `fdc.c`, `fdc.h`, `fdc.r` and `fdc.n`, and what SPECIFY sets: `fdc.step_time`, `fdc.head_load_time` and `fdc.head_unload_time`, in microseconds, and `fdc.non_dma`.
    /// - Spectrum: the ULA's `ula.border`, `ula.speaker` and `ula.microphone`, and `ula.frame_tick`, the T-states since the frame's interrupt.
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

    /// Write what a machine holds, by `inspect`'s paths; every value is written, or none. An unsigned field takes a number, or hexadecimal written with &, #, $ or 0x; a signed field takes a number. A value of the wrong kind, or one the emulator could not run from, such as a `gate_array.pen` above 16, is refused and the machine left unchanged. Afterwards a CPC's memory map is worked out again, and a Spectrum's ULA is moved to the tick of the frame that `ula.frame_tick` names. The answer gives each path as it now reads. Writing the emulator's machinery rather than chip registers can leave a machine in a state no real hardware reaches, and what conditions see from there is not guaranteed.
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
            built.record(built.tick(), "set", arguments, None, None);
            Ok::<_, String>(written)
        })
        .await??;
        Ok(Json(Written { values: written }))
    }

    /// Read a machine's memory. With `address`, as the processor reads it, through the banking and the ROMs in front of the RAM: on a CPC with its lower ROM enabled, &0000 reads the ROM. With `ram`, a CPC's RAM itself, by its own addresses, whatever is paged in: &0000 is the RAM under that ROM, and a 6128's second 64K is &10000 to &1FFFF.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn peek(
        &self,
        Parameters(request): Parameters<PeekRequest>,
    ) -> Result<Json<Peeked>, String> {
        let length = request.length.unwrap_or(1);
        let (_, orders) = self.thread(&request.handle)?;
        let (address, ram, bytes) = match (&request.address, &request.ram) {
            (Some(given), None) => {
                let (start, address) = start(given)?;
                let length = match start {
                    START_ADDRESS => span(address, length)?,
                    _ => bytes_asked(length)?,
                };
                let (address, bytes) = Server::ask(&request.handle, orders, move |built| {
                    let address = built.start(start, address);
                    (address, built.peek(address, length))
                })
                .await?;
                (Some(Hexadecimal::address(address)), None, bytes)
            }
            (None, Some(given)) => {
                let ram = ram_address(given)?;
                let length = bytes_asked(length)?;
                let bytes = Server::ask(&request.handle, orders, move |built| {
                    built.peek_ram(ram, length)
                })
                .await??;
                (None, Some(Hexadecimal::ram(ram)), bytes)
            }
            _ => {
                return Err(
                    "give address or ram, exactly one: {\"address\": \"&C000\"} reads as the \
                     processor does, through the banking and the ROMs, and {\"ram\": \
                     \"&1C000\"} reads a CPC's RAM by its own addresses"
                        .to_owned(),
                );
            }
        };
        Ok(Json(Peeked {
            address,
            ram,
            bytes: digit_pairs(&bytes),
        }))
    }

    /// Search a machine's memory for bytes or for text, read as `peek` reads it with `address`: through the banking and the ROMs in front of the RAM. Matches may overlap.
    /// - `bytes`: an address is stored low byte first, so `"6F 14"` finds every absolute reference to &146F: CALL, JP and their conditional forms, LD, and tables of addresses. Data may hold the same bytes. A relative jump (JR, DJNZ) holds a displacement instead, and is not found this way.
    /// - `text`: a word or a string whose last character has bit 7 set, as both machines' ROMs end many of theirs, is found too.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn find(
        &self,
        Parameters(request): Parameters<FindRequest>,
    ) -> Result<Json<Found>, String> {
        let pattern = match (&request.bytes, &request.text) {
            (Some(bytes), None) => pattern(bytes)?,
            (None, Some(text)) => text_pattern(text)?,
            _ => {
                return Err(
                    "give bytes or text, exactly one: {\"bytes\": \"CD ?? 14\"} finds those \
                     bytes, ?? matching any byte, and {\"text\": \"READY\"} finds that text, bit \
                     7 ignored"
                        .to_owned(),
                );
            }
        };
        let first = request
            .address
            .as_ref()
            .map(address)
            .transpose()
            .map_err(|problem| format!("address {problem}"))?
            .unwrap_or(0);
        let last = request
            .to
            .as_ref()
            .map(address)
            .transpose()
            .map_err(|problem| format!("to {problem}"))?
            .unwrap_or(0xFFFF);
        if last < first {
            return Err(format!(
                "to {} is below address {}; give a to of {} or above",
                Hexadecimal::address(last),
                Hexadecimal::address(first),
                Hexadecimal::address(first)
            ));
        }
        let (_, orders) = self.thread(&request.handle)?;
        let found = Server::ask(&request.handle, orders, move |built| {
            built.find(first, last, &pattern)
        })
        .await?;
        Ok(Json(Found {
            addresses: found
                .iter()
                .take(MOST_FOUND)
                .map(|address| Hexadecimal::address(*address))
                .collect(),
            count: found.len(),
            next: found
                .get(MOST_FOUND)
                .map(|address| Hexadecimal::address(*address)),
        }))
    }

    /// Show what a machine's monitor shows, as a PNG next to the answer, cropped and drawn as the emulator's command line draws it with --screenshot. Each pixel's palette index is what the machine sent to the monitor: a CPC's hardware colour code, 0 to 31, or a Spectrum's colour, 0 to 15, with bits for brightness, green, red and blue from the highest bit. After a run by frames that ran all its frames the picture is whole. After anything else, the lines above the beam, and the part of the beam's line left of the beam, are from this frame, and the rest from the frame before; the answer's `whole` says whether the beam is partway through the picture. A machine that has not run yet shows an unpainted picture. The picture's size and crop:
    /// - CPC: 768 by 544, samples 208 to 975 of a line's 1024 and lines 34 to 305 of 312, each line drawn twice.
    /// - Spectrum: 352 by 264, samples 96 to 447 of 448 and lines 20 to 283 of 312.
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
            whole: shown.whole,
        };
        Ok(Pictured {
            answer,
            png: shown.picture.png(),
        })
    }

    /// Draw a CPC's memory as a picture, as colophon-player's <colophon-cpc-screen> does, so that a sprite sheet, a font or an off-screen buffer shows its shape. The RAM is read whatever ROM the processor sees over it. The PNG has 8 pixels across for each byte and 2 down for each line, the scale `monitor` uses for a CPC, and each pixel's palette index is its pen. It uses one mode and one palette and memory as it is now, so where a program changes the mode, the inks or memory partway down a frame, use `monitor`. Refused on a Spectrum. Whatever is not given is read from the machine, so a request with no options draws the memory the 6845 displays:
    /// - from the 6845: `base` from R12 and R13, `width` twice R1, `height` R6 times `rasters`, `rasters` R9 plus 1;
    /// - from the Gate Array: `mode`, the mode in force, and `palette`, its 16 inks.
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
            base: Hexadecimal::ram(screen.base),
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

    /// Disassemble a machine's memory from an address: each instruction's address, bytes and text, in Zilog's mnemonics with addresses, bytes and displacements in & hexadecimal and relative jump targets worked out. Memory is read as `peek` reads it, wrapping from &FFFF to &0000. A chain of DD and FD prefixes is one instruction, as `step` runs it. Undocumented instructions use colophon-player's names: IXH, IXL, SLL, RLC (IX+&05),B and the like, IN (C), OUT (C),0, IM 0/1, NOP for ED followed by a byte that starts no instruction, and BRK for ED FF. It does not tell code from data: bytes are decoded from wherever the listing starts. On a CPC, an instruction that jumps or calls to an entry of the firmware's jumpblocks has `firmware`, the entry's name in SOFT 968, Amstrad's firmware manual, such as "KM WAIT KEY" for `CALL &BB18`. The name goes with the address, whatever memory holds there now. The entries named, 3 bytes apart:
    /// - The main jumpblock: &BB00 to &BD5B on a cpc6128, to &BD58 on a cpc664, to &BD37 on a cpc464.
    /// - The high kernel jumpblock: &B900 to &B921, and &B92A on a cpc664 or a cpc6128.
    /// - The indirections: &BDCD to &BDF4 on a cpc664 or a cpc6128, to &BDF1 on a cpc464.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn disassemble(
        &self,
        Parameters(request): Parameters<DisassembleRequest>,
    ) -> Result<Json<Disassembled>, String> {
        let (start, address) = start(&request.address)?;
        let instructions = request.instructions.unwrap_or(16);
        let instructions = u16::try_from(instructions)
            .ok()
            .filter(|instructions| (1..=MOST_INSTRUCTIONS_LISTED).contains(instructions))
            .ok_or_else(|| {
                format!(
                    "instructions is {instructions}; give 1 to {MOST_INSTRUCTIONS_LISTED}, and \
                     disassemble again from where the listing ends to go further"
                )
            })?;
        let (machine, orders) = self.thread(&request.handle)?;
        let listed = Server::ask(&request.handle, orders, move |built| {
            let address = built.start(start, address);
            built.disassemble(address, instructions, usize::from(MOST_BYTES))
        })
        .await??;
        Ok(Json(Disassembled {
            instructions: listed
                .into_iter()
                .map(|(address, bytes, instruction)| Listed {
                    address: Hexadecimal::address(address),
                    bytes: digit_pairs(&bytes),
                    text: instruction.text,
                    firmware: instruction
                        .target
                        .and_then(|target| machine.firmware_entry(target)),
                })
                .collect(),
        }))
    }

    /// Write a machine's memory as its processor would: a write under a CPC's ROMs goes to the RAM beneath them, and a write into a Spectrum's ROM is lost.
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
        let mut arguments = journaled(&request);
        arguments["address"] = json!(Hexadecimal::address(address));
        Server::ask(&request.handle, orders, move |built| {
            built.poke(address, &bytes);
            built.record(built.tick(), "poke", arguments, None, None);
        })
        .await?;
        Ok(Json(Poked {
            address: Hexadecimal::address(address),
            length,
        }))
    }
}

#[tool_handler(router = router_without_formats(), meta = identified())]
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
                    "no tool is named {}; the tools are {}",
                    request.name,
                    tools.join(", ")
                ),
                None,
            ));
        }
        let call = ToolCallContext::new(self, request, context);
        Ok(match router.call(call).await? {
            CallToolResponse::Complete(mut result) => {
                result.meta = Some(identified());
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DI: u8 = 0xF3;
    const JR: u8 = 0x18;
    const LD_SP: u8 = 0x31;
    const LD_A_TO: u8 = 0x32;
    const PUSH_HL: u8 = 0xE5;

    // DI, LD SP,&9000, then PUSH HL round a JR, every write matching a range
    // of the whole of memory; LD (&8000),A writes once first, so the
    // 1048576th match is the first of a PUSH's two writes, the high byte
    // first (Z80 CPU User Manual, Zilog UM0080,
    // https://www.zilog.com/docs/z80/um0080.pdf), whose second is kept with
    // it.
    const PUSHING: [u8; 10] = [
        DI, LD_SP, 0x00, 0x90, LD_A_TO, 0x00, 0x80, PUSH_HL, JR, 0xFD,
    ];

    // A Spectrum whose ROM is PUSHING, gathering every write to a file for
    // 240 frames' ticks, in which it makes more writes than a gathering of
    // the first fills with, and fewer than twice as many.
    const TICKS: u64 = 240 * 69888;

    fn gathering_writes(gather_last: Option<bool>) -> (Run, Vec<Match>) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "colophon-mcp-server-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("a scratch directory");
        let mut rom = vec![0x76; 0x4000];
        rom[..PUSHING.len()].copy_from_slice(&PUSHING);
        fs::write(directory.join("spectrum48.rom"), rom).expect("a stand-in is written");
        let machine = machines::named("spectrum48").expect("the 48");
        let parts = machine.parts(&directory, None);
        let _ = fs::remove_dir_all(&directory);
        let mut built = parts
            .expect("its stand-in is read")
            .build(machine)
            .expect("a machine without a snapshot is always built");
        let gathering = json!([{ "write": 0, "to": "&FFFF" }]);
        let watched_for = watched_for(None, Some(&gathering), None).expect("a gathering");
        let watching = watching(&watched_for, gather_last, None, true);
        let (run, watched) = built.run(u32::MAX, TICKS, &watching, &[]);
        assert_eq!(watched.full, gather_last.is_none(), "{run:?}");
        let gathered = watched.matches.into_iter().filter(|seen| seen.gathers);
        (run, gathered.collect())
    }

    #[test]
    fn a_gathering_written_to_a_file_fills_at_its_1048576th_match() {
        let (run, gathered) = gathering_writes(None);
        assert!(run.ticks < TICKS, "{run:?}");
        assert_eq!(gathered.len(), 1_048_577);
        let [first, second] = &gathered[gathered.len() - 2..] else {
            unreachable!("two matches at least");
        };
        assert_eq!((first.pc, second.pc), (7, 7));
        assert_eq!(first.address, second.address.wrapping_add(1));
    }

    // Kept last, the gathering runs its ticks and keeps 1048576 matches,
    // which take up where a gathering of the first, filled, has them.
    #[test]
    fn a_gathering_of_the_last_written_to_a_file_keeps_the_last_1048576() {
        let (_, first) = gathering_writes(None);
        let (run, last) = gathering_writes(Some(true));
        assert!(run.ticks >= TICKS, "{run:?}");
        assert_eq!(last.len(), 1_048_576);
        let told = |seen: &Match| (seen.tick, seen.address, seen.value, seen.pc);
        let from = first
            .iter()
            .position(|seen| told(seen) == told(&last[0]))
            .expect("the two gatherings overlap");
        assert!(from > 0);
        assert!(
            first[from..]
                .iter()
                .zip(&last)
                .all(|(one, other)| told(one) == told(other))
        );
        assert!(last.last().unwrap().tick <= run.ticks);
    }
}
