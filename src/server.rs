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

use crate::machines::{self, MACHINES, Machine};

// Each is spoken by a test; those before 2026-07-28 open with initialize.
static REVISIONS: [ProtocolVersion; 3] = [
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];

// A machine holds raw pointers, a CPC's into itself, so it is not Send, and
// the server must be. Each is built on a thread of its own, and never leaves
// it.
struct MachineThread {
    handle: String,
    machine: &'static Machine,
    put_away: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl MachineThread {
    fn close(self) {
        drop(self.put_away);
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

#[derive(Deserialize, JsonSchema)]
struct OpenRequest {
    /// The machine, by the name the emulator's command line gives it.
    #[schemars(schema_with = "machine_names")]
    machine: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize, JsonSchema)]
struct CloseRequest {
    /// The handle machine_open gave the machine.
    handle: String,
}

#[derive(Serialize, JsonSchema)]
struct Standing {
    /// The handle the server gave the machine.
    handle: String,
    /// The machine, by the name the emulator's command line gives it.
    machine: String,
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

#[tool_router]
impl Server {
    pub fn new(roms: PathBuf) -> Server {
        Server {
            roms,
            threads: Mutex::default(),
        }
    }

    /// Build a machine, powered on and standing before its first instruction, and give it a handle. It stands until machine_close puts it away or the server stops, and this server never gives its handle to another machine.
    #[tool]
    fn machine_open(
        &self,
        Parameters(request): Parameters<OpenRequest>,
    ) -> Result<Json<Standing>, String> {
        let machine = machines::named(&request.machine).ok_or_else(|| {
            let names: Vec<&str> = MACHINES.iter().map(|machine| machine.name).collect();
            format!(
                "no machine is called {}; the machines are {}",
                request.machine,
                names.join(", ")
            )
        })?;
        let parts = machine.parts(&self.roms)?;
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        threads.opened += 1;
        let handle = format!("{}-{}", machine.name, threads.opened);
        let (built, ready) = mpsc::sync_channel::<()>(0);
        let (put_away, waiting) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name(handle.clone())
            .spawn(move || {
                let _machine = parts.build();
                let _ = built.send(());
                let _ = waiting.recv();
            })
            .map_err(|error| {
                format!(
                    "cannot start a thread for the machine: {error}; machine_close frees the \
                     thread of a machine standing"
                )
            })?;
        if ready.recv().is_err() {
            let _ = thread.join();
            return Err(format!(
                "{} could not be built; the server's error output says why",
                machine.name
            ));
        }
        let standing = MachineThread {
            handle,
            machine,
            put_away,
            thread,
        };
        let answer = standing.standing();
        threads.standing.push(standing);
        Ok(Json(answer))
    }

    /// The machines standing, each by its handle and its name, in the order they were opened.
    #[tool(input_schema = no_arguments())]
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

    /// Put a machine away. Its handle names nothing afterwards.
    #[tool]
    fn machine_close(
        &self,
        Parameters(request): Parameters<CloseRequest>,
    ) -> Result<Json<Closed>, String> {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(index) = threads
            .standing
            .iter()
            .position(|standing| standing.handle == request.handle)
        else {
            let handles: Vec<&str> = threads
                .standing
                .iter()
                .map(|standing| standing.handle.as_str())
                .collect();
            return Err(if handles.is_empty() {
                format!(
                    "no machine has the handle {}; no machine is standing, and machine_open \
                     stands one up",
                    request.handle
                )
            } else {
                format!(
                    "no machine has the handle {}; the machines standing have the handles {}",
                    request.handle,
                    handles.join(", ")
                )
            });
        };
        threads.standing.remove(index).close();
        Ok(Json(Closed {
            handle: request.handle,
        }))
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
