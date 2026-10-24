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

use crate::fields::hexadecimal_number;
use crate::keys::Key;
use crate::machine::{
    ANY_MEMORY, COUNT, Condition, EXECUTE, FIRST_ROM_LOCATION, GATHER, IN, Kind, List, Match,
    Memory, OUT, PSG, Place, RAM, READ, ROM, ROM_BYTES, Run, UNTIL, WRITE,
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
// A match written to a file takes a line of some 100 characters, so these
// come to some 100MB, and the room a run keeps for them to 32MB.
const MOST_GATHERED_TO_A_FILE: u32 = 1 << 20;
// A place counted takes some 60 characters, so these come to about twice what
// MOST_BYTES does.
const MOST_COUNTED: u32 = 256;
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
        .ok_or_else(|| format!("{value} is no address; {ADDRESS_FORMS}"))
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
        .ok_or_else(|| format!("{value} is no address in the RAM; {RAM_ADDRESS_FORMS}"))
}

const CONDITION_FORMS: &str = "a condition is {\"execute\": address}, {\"read\": address} or \
                               {\"write\": address}, each with \"to\" and the last address if it \
                               is a range, and an execute or a read with \"memory\": \"ram\" or \
                               \"rom\" if only the RAM's bytes or only a ROM's are to match; \
                               {\"in\": port, \"mask\": bits} or {\"out\": port, \"mask\": bits}, \
                               the mask being the bits of the port's address that its device \
                               decodes; or on a CPC {\"psg\": register}, with \"to\" and the last \
                               register if it is a range";

const REGISTER_FORMS: &str = "a register of the PSG is a number from 0 to 15";

// "The PSG has 16 internal registers" ("AY-3-8912 PSG", Kevin Thacker's
// cpctech,
// https://web.archive.org/web/20220626135348/https://cpctech.cpcwiki.de/docs/psg.html).
fn register(value: &Value) -> Result<u16, String> {
    value
        .as_u64()
        .filter(|number| *number <= 15)
        .and_then(|number| u16::try_from(number).ok())
        .ok_or_else(|| format!("{value} is no register; {REGISTER_FORMS}"))
}

// A condition, kept once in a tool's $defs, as until, gather and count all
// take lists of them.
struct ConditionForms;

impl JsonSchema for ConditionForms {
    fn schema_name() -> Cow<'static, str> {
        "Condition".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let address = generator.subschema_for::<AddressForms>().to_value();
        let register = json!({ "type": "integer", "minimum": 0, "maximum": 15 });
        let condition = |kind: &str, other: &str, schema: &Value, required: &[&str]| {
            json!({
                "type": "object",
                "properties": { kind: schema, other: schema },
                "required": required,
                "additionalProperties": false,
            })
        };
        let with_memory = |kind: &str| {
            let mut branch = condition(kind, "to", &address, &[kind]);
            let names: Vec<&str> = MEMORY_NAMES.iter().map(|(name, _)| *name).collect();
            branch["properties"]["memory"] = json!({ "enum": names });
            branch
        };
        let branches = [
            with_memory("execute"),
            with_memory("read"),
            condition("write", "to", &address, &["write"]),
            condition("in", "mask", &address, &["in", "mask"]),
            condition("out", "mask", &address, &["out", "mask"]),
            condition("psg", "to", &register, &["psg"]),
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

const MEMORY_NAMES: [(&str, Memory); 2] = [("ram", RAM), ("rom", ROM)];

const KIND_NAMES: [(&str, Kind); 6] = [
    ("execute", EXECUTE),
    ("read", READ),
    ("write", WRITE),
    ("in", IN),
    ("out", OUT),
    ("psg", PSG),
];

fn condition(value: &Value, list: List) -> Result<Condition, String> {
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
            None => format!("{value} names no kind"),
            Some((last, rest)) => format!(
                "{value} names {} and {last}, and a condition names one",
                rest.join(", ")
            ),
        });
    };
    let parse: fn(&Value) -> Result<u16, String> = if kind == PSG { register } else { address };
    let first = parse(&fields[name]).map_err(|problem| format!("{name} {problem}"))?;
    let port = kind == IN || kind == OUT;
    let other = if port { "mask" } else { "to" };
    let takes_memory = kind == EXECUTE || kind == READ;
    if let Some(unknown) = fields
        .keys()
        .find(|key| *key != name && *key != other && !(takes_memory && *key == "memory"))
    {
        return Err(format!("{value} has {unknown}, which {name} does not take"));
    }
    let memory = fields
        .get("memory")
        .map(|given| {
            MEMORY_NAMES
                .iter()
                .find(|(name, _)| given.as_str() == Some(name))
                .map(|(_, memory)| *memory)
                .ok_or_else(|| format!("memory {given} is neither \"ram\" nor \"rom\""))
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
            let shown = |at: u16| {
                if kind == PSG {
                    at.to_string()
                } else {
                    Hexadecimal::address(at).to_string()
                }
            };
            return Err(format!(
                "{value} runs back from {} to {}, and a range runs upward",
                shown(first),
                shown(last)
            ));
        }
        (last, 0)
    };
    Ok(Condition {
        kind,
        first,
        last,
        mask,
        memory,
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
            "path names the file gather and count write what they find to, and neither is \
             given; give gather or count the conditions to watch, or leave path out"
                .to_owned(),
        );
    }
    if gather.is_none() && gather_last.is_some() {
        return Err(
            "gather_last says which matches of gather are kept, and gather is not given; give \
             gather the conditions to gather, or leave gather_last out"
                .to_owned(),
        );
    }
    if count.is_none() && count_by.is_some() {
        return Err(
            "count_by says what count counts an access by, and count is not given; give count \
             the conditions to count, or leave count_by out"
                .to_owned(),
        );
    }
    Ok(())
}

fn watching(
    conditions: &[Condition],
    gather_last: Option<bool>,
    count_by: Option<CountBy>,
    filing: bool,
) -> Watching<'_> {
    let gathering = conditions.iter().any(|condition| condition.list == GATHER);
    Watching {
        conditions,
        most_gathered: if filing && gathering {
            MOST_GATHERED_TO_A_FILE
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
// form it was given in: its addresses after &, its registers by their
// numbers, and a range of one address or register as that one.
fn journal_conditions(arguments: &mut Value, conditions: &[Condition]) {
    let written = |condition: &Condition| {
        let (name, _) = KIND_NAMES
            .iter()
            .find(|(_, kind)| *kind == condition.kind)
            .expect("every kind is named");
        let written_as = |number: u16| {
            if condition.kind == PSG {
                json!(number)
            } else {
                json!(Hexadecimal::address(number))
            }
        };
        let mut fields = Map::new();
        fields.insert((*name).to_owned(), written_as(condition.first));
        if condition.kind == IN || condition.kind == OUT {
            fields.insert("mask".to_owned(), written_as(condition.mask));
        } else if condition.last != condition.first {
            fields.insert("to".to_owned(), written_as(condition.last));
        }
        if let Some((name, _)) = MEMORY_NAMES
            .iter()
            .find(|(_, memory)| *memory == condition.memory)
        {
            fields.insert("memory".to_owned(), json!(name));
        }
        Value::Object(fields)
    };
    for (field, list) in [("until", UNTIL), ("gather", GATHER), ("count", COUNT)] {
        if let Some(listed) = arguments.get_mut(field) {
            *listed = conditions
                .iter()
                .filter(|condition| condition.list == list)
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
    /// A snapshot to load into the machine once it is built: its path on the file system the server runs on, a relative one read from the server's working directory, which the answer and the journal name absolute.
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
    /// Conditions that stop the run at their first match. {"execute": address} stops before the instruction there begins; {"read": address} stops at the end of an instruction that reads the byte there as data, and {"write": address} at the end of one that writes it; each takes "to" and a last address for a range. {"in": port, "mask": bits} and {"out": port, "mask": bits} stop at the end of an instruction that reads or writes a port whose address agrees with the port on the bits of the mask, which are the bits its device decodes: {"out": "&7F00", "mask": "&C000"} is a CPC's Gate Array, and {"out": "&FE", "mask": "&01"} a Spectrum's ULA. {"execute": address} and {"read": address} also take "memory": "ram" or "rom", to match only an instruction or a byte the RAM holds, or only one a ROM holds: on a CPC, &0000 to &3FFF is the lower ROM's and &C000 to &FFFF an upper ROM's while the Gate Array enables each, and the RAM's while it does not; on a Spectrum, &0000 to &3FFF is the ROM's. On a CPC, {"psg": register} stops at the end of an instruction that writes one of the PSG's registers, numbered 0 to 15, and takes "to" and a last register for a range. The processor reaches the PSG through the PPI, and the emulator runs the PSG at each access to the PPI, by port C as the access leaves it, so while port C gives the PSG the function WRITE, each access to the PPI writes the selected register, an IN from the PPI and the PPI's bit set/reset written to its control port among them. An instruction's own bytes are never read as data. A run that begins where an execute condition matches runs that instruction first. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<Value>,
    /// Conditions whose matches are gathered as the run goes, written as until's are, the instruction the run begins on among them, which until never stops before. The run stops at the end of the instruction that makes the 128th match, or with path the 1048576th, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too; the next run gathers on from there. Not so with gather_last. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// Whether gather keeps the last 128 matches rather than the first, or with path the last 1048576: true runs on past the 128th, or the 1048576th, and lists those gathered last, in the order they happened, so that a run stopped by until, or at its end, says what led there. false if not given, and refused without gather.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather_last: Option<bool>,
    /// Conditions whose matches are counted as the run goes, by the place each matched: an instruction begun at an address, a byte read or written there, a port read or written, by the whole of its address, whatever bits its condition's mask decodes, a register of the PSG written, or with count_by "pc", an access by the instruction that made it. A place in memory is a byte of it, so that on a CPC the lower ROM's &0038, with rom saying so, and the RAM's are two places, and a byte the processor reaches at two addresses, as a 6128's RAM configurations can page one, is one place, told by the address of its first match; counted by pc, an access is counted by the byte of memory its instruction began at. Written as until's are, they count every instruction begun in a range with {"execute": "&4000", "to": "&BFFF"}. The run stops at the end of the instruction that counts at the 256th place, counting the rest of that instruction, and of the response to an interrupt taken at its end, too; the next run counts afresh from there. With path, a count never fills, and counts every place the run's frames or ticks reach. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
    /// What count counts an access by: "address", the byte's address, the port's or the PSG register's number, or "pc", where the instruction that made the access begins, so that a count of the writes over a screen says which instructions wrote it and how many times each did. An instruction begun is counted by its address either way, and by pc, an access made by the response to an interrupt, which no instruction made, is not counted. "address" if not given, and refused without count.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count_by: Option<CountBy>,
    /// Keys held down from the run's start to its end, on top of whatever the keyboard holds, which it holds again afterwards. Each is named as colophon-player names it, as the key is printed: on a CPC "SPACE", "Q", "RETURN", "COPY", "f5", "↑", or "(FIRE 2)" for joystick 0's main button, whose others are "(UP)", "(DOWN)", "(LEFT)", "(RIGHT)", "(FIRE 1)" and "(SPARE)", joystick 1 being wired to keys that carry digits and letters; on a Spectrum "CAPS", "SYM", "ENTER", "SPACE", "Q". A CPC's key may also be given by its number, 0 to 79, as the firmware numbers it, its line of the matrix times eight and its bit. An unknown key is refused with the names there are. A program that reads its keyboard only now and then can miss a key held for less than the time between its readings.
    #[schemars(schema_with = "keys_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<Vec<Value>>,
    /// A file to write what gather and count find to, in place of the answer's gathered and counted, for a client to read with tools of its own: a path on the file system the server runs on that names no file yet, a relative one from the server's working directory, which the answer and the journal name absolute. A path that names a file already, or a directory that is not there, is refused before the run begins. Each line of the file is a JSON object and ends in a newline: first each match gathered, in the order it happened, then each place counted, in counted's order, each as the answer would list it, after "list": "gather" or "count", the list it belongs to. The file stands at its path from before the run, empty, and holds what it is to hold once the answer names it by its SHA-256, so a client reads it after the answer. One that cannot be written whole after the run is taken away again, and the run, which is not taken back, is refused with where the machine stands, and journaled without the file. Refused without gather or count.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
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
    /// Conditions whose matches are gathered as the call goes, written as run's until takes them. The call stops at the end of the instruction that makes the 128th match, or with path the 1048576th, keeping that instruction's other matches, and those of the response to an interrupt taken at its end, too. Not so with gather_last. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather: Option<Value>,
    /// Whether gather keeps the last 128 matches rather than the first, or with path the last 1048576, as run's gather_last keeps them. false if not given, and refused without gather.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    gather_last: Option<bool>,
    /// Conditions whose matches are counted as the call goes, by the place each matched, as run's count counts them. The call stops at the end of the instruction that counts at the 256th place, counting the rest of that instruction, and of the response to an interrupt taken at its end, too. With path, a count never fills. At most 16.
    #[schemars(schema_with = "conditions_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<Value>,
    /// What count counts an access by, as run's count_by says. "address" if not given, and refused without count.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    count_by: Option<CountBy>,
    /// A file to write what gather and count find to, in place of the answer's gathered and counted, as run's path takes it. A path that names a file already, or a directory that is not there, is refused before the call begins, and a call refused takes the file made for it away again.
    #[schemars(transform = absent_not_null)]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
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

// What a count counts an access by.
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CountBy {
    /// The byte's address, the port's, or the PSG register's number.
    Address,
    /// Where the instruction that made the access begins.
    Pc,
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
    /// The disc image: its path on the file system the server runs on, a relative one read from the server's working directory, which the answer and the journal name absolute.
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
    /// The tape image: its path on the file system the server runs on, a relative one read from the server's working directory, which the answer and the journal name absolute.
    path: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// Where to write the snapshot: a path on the file system the server runs on that names no file yet, a relative one from the server's working directory, which the answer names absolute.
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
    /// Each value to write, by its path: {"cpu.pc": "&4000", "gate_array.inks[16]": 20}. A structure takes an object of some of its fields and an array an array of all its elements.
    #[schemars(schema_with = "values_schema")]
    values: Map<String, Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PeekRequest {
    /// The handle machine_open gave the machine.
    handle: String,
    /// The first address to read, as the processor reaches it, through the banking and the ROMs. Give this or ram.
    #[schemars(schema_with = "address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    address: Option<Value>,
    /// The first address to read in a CPC's RAM, by the RAM's own addresses, as screen's base counts them, whatever is paged in: a 6128's second 64K is &10000 to &1FFFF. Give this or address.
    #[schemars(schema_with = "ram_address_schema")]
    #[schemars(transform = absent_not_null)]
    #[serde(default)]
    ram: Option<Value>,
    /// How many bytes to read, from 1 to 4096, none of them past &FFFF, or for ram past the end of the machine's RAM; 1 if not given.
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
    /// Where the snapshot was written, an absolute path.
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
    /// The program counter, in hexadecimal after &: where the next instruction is fetched.
    pc: Hexadecimal,
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
    /// The gathering came to 128 matches, or to 1048576 written to a file, or the count to 256 places, and holds the rest of the instruction that filled it as well, and of the response to an interrupt taken at its end. A gathering of the last matches never fills, nor does a count written to a file.
    Full,
    /// The processor halted on HALT with interrupts disabled, where it waits for good: IFF1 shuts out INT, and nothing on either machine raises the NMI that would wake it. A run from here runs as asked, halted throughout, unless set has written in an NMI; call wakes the processor for its routine alone, and set or state_restore takes it past the HALT.
    Halted,
    /// The routine called returned.
    Returned,
}

#[derive(Serialize, JsonSchema)]
struct Matched {
    /// The machine's tick: as an instruction is about to begin, or once the tick that put an access on the bus has run.
    tick: u64,
    /// An instruction about to begin, by its address, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    execute: Option<Hexadecimal>,
    /// A byte the program read as data, by its address, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    read: Option<Hexadecimal>,
    /// A byte the processor wrote, by its address, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    write: Option<Hexadecimal>,
    /// A port the processor read, by its address, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    r#in: Option<Hexadecimal>,
    /// A port the processor wrote, by its address, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    out: Option<Hexadecimal>,
    /// One of a CPC's PSG registers written, by its number, from 0 to 15.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    psg: Option<u16>,
    /// On a CPC, the ROM that held the instruction or answered the read: "lower", or "upper" and its number, a number with no upper ROM fitted selecting ROM 0. Not given where the RAM held the byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rom: Option<String>,
    /// On a CPC, where the RAM holds the byte, by the RAM's own addresses as peek's ram counts them, where that is not the address the processor reached it at: a 6128's RAM configurations page in its second 64K's blocks, and its base 64K's &C000 to &FFFF at &4000. Not given where it is, or where a ROM held the byte; a write reaches the RAM beneath an enabled ROM.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    ram: Option<Hexadecimal>,
    /// The byte read or written, in hexadecimal after &; for a port read, the byte the processor took; for a register of the PSG, the byte the PPI's port A gave it, of which a register narrower than eight bits keeps only its low bits, as psg.registers then reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    value: Option<Hexadecimal>,
    /// Where the instruction that made the access begins, in hexadecimal after &; not given for an instruction about to begin, whose address is its own, or when the response to an interrupt made the access.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc: Option<Hexadecimal>,
    /// On a CPC, the ROM that held the instruction that made the access, as rom names it; not given where the RAM held it, or where pc is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_rom: Option<String>,
    /// On a CPC, where the RAM holds the instruction that made the access, as ram gives it, where that is not pc; not given where it is, where a ROM held it, or where pc is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_ram: Option<Hexadecimal>,
    /// The monitor's beam across its line, as monitor.beam_x counts it, in samples: a line's picture is monitor.width of them, and the count runs on past it until a sync ends the line.
    beam_x: u16,
    /// The monitor's beam down its frame, as monitor.beam_y counts it, in lines, of which monitor.height make a frame.
    beam_y: u16,
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
    fn of(seen: &Match, paged: bool) -> Matched {
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
            rom,
            ram,
            value: access.then_some(Hexadecimal::byte(seen.value)),
            pc: made.then_some(Hexadecimal::address(seen.pc)),
            pc_rom,
            pc_ram,
            beam_x: seen.beam_x,
            beam_y: seen.beam_y,
        }
    }
}

// A place counted, told by its first match.
#[derive(Serialize, JsonSchema)]
struct Counted {
    /// The machine's tick at the place's first match: as the instruction was about to begin, or once the tick that put the access on the bus had run.
    tick: u64,
    /// Where instructions began, in hexadecimal after &.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    execute: Option<Hexadecimal>,
    /// A byte the program read as data, by its address, in hexadecimal after &; counted by pc, the address of the place's first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    read: Option<Hexadecimal>,
    /// A byte the processor wrote, by its address, in hexadecimal after &; counted by pc, the address of the place's first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    write: Option<Hexadecimal>,
    /// A port the processor read, by its address, in hexadecimal after &; counted by pc, the port of the place's first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    r#in: Option<Hexadecimal>,
    /// A port the processor wrote, by its address, in hexadecimal after &; counted by pc, the port of the place's first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    out: Option<Hexadecimal>,
    /// One of a CPC's PSG registers written, by its number, from 0 to 15; counted by pc, the register of the place's first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    psg: Option<u16>,
    /// On a CPC, the ROM that held the instruction or answered the read, as a match's rom names it, which counted by address is part of the place; not given where the RAM held the byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    rom: Option<String>,
    /// On a CPC, where the RAM holds the byte, as a match's ram gives it, which counted by address is part of the place; not given where that is the address the processor reached it at, or where a ROM held the byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    ram: Option<Hexadecimal>,
    /// Where the instruction that made the place's first match begins, in hexadecimal after &, which counted by pc is the place itself; not given for instructions, whose address is their own, or when the response to an interrupt made the access.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc: Option<Hexadecimal>,
    /// On a CPC, the ROM that held the instruction at pc, as rom names it, which counted by pc is part of the place; not given where the RAM held it, or where pc is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_rom: Option<String>,
    /// On a CPC, where the RAM holds the instruction at pc, as ram gives it, which counted by pc is part of the place; not given where that is pc, where a ROM held it, or where pc is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    pc_ram: Option<Hexadecimal>,
    /// The instruction's bytes as the processor first fetched them there, as pairs of hexadecimal digits: its opcodes and its operands, so that code overwritten since still says what ran; of an instruction longer than four bytes, as only a chain of prefixes makes one, its first four. Given for instructions alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    bytes: Option<String>,
    /// How many times it matched.
    count: u32,
}

impl Counted {
    fn of((place, count): &(Place, u32), paged: bool) -> Counted {
        let first = Matched::of(&place.first, paged);
        let access = place.first.kind != EXECUTE;
        Counted {
            tick: first.tick,
            execute: first.execute,
            read: first.read,
            write: first.write,
            r#in: first.r#in,
            out: first.out,
            psg: first.psg,
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
    /// The ticks the machine has run since it was stood up, from power-on or from its snapshot.
    tick: u64,
    /// The program counter, in hexadecimal after &: where the next instruction is fetched.
    pc: Hexadecimal,
    /// Why the run ended.
    stopped: Stopped,
    /// What matched the conditions of until as the run stopped: the matching accesses of the instruction it stopped after, with those of an interrupt's response taken at its end, and the instruction it then stands before, if that matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    matched: Option<Vec<Matched>>,
    /// What matched the conditions of gather, in the order it happened: the first matches, or with gather_last the last 128.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    gathered: Option<Vec<Matched>>,
    /// Each place that matched the conditions of count, told by its first match, and how many times it matched: executes first, then reads, writes, port reads, port writes and the PSG's registers written, each by address or register, or by pc, and at one address the RAM's places first, by the RAM's own addresses, then the lower ROM's, then the upper ROMs' by their numbers. A byte of memory the processor reaches at two addresses is one place, told by the address of its first match.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    counted: Option<Vec<Counted>>,
    /// The file written at path, which holds what gathered and counted would have listed, in their place.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    written: Option<Filed>,
}

// The file a run wrote its lists to.
#[derive(Serialize, JsonSchema)]
struct Filed {
    #[serde(flatten)]
    file: FileRead,
    /// How many of its lines are matches of gather, which come first; given with gather.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    gathered: Option<usize>,
    /// How many of its lines are places of count, which follow; given with count.
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

fn write_lists(file: &mut NewFile, watched: &Watched) -> io::Result<()> {
    for seen in watched.matches.iter().filter(|seen| seen.gathers) {
        line(file, "gather", Matched::of(seen, watched.paged))?;
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
    fn watched(mut self, watched: &Watched, lists: Lists) -> Result<Ran, String> {
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
        let listed = |chosen: fn(&Match) -> bool| {
            watched
                .matches
                .iter()
                .filter(|seen| chosen(seen))
                .map(|seen| Matched::of(seen, watched.paged))
                .collect()
        };
        self.matched = watched.matched.then(|| listed(|seen| seen.stops));
        let Some(mut file) = lists.file else {
            self.gathered = lists.gathering.then(|| listed(|seen| seen.gathers));
            self.counted = lists.counting.then(|| {
                watched
                    .counts
                    .iter()
                    .map(|place| Counted::of(place, watched.paged))
                    .collect()
            });
            return Ok(self);
        };
        let written = write_lists(&mut file, watched);
        let file = file.finish(written).map_err(|problem| {
            format!(
                "{problem}. The request is not taken back: it ran {} ticks, and the machine stands \
                 where it ended, at tick {} with its program counter at {}, and its journal holds \
                 the request; make it again, from a state kept before it or from the journal's \
                 requests before it, to write the file, at a path the file system can take it at",
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
    /// The first address read, as the processor reaches it, in hexadecimal after &; given when address was.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = absent_not_null)]
    address: Option<Hexadecimal>,
    /// The first address read in the RAM, in hexadecimal after &; given when ram was.
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
    /// Whether the beam stands outside the picture, at or before its first pixel or past its last, as it does after a run by frames that ran all its frames: false while the beam is partway through it, where the lines above the beam, and its own line to its left, are of the frame it is painting and the rest of the passes before. A line that a frame shorter than the picture never reaches keeps what an earlier pass left there, whatever whole says.
    whole: bool,
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
    /// The address the first byte was read from, in the RAM, in hexadecimal after &.
    base: Hexadecimal,
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
    /// Where the instruction begins, in hexadecimal after &.
    address: Hexadecimal,
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

fn bytes_asked(length: i64) -> Result<u16, String> {
    u16::try_from(length)
        .ok()
        .filter(|length| (1..=MOST_BYTES).contains(length))
        .ok_or_else(|| format!("{length} bytes is not from 1 to {MOST_BYTES}"))
}

// A span of memory that ends at &FFFF or before it.
fn span(address: u16, length: i64) -> Result<u16, String> {
    let length = bytes_asked(length)?;
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

    /// Build a machine, powered on and standing before its first instruction, or standing where a snapshot was taken, and give it a handle. It stands until machine_close puts it away or the server stops, and this server never gives its handle to another machine. The answer names every file the machine was built from by its SHA-256. A snapshot is loaded as the emulator's command line loads one, into the machine named, after its firmware is fitted. A CPC's is an SNA of version 1, 2 or 3, of which only the fields of version 1 are read: none keeps the disc controller, which the machine finds as at power-on, so one taken inside a disc access does not go on with it, and AMSDOS, which sets the controller up with SPECIFY as the machine starts, reads no disc from there, until set writes fdc.non_dma back as SPECIFY left it; and the 6845's counters a version 3 carries are not read, so the machine starts its frame from the top, with the 6845's counters, the Gate Array's interrupt counter and the monitor's beam all at 0. Its first run by frames therefore ends early, at the retrace the 6845's first vertical sync brings, some 62000 ticks in on the firmware's screen; until then monitor.beam_y runs up to three lines past the 6845's line, and from then on, on the firmware's screen, 70 or 71 past it, the border above the picture. A Spectrum's is the 48K .sna, which keeps no beam position either. The machine's tick starts at 0 from either.
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
        let snapshot = request
            .snapshot
            .as_deref()
            .map(machines::absolute)
            .transpose()?;
        let roms = self.roms.clone();
        let parts = Server::blocking(
            format!("the files {} is built from", machine.name),
            move || machine.parts(&roms, snapshot.as_deref()),
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

    /// Run a machine by its frames or by its ticks; give one of the two. Nothing but this tool, step, call and type moves a machine, and state_restore puts one back where it stood; the same requests of the same machine give the same answers. A frame is the monitor's, and ends as its retrace sends the beam to the top-left corner, so a run by frames ends with a whole picture on the monitor. A CPC's frame is whatever its 6845 is programmed to make and may never come, so a run by frames is limited to twice a standard frame for each frame asked, a standard frame being 79872 ticks on a CPC, at 4 MHz, and 69888 on a Spectrum, at 3.5 MHz. A run by ticks runs that many, as the emulator's command line runs its frames. Either then goes on to the end of the instruction standing there, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a run ends between instructions, a few ticks past what was asked, unless it ends inside a chain of prefixes. A run stops sooner at the end of a HALT that leaves interrupts disabled, where the processor waits for good, since neither machine raises the NMI that would wake it, and says so; one that begins there runs as asked, unless set has written in an NMI, which wakes it. One request runs at most 1500 frames, or the ticks of 1500 standard frames; run again to go further. A request is not taken back: one a client stops waiting for runs to its end all the same, and the machine's tick in the next answer says how far. until stops a run sooner, where a condition matches, gather lists what matches its conditions on the way, and count counts how many times each place matches its conditions, by its address or by the instruction that made it, and tells each by its first match; each match gathered says its tick, what it was and where, the byte, the instruction that made it and where the monitor's beam stood, and on a CPC which ROM held the instruction or the byte, or where in its RAM, when the address alone does not say. With path, what gather and count find is written to a new file instead, a line for each match and each place, so that one run lists what a program does over many frames, a trace of each instruction it runs or its writes to the PSG, up to 1048576 matches, and counts, over every frame it runs, every byte it reads or writes. keys holds keys down through the run, as a player holds a direction and fire for as long as a stretch of the game wants them.
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
        let conditions = watched_for(
            request.until.as_ref(),
            request.gather.as_ref(),
            request.count.as_ref(),
        )?;
        machine.watchable(&conditions)?;
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
        journal_conditions(&mut arguments, &conditions);
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
                            "frames is {frames}, and a run takes from 1 to {MOST_FRAMES} frames; \
                             run again to go further"
                        )
                    })?;
                let limit = 2 * u64::from(frames) * machine.standard_frame();
                Server::ask(&request.handle, orders, move |built| {
                    let file = path.as_deref().map(NewFile::create).transpose()?;
                    let tick = built.tick();
                    let watching = watching(&conditions, gather_last, count_by, file.is_some());
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
                    let ran = Ran::of(run, built, stopped).watched(&watched, lists);
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
                            "ticks is {ticks}, and a run on a {} takes from 1 to {most_ticks} \
                             ticks, the ticks of {MOST_FRAMES} standard frames; run again to go \
                             further",
                            machine.name
                        )
                    })?;
                Server::ask(&request.handle, orders, move |built| {
                    let file = path.as_deref().map(NewFile::create).transpose()?;
                    let tick = built.tick();
                    let watching = watching(&conditions, gather_last, count_by, file.is_some());
                    let (run, watched) = built.run(u32::MAX, ticks, &watching, &keys);
                    let lists = Lists {
                        gathering,
                        counting,
                        file,
                    };
                    let ran = Ran::of(run, built, Stopped::Ticks).watched(&watched, lists);
                    built.record(tick, "run", arguments, None, wrote(&ran));
                    ran
                })
                .await??
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

    /// Call a routine of the machine's as its own CALL would: the program counter is pushed where the stack pointer stands, and the processor jumps to the address. The call ends when the processor is back at the address pushed with the stack pointer where it stood, which is where the routine's own return leaves it, so a routine that returns leaves the machine standing where it was, with its registers and memory as the routine left them, for inspect and peek to read. The values given are written first, as set writes them, so they can hand the routine its arguments, and they may move the stack pointer. Interrupts stay as they stand, so their handlers run within the call only if IFF1 is set: a run by frames leaves either machine's firmware inside the handler of the frame's interrupt, with IFF1 clear, and a routine called there runs with no interrupt at all, unless it enables them itself. A processor halted on HALT is woken for the routine, and halted again if the routine returns within the call. A routine that has not returned when the call stops, at its frames, at a condition of until or with its gathering or its count full, is left running and awake; a later run carries it on and past its return, and stops there only if until asks for the address the call was made from. A routine that halts on HALT with interrupts disabled stops the call there, halted, as it stops a run: one that waits on HALT for the next frame, called with IFF1 clear, as inside an interrupt's handler or after DI, waits for good. Refused, with the machine as it stood, inside a chain of prefixes, or where the address pushed would not read back. The answer is a run's.
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
                    "frames is {frames}, and a call waits from 1 to {MOST_FRAMES} frames; run \
                     again to carry the routine further"
                )
            })?;
        let conditions = watched_for(
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
        journal_conditions(&mut arguments, &conditions);
        let values = request.values.unwrap_or_default();
        let path = request.path;
        let (machine, orders) = self.thread(&request.handle)?;
        machine.watchable(&conditions)?;
        let limit = 2 * u64::from(frames) * machine.standard_frame();
        let ran = Server::ask(&request.handle, orders, move |built| {
            let file = path.as_deref().map(NewFile::create).transpose()?;
            let tick = built.tick();
            let watching = watching(&conditions, gather_last, count_by, file.is_some());
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
            let ran = Ran::of(run, built, stopped).watched(&watched, lists);
            built.record(tick, "call", arguments, None, wrote(&ran));
            ran
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
            built.record(tick, "step", arguments, None, None);
            Ran::of(run, built, Stopped::Instructions)
        })
        .await?;
        Ok(Json(ran))
    }

    /// Type text at a machine as the emulator's command line types it with --type: each character's key held down, with the shift it needs, for 3 standard frames, then let go for 3 on a CPC and 6 on a Spectrum, long enough for each machine's firmware to see a key go down and come up, and to take the same key twice. Type at a machine once its firmware has started, as it takes no key before: the command line waits 78 frames for a CPC's and 128 for a Spectrum's, whose ROM takes none before frame 85. Keys are pressed, not characters handed over, so what arrives is what the firmware makes of them: on a Spectrum a letter typed where the cursor shows K is a keyword, so "p" at the start of a line is PRINT, and "j" then two of "\"" is LOAD "". Keys the keyboard holds already, as set writes them, stay held through the typing, where the command line lets every key go. A newline is RETURN on a CPC and ENTER on a Spectrum; on a CPC a tab is TAB, an escape, \u001b, ESC, and a backspace, \b, DEL. Keys that type no character, such as COPY, the function keys and the joysticks, and a character outside ASCII, such as a CPC's £, SHIFT with ^, are held by run's keys. A text is ASCII, and one holding a character no key types, alone or with a shift, is refused before a key goes down. A program reading the keyboard only now and then loses the keys pressed between, so type at it a character at a time, with runs between that stop where it reads the keyboard. At most 250 characters on a CPC and 166 on a Spectrum, 1500 frames' worth. Each key goes down and comes up between instructions, a few ticks at most from where the command line puts it, counting from where the typing began, and the typing ends at the end of the instruction standing as its frames' ticks run out, so the answer is a run's that took those ticks and a few more. A stretch that ends inside a chain of prefixes ends the typing there, unsettled, as it ends a run; a HALT that leaves interrupts disabled does not, and the typing takes its ticks whatever the program does.
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
            built.record(tick, "type", arguments, None, None);
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

    /// Put a tape in a Spectrum's deck and press PLAY: a .tap, bare blocks at the ROM's own timings, or a .tzx, read whole from its path on the file system the server runs on and copied into the machine, as the emulator's command line gives a tape with --tape. A Spectrum has no motor line, so the tape plays from its start as the machine runs, whatever the machine does: put it in just before the program asks for it, as LOAD "" does, since a header's pilot tone lasts 5 seconds and the ROM listens for only part of it. A tape stops where a block marks it to stop, as a multiload's does between its parts, and nothing here presses PLAY again, so such a tape loads its first part and waits, as it does on the command line. Blocks the emulator's reader cannot play, a waveform recorded sample by sample, data built from a table of symbols, blocks visited out of order, or a block whose ID it does not know, are refused as the tape goes in, and the deck keeps the tape it had. The tape takes the place of whatever tape the deck held. A CPC is given no tape here: the emulator has never loaded one through a CPC's firmware, so nothing could hold such a loading to what the machine did. The answer names the file by its SHA-256, with the machine's tick as the tape went in.
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

    /// Write a machine to a new file as a snapshot, in the format the emulator's command line writes with --save and machine_open reads: an SNA of version 1 for a CPC, the 48K .sna for a Spectrum. The file is written only where none is, at its path on the file system the server runs on, from the server's working directory if relative, so that nothing is ever written over: a path that names a file already is refused. A snapshot keeps less than the machine. Neither keeps whether the processor is halted, so a machine saved on HALT, waiting for an interrupt, runs on past the HALT as soon as it is read. A CPC's keeps no counters of the 6845's, so the machine read from it starts its frame from the top, and neither the discs nor the disc controller, which the machine read from it finds as at power-on, so one saved inside a disc access does not go on with it, and AMSDOS, which sets the controller up with SPECIFY as the machine starts, reads no disc from there until set writes fdc.non_dma back as SPECIFY left it. A Spectrum's keeps no beam position and no tape, and keeps the program counter in the two bytes below the stack pointer, in the file and not in the machine, so a machine read from it has lost what those two bytes held; a Spectrum whose stack pointer is below &4002, 0 among them, is refused. Refused inside an instruction, as a run or a step ending in a chain of prefixes leaves the machine. The machine is left as it stands. The file holds the machine's RAM whole, to be read as an image of it: a CPC's after its header of 256 bytes, all 64K or 128K of it by the RAM's own addresses, as peek's ram counts them, and a Spectrum's after its header of 27, &4000 to &FFFF, save the two bytes below the stack pointer, which hold the program counter. The answer names the file by its SHA-256, with the machine's tick.
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
            pc: Hexadecimal::address(pc),
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

    /// What has been done to a machine, on the branch it stands on: the build that runs it, the files it was built from, each named by its SHA-256, and every request that stood it up or changed it, in the order made, the first of them the machine_open that stood it up, each with the machine's tick as it began and the arguments it was given, save that an address or a port is written in hexadecimal after &, a register of the PSG as a number, a range of one address or register as that one, and a path absolute, while the values set and call write are kept as given, and a snapshot, a disc or a tape read named by its SHA-256 as well, and a file a run or a call wrote named by what it holds. Reads change nothing and are not entered, nor are states kept or let go; restoring a state makes the journal the state's. The same requests made in this order, on the same build and with the same files, make a machine that stands as this one stands, and write files that hold what these hold, each at a path that names nothing yet, which is how a finding is made again. Entries are answered from the one asked for, 0 if none is, as many as fit in 8192 characters, and at least one while any remain, however long; next says where the following page begins.
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

    /// Write what a machine holds, by the paths inspect reads. A field of unsigned integers takes a number, or hexadecimal after &, #, $ or 0x, as an address does; a signed field takes a number. Every value is written, or none: a value of the wrong kind, or one the emulator could not run from, such as a pen past the seventeen the Gate Array has, is refused, and the machine is left as it stood. A CPC's memory map is worked out again afterwards, as it is when its own processor writes the registers behind it, and a Spectrum's ULA stood at the tick of the frame its frame_tick names, from which its beam is reckoned. The answer gives each path as it now reads. Writing the emulator's machinery, rather than the chips' registers, can leave a machine in a state no silicon could reach.
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

    /// Read a machine's memory. By address, as its processor would, through the banking and the ROMs standing in front of the RAM: on a CPC with its lower ROM enabled, &0000 reads the ROM. By ram, a CPC's RAM itself, by the RAM's own addresses, as screen's base counts them, whatever is paged in: &0000 is the RAM under that ROM, and a 6128's second 64K is &10000 to &1FFFF. From 1 to 4096 bytes, none past &FFFF, or past the end of the RAM.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn peek(
        &self,
        Parameters(request): Parameters<PeekRequest>,
    ) -> Result<Json<Peeked>, String> {
        let length = request.length.unwrap_or(1);
        let (_, orders) = self.thread(&request.handle)?;
        let (address, ram, bytes) = match (&request.address, &request.ram) {
            (Some(given), None) => {
                let address = address(given)?;
                let length = span(address, length)?;
                let bytes = Server::ask(&request.handle, orders, move |built| {
                    built.peek(address, length)
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
                    "peek reads from address or from ram, one of the two: {\"address\": \
                     \"&C000\"} reads as the processor does, through the banking and the ROMs, \
                     and {\"ram\": \"&1C000\"} reads a CPC's RAM by its own addresses"
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

    /// Show what a machine's monitor shows, as a PNG beside the answer: its picture as the emulator's command line crops and draws its screenshots, which a picture written there with --screenshot matches pixel for pixel. A CPC's is 768 by 544 pixels, a sample of the raster to each across and every line drawn twice, cut from samples 208 to 975 of the 1024 a line has and lines 34 to 305 of 312; a Spectrum's is 352 by 264, from samples 96 to 447 of 448 and lines 20 to 283 of 312. Each pixel's index in the PNG's palette is what the machine put on the cable: a CPC's hardware colour code, 0 to 31, or a Spectrum's sample, 0 to 15, its bits brightness, green, red and blue from the highest. The beam paints the picture as the machine runs, so it is whole after a run by frames that ran all its frames, which ends as the retrace sends the beam to the top-left corner; after any other run, a step, a call or typing, the lines above the beam, and its own line to its left, are this frame's, and the rest are the frame's before, and the answer's whole says whether the beam is partway through the picture. A machine not yet run shows a picture never painted.
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
                    address: Hexadecimal::address(address),
                    bytes: digit_pairs(&bytes),
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
        let conditions = watched_for(None, Some(&gathering), None).expect("a gathering");
        let watching = watching(&conditions, gather_last, None, true);
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
