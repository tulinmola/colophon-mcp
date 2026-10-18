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

use crate::machine::Run;
use crate::machines::{self, Built, FileRead, MACHINES, Machine};

// Each is spoken by a test; those before 2026-07-28 open with initialize.
static REVISIONS: [ProtocolVersion; 3] = [
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];

// How far one call may run a machine: half a minute of either machine's time,
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

// An argument that may be left out is not one that may be null, and a type
// that admits null does not carry to every client.
fn absent_not_null(schema: &mut Schema) {
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

fn address_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "anyOf": [
            { "type": "integer", "minimum": 0, "maximum": 65535 },
            { "type": "string", "pattern": "^(&|#|\\$|0x|0X)[0-9A-Fa-f]{1,4}$" },
        ]
    })
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
        }
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

    /// Run a machine by its frames or by its ticks; give one of the two. Nothing but this tool and step moves a machine, and the same calls on the same machine give the same answers. A frame is the monitor's, and ends as its retrace sends the beam to the top-left corner, so a run by frames ends with a whole picture on the monitor. A CPC's frame is whatever its 6845 is programmed to make and may never come, so a run by frames is limited to twice a standard frame for each frame asked, a standard frame being 79872 ticks on a CPC, at 4 MHz, and 69888 on a Spectrum, at 3.5 MHz. A run by ticks runs that many, as the emulator's command line runs its frames. Either then goes on to the end of the instruction standing there, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a run ends between instructions, a few ticks past what was asked, unless it ends inside a chain of prefixes. One call runs at most 1500 frames, or the ticks of 1500 standard frames; run again to go further. A call is not taken back: one a client stops waiting for runs to its end all the same, and the machine's tick in the next answer says how far.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn run(&self, Parameters(request): Parameters<RunRequest>) -> Result<Json<Ran>, String> {
        let (machine, orders) = self.thread(&request.handle)?;
        let most_ticks = u64::from(MOST_FRAMES) * machine.standard_frame();
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
                    let run = built.run(frames, limit);
                    let stopped = if run.frames >= frames {
                        Stopped::Frames
                    } else {
                        Stopped::Limit
                    };
                    Ran::of(run, built, stopped)
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
                    let run = built.run(u32::MAX, ticks);
                    Ran::of(run, built, Stopped::Ticks)
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

    /// Step a machine by its instructions. Each instruction is run to its end, and through the response to an interrupt taken at its end, to the first fetch of its handler, so a step can land in an interrupt's handler. A processor halted on HALT idles on fetches that run nothing, and each of those is an instruction here. A chain of prefixes is no instruction until it ends, so a step caught in one long enough stops there, unsettled. One call runs at most 100000 instructions, and is not taken back, as a run is not. The answer is a run's.
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
