// The server as a client meets it: the built binary, spoken to over its
// standard input and output in raw lines, and graded against the
// specification's own sentences, quoted where they are used. Revision
// 2026-07-28 is at https://modelcontextprotocol.io/specification/2026-07-28,
// and 2025-11-25 and 2025-06-18 beside it; the text quoted was read in the
// specification repository at tag 2026-07-28.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use colophon_mcp::machines::MACHINES;
use serde_json::{Value, json};

const MODERN: &str = "2026-07-28";
const LEGACY: [&str; 2] = ["2025-06-18", "2025-11-25"];
const TOOLS: [&str; 3] = ["machine_close", "machine_list", "machine_open"];
// Long past anything the server does, so that a server that has stopped
// answering fails a test rather than hanging it.
const PATIENCE: Duration = Duration::from_secs(10);

// Stand-ins for the firmware, at the sizes the machines read.
struct Roms(PathBuf);

impl Roms {
    fn new(files: &[(&str, usize)]) -> Roms {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "colophon-mcp-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("a scratch directory");
        for &(file, size) in files {
            fs::write(path.join(file), vec![0; size]).expect("a stand-in is written");
        }
        Roms(path)
    }

    fn all() -> Roms {
        Roms::new(&[
            ("cpc6128.rom", 0x8000),
            ("cpc664.rom", 0x8000),
            ("cpc464.rom", 0x8000),
            ("amsdos.rom", 0x4000),
            ("spectrum48.rom", 0x4000),
        ])
    }
}

impl Drop for Roms {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn server() -> Command {
    Command::new(env!("CARGO_BIN_EXE_colophon-mcp"))
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    read: Vec<String>,
    next_id: u64,
    // The revision each request is made in; None for a legacy session,
    // whose revision was settled by initialize.
    revision: Option<&'static str>,
}

impl Session {
    fn start(roms: &Roms) -> Session {
        let mut child = server()
            .arg("--roms")
            .arg(&roms.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the server starts");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("its output"));
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            stdin,
            lines,
            read: Vec::new(),
            next_id: 0,
            revision: Some(MODERN),
        }
    }

    // A session opened with initialize, in a revision before 2026-07-28.
    fn legacy(roms: &Roms, revision: &str) -> (Session, Value) {
        let mut session = Session::start(roms);
        session.revision = None;
        let answer = session.request(
            "initialize",
            json!({
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": { "name": "colophon-mcp-tests", "version": "0" },
            }),
        );
        session.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        (session, answer)
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("the input is open");
        writeln!(stdin, "{message}").expect("the server reads its input");
        stdin.flush().expect("the line reaches the server");
    }

    fn receive(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(PATIENCE)
            .expect("the server answers in time");
        self.read.push(line.clone());
        serde_json::from_str(&line).expect("every line is JSON")
    }

    fn request(&mut self, method: &str, mut params: Value) -> Value {
        self.next_id += 1;
        if let Some(revision) = self.revision {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": revision,
                "io.modelcontextprotocol/clientCapabilities": {},
            });
        }
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let response = self.receive();
        assert_eq!(response["id"], id, "{response}");
        response
    }

    fn result(&mut self, method: &str, params: Value) -> Value {
        let response = self.request(method, params);
        assert!(response["error"].is_null(), "{response}");
        response["result"].clone()
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.result(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
    }

    fn answer(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], false, "{result}");
        result["structuredContent"].clone()
    }

    fn refusal(&mut self, tool: &str, arguments: Value) -> String {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], true, "{result}");
        result["content"][0]["text"]
            .as_str()
            .expect("the refusal is told as text")
            .to_owned()
    }

    fn open(&mut self, machine: &str) -> String {
        let standing = self.answer("machine_open", json!({ "machine": machine }));
        assert_eq!(standing["machine"], machine, "{standing}");
        standing["handle"].as_str().expect("a handle").to_owned()
    }

    fn handles(&mut self) -> Vec<String> {
        let listing = self.answer("machine_list", json!({}));
        listing["machines"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|standing| standing["handle"].as_str().expect("a handle").to_owned())
            .collect()
    }

    // Closes the server's input and waits for it to stop, returning how it
    // ended, every line it wrote and what it said on its error output.
    fn finish(mut self) -> (ExitStatus, Vec<String>, String) {
        drop(self.stdin.take());
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the server is waited for") {
                break status;
            }
            assert!(
                started.elapsed() < PATIENCE,
                "the server outlives its input"
            );
            thread::sleep(Duration::from_millis(10));
        };
        while let Ok(line) = self.lines.recv_timeout(PATIENCE) {
            self.read.push(line);
        }
        let mut errors = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr
                .read_to_string(&mut errors)
                .expect("its error output is read");
        }
        (status, std::mem::take(&mut self.read), errors)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Run as build_name runs it, without the variables a hook or `rebase --exec`
// sets, which would send it to the repository the hook runs in.
fn git_head(repository: &str) -> String {
    let variables = Command::new("git")
        .args(["rev-parse", "--local-env-vars"])
        .output()
        .expect("git runs");
    let mut git = Command::new("git");
    for variable in String::from_utf8_lossy(&variables.stdout).lines() {
        git.env_remove(variable);
    }
    let output = git
        .args(["-C", repository, "rev-parse", "HEAD"])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("a commit id")
        .trim()
        .to_owned()
}

// "Servers MUST implement server/discover." The server's name and version
// are its "serverInfo", which "Servers SHOULD include".
#[test]
fn discovery_names_the_server_its_build_and_the_revisions_it_speaks() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let discovered = session.result("server/discover", json!({}));
    assert_eq!(
        discovered["supportedVersions"],
        json!(["2025-06-18", "2025-11-25", "2026-07-28"])
    );
    let identity = &discovered["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(identity["name"], "colophon-mcp");
    assert_eq!(identity["version"], env!("COLOPHON_MCP_BUILD"));
}

// Whether the files were committed is graded in build_name's own tests;
// here, that the commits are the ones the checkouts stand at.
#[test]
fn the_build_is_named_for_the_commits_it_was_compiled_from() {
    let build = env!("COLOPHON_MCP_BUILD");
    let mcp = git_head(env!("CARGO_MANIFEST_DIR"));
    let emulator = git_head(env!("COLOPHON_MCP_EMULATOR_DIR"));
    let pattern = format!("0.0.0+mcp.{mcp}");
    assert!(build.starts_with(&pattern), "{build}");
    assert!(build.contains(&format!(".emulator.{emulator}")), "{build}");
}

// "If the server supports the requested protocol version, it MUST respond
// with the same version." (2025-06-18 and 2025-11-25, lifecycle)
#[test]
fn initialize_is_answered_in_the_revision_asked_for() {
    let roms = Roms::all();
    for revision in LEGACY {
        let (_session, answer) = Session::legacy(&roms, revision);
        let result = &answer["result"];
        assert_eq!(result["protocolVersion"], revision, "{answer}");
        assert_eq!(result["serverInfo"]["name"], "colophon-mcp", "{answer}");
        assert_eq!(
            result["serverInfo"]["version"],
            env!("COLOPHON_MCP_BUILD"),
            "{answer}"
        );
    }
}

// "Otherwise, the server MUST respond with another protocol version it
// supports. This SHOULD be the latest version supported by the server."
// (2025-11-25, lifecycle) 2026-07-28 has no initialize, so the
// latest the server can answer one with is 2025-11-25.
#[test]
fn initialize_in_a_revision_not_spoken_is_answered_in_the_latest_that_is() {
    let roms = Roms::all();
    for revision in ["2024-11-05", MODERN] {
        let (_session, answer) = Session::legacy(&roms, revision);
        assert_eq!(
            answer["result"]["protocolVersion"], "2025-11-25",
            "{answer}"
        );
    }
}

// "A request missing any required field is malformed; the server MUST
// reject it with JSON-RPC error code -32602 (Invalid params)."
#[test]
fn a_request_that_does_not_say_its_revision_is_refused() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.revision = None;
    let response = session.request("tools/list", json!({}));
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

// "If the server does not implement the requested version ... it MUST
// respond with an UnsupportedProtocolVersionError listing the versions it
// does support", code -32022.
#[test]
fn a_revision_not_spoken_is_refused_with_those_that_are() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.revision = Some("2024-11-05");
    let response = session.request("tools/list", json!({}));
    let error = &response["error"];
    assert_eq!(error["code"], -32022, "{response}");
    assert_eq!(error["data"]["requested"], "2024-11-05", "{response}");
    assert_eq!(
        error["data"]["supported"],
        json!(["2025-06-18", "2025-11-25", "2026-07-28"]),
        "{response}"
    );
}

fn tools(session: &mut Session) -> Value {
    session.result("tools/list", json!({}))["tools"].clone()
}

// The set of tools "MUST NOT vary per-connection or as a side effect of
// other requests on the connection", and "Servers SHOULD return tools in a
// deterministic order".
#[test]
fn the_tools_are_the_same_whatever_has_been_done() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let first = tools(&mut session);
    let names: Vec<&str> = first
        .as_array()
        .expect("a list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, TOOLS);
    let handle = session.open("cpc6128");
    assert_eq!(tools(&mut session), first);
    session.answer("machine_close", json!({ "handle": handle }));
    assert_eq!(tools(&mut session), first);
    assert_eq!(tools(&mut Session::start(&roms)), first);
}

fn input_schema(tools: &Value, name: &str) -> Value {
    tools
        .as_array()
        .expect("a list")
        .iter()
        .find(|tool| tool["name"] == name)
        .expect("the tool is listed")["inputSchema"]
        .clone()
}

// "For tools with no parameters ... { "type": "object",
// "additionalProperties": false } - Recommended: explicitly accepts only
// empty objects"
#[test]
fn machine_list_takes_no_arguments() {
    let roms = Roms::all();
    let tools = tools(&mut Session::start(&roms));
    assert_eq!(
        input_schema(&tools, "machine_list"),
        json!({ "type": "object", "additionalProperties": false })
    );
}

#[test]
fn machine_open_offers_the_machines_by_their_names() {
    let roms = Roms::all();
    let tools = tools(&mut Session::start(&roms));
    let schema = input_schema(&tools, "machine_open");
    let names: Vec<&str> = MACHINES.iter().map(|machine| machine.name).collect();
    assert_eq!(schema["properties"]["machine"]["enum"], json!(names));
    assert_eq!(schema["required"], json!(["machine"]));
}

fn a_machine_stands_from_its_opening_to_its_closing(session: &mut Session) {
    assert!(session.handles().is_empty());
    let handle = session.open("spectrum48");
    let listing = session.answer("machine_list", json!({}));
    assert_eq!(
        listing,
        json!({ "machines": [{ "handle": handle, "machine": "spectrum48" }] })
    );
    let closed = session.answer("machine_close", json!({ "handle": handle }));
    assert_eq!(closed, json!({ "handle": handle }));
    assert!(session.handles().is_empty());
}

#[test]
fn a_machine_stands_from_its_opening_to_its_closing_in_each_revision() {
    let roms = Roms::all();
    a_machine_stands_from_its_opening_to_its_closing(&mut Session::start(&roms));
    for revision in LEGACY {
        let (mut session, _) = Session::legacy(&roms, revision);
        a_machine_stands_from_its_opening_to_its_closing(&mut session);
    }
}

#[test]
fn every_machine_the_command_line_names_opens_and_is_listed_in_order() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handles: Vec<String> = MACHINES
        .iter()
        .map(|machine| session.open(machine.name))
        .collect();
    assert_eq!(session.handles(), handles);
}

#[test]
fn a_handle_is_never_given_twice() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let first = session.open("cpc464");
    session.answer("machine_close", json!({ "handle": first }));
    let second = session.open("cpc464");
    assert_ne!(first, second);
    let refusal = session.refusal("machine_close", json!({ "handle": first }));
    assert!(
        refusal.starts_with(&format!("no machine has the handle {first}")),
        "{refusal}"
    );
}

// Input validation and business-logic errors are "Tool Execution Errors",
// "reported in tool results with isError: true".
#[test]
fn an_unknown_machine_is_refused_with_the_names_there_are() {
    let roms = Roms::all();
    let refusal = Session::start(&roms).refusal("machine_open", json!({ "machine": "zx81" }));
    let names: Vec<&str> = MACHINES.iter().map(|machine| machine.name).collect();
    assert_eq!(
        refusal,
        format!(
            "no machine is called zx81; the machines are {}",
            names.join(", ")
        )
    );
}

#[test]
fn an_unknown_handle_is_refused_with_the_handles_there_are() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    assert_eq!(
        session.refusal("machine_close", json!({ "handle": "cpc6128-9" })),
        "no machine has the handle cpc6128-9; no machine is standing, and machine_open stands \
         one up"
    );
    let first = session.open("cpc6128");
    let second = session.open("spectrum48");
    assert_eq!(
        session.refusal("machine_close", json!({ "handle": "cpc6128-9" })),
        format!(
            "no machine has the handle cpc6128-9; the machines standing have the handles \
             {first}, {second}"
        )
    );
}

#[test]
fn missing_firmware_is_refused_as_a_tool_and_opens_nothing() {
    let roms = Roms::new(&[]);
    let mut session = Session::start(&roms);
    let refusal = session.refusal("machine_open", json!({ "machine": "spectrum48" }));
    assert!(refusal.contains("`make roms`"), "{refusal}");
    assert!(session.handles().is_empty());
}

// "For backwards compatibility, a tool that returns structured content
// SHOULD also return the serialized JSON in a TextContent block."
#[test]
fn a_result_carries_its_json_as_text_too() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let result = session.call("machine_open", json!({ "machine": "cpc664" }));
    let content = result["content"].as_array().expect("content");
    assert_eq!(content.len(), 1, "{result}");
    assert_eq!(content[0]["type"], "text", "{result}");
    let text: Value =
        serde_json::from_str(content[0]["text"].as_str().expect("text")).expect("JSON");
    assert_eq!(text, result["structuredContent"]);
}

// "The server MUST NOT write anything to its stdout that is not a valid MCP
// message", and "The server MUST NOT write JSON-RPC requests to stdout."
// (stdio transport)
#[test]
fn nothing_but_answers_reaches_standard_output() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.result("server/discover", json!({}));
    tools(&mut session);
    let handle = session.open("cpc6128");
    session.refusal("machine_open", json!({ "machine": "zx81" }));
    session.answer("machine_close", json!({ "handle": handle }));
    session.revision = None;
    session.request("tools/list", json!({}));
    let (_, lines, _) = session.finish();
    assert!(!lines.is_empty());
    for line in lines {
        let message: Value = serde_json::from_str(&line).expect("every line is JSON");
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        assert!(message["method"].is_null(), "{line}");
        assert!(
            message["result"].is_object() != message["error"].is_object(),
            "{line}"
        );
    }
}

// "Servers SHOULD exit promptly when their standard input is closed or
// reads return end-of-file." (stdio transport)
#[test]
fn the_server_stops_when_its_input_closes() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.open("cpc6128");
    session.open("spectrum48");
    let (status, _, errors) = session.finish();
    assert!(status.success(), "{status}: {errors}");
    assert_eq!(errors, "");
}

#[test]
fn a_client_may_ask_who_the_server_is_and_go() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.result("server/discover", json!({}));
    let (status, _, errors) = session.finish();
    assert!(status.success(), "{status}: {errors}");
    assert_eq!(errors, "");
}

fn refused_at_the_start(arguments: &[&str]) -> String {
    let output = server().args(arguments).output().expect("the server runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    String::from_utf8(output.stderr).expect("text")
}

#[test]
fn the_server_will_not_start_without_a_directory_to_read_firmware_from() {
    assert!(refused_at_the_start(&[]).starts_with("usage: colophon-mcp --roms DIRECTORY"));
    assert!(refused_at_the_start(&["--rom", "roms"]).starts_with("usage:"));
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-directory");
    assert_eq!(
        refused_at_the_start(&["--roms", missing.to_str().expect("a path")]),
        format!(
            "--roms names {}, which is no directory\n\nusage: colophon-mcp --roms DIRECTORY\n\n  \
             --roms DIRECTORY  the firmware, as the emulator's `make roms` fetches it\n",
            missing.display()
        )
    );
}

// "The result MUST include a resultType field", and "Servers SHOULD include
// the following io.modelcontextprotocol/* field in every result's _meta":
// serverInfo. (2026-07-28, basic) The older revisions ask neither, and the
// server names its build in their results all the same. A ping's result, and
// those rmcp gives empty for what the server does not declare, such as
// resources/list, carry nothing; initialize names it in its serverInfo.
#[test]
fn every_result_is_complete_and_names_the_build() {
    let roms = Roms::all();
    let identity = json!({ "name": "colophon-mcp", "version": env!("COLOPHON_MCP_BUILD") });
    let mut session = Session::start(&roms);
    let results = [
        session.result("server/discover", json!({})),
        session.result("tools/list", json!({})),
        session.call("machine_open", json!({ "machine": "cpc6128" })),
        session.call("machine_close", json!({ "handle": "cpc6128-9" })),
    ];
    for result in results {
        assert_eq!(result["resultType"], "complete", "{result}");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"], identity,
            "{result}"
        );
    }
    for revision in LEGACY {
        let (mut session, _) = Session::legacy(&roms, revision);
        let results = [
            session.result("tools/list", json!({})),
            session.call("machine_open", json!({ "machine": "cpc6128" })),
            session.call("machine_close", json!({ "handle": "cpc6128-9" })),
        ];
        for result in results {
            assert_eq!(
                result["_meta"]["io.modelcontextprotocol/serverInfo"], identity,
                "{result}"
            );
        }
    }
}

// "Servers that support tools MUST declare the tools capability" (2025-11-25
// and 2026-07-28, tools).
#[test]
fn the_tools_capability_is_declared_in_each_revision() {
    let roms = Roms::all();
    let discovered = Session::start(&roms).result("server/discover", json!({}));
    assert!(
        discovered["capabilities"]["tools"].is_object(),
        "{discovered}"
    );
    for revision in LEGACY {
        let (_session, answer) = Session::legacy(&roms, revision);
        assert!(
            answer["result"]["capabilities"]["tools"].is_object(),
            "{answer}"
        );
    }
}

// "The receiver MUST respond promptly with an empty response" (2025-11-25
// and 2025-06-18, ping), and a client may ping before it is answered: "The
// client SHOULD NOT send requests other than pings before the server has
// responded to the initialize request" (2025-11-25, lifecycle).
#[test]
fn ping_is_answered_in_the_revisions_that_have_it() {
    let roms = Roms::all();
    for revision in LEGACY {
        let (mut session, _) = Session::legacy(&roms, revision);
        assert_eq!(session.result("ping", json!({})), json!({}));
    }
    // A ping written after initialize and before its answer is read.
    let mut session = Session::start(&roms);
    session.send(json!({
        "jsonrpc": "2.0",
        "id": "initialize",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "colophon-mcp-tests", "version": "0" },
        },
    }));
    session.send(json!({ "jsonrpc": "2.0", "id": "ping", "method": "ping" }));
    let answers = [session.receive(), session.receive()];
    let ping = answers
        .iter()
        .find(|answer| answer["id"] == "ping")
        .expect("the ping is answered");
    assert_eq!(ping["result"], json!({}), "{ping}");
}

// 2026-07-28 removes ping, and "A request carrying modern per-request _meta
// is served statelessly according to this revision" (versioning), so a ping
// that names it is a method the server does not have. rmcp refuses one only
// once a request other than server/discover has opened that revision's
// lifecycle: a ping before then, after a discovery too, it answers as a ping
// before initialize, empty, without the resultType the revision requires. The
// divergence is rmcp's, and is held here so that a release that changes it is
// seen.
#[test]
fn a_ping_in_2026_07_28_is_refused_once_the_revision_has_been_spoken() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    assert_eq!(session.result("ping", json!({})), json!({}));
    session.result("server/discover", json!({}));
    assert_eq!(session.result("ping", json!({})), json!({}));
    tools(&mut session);
    let response = session.request("ping", json!({}));
    assert_eq!(response["error"]["code"], -32601, "{response}");
}

// A request whose _meta lacks a required field is malformed as one without
// _meta is: "A request missing any required field is malformed".
#[test]
fn a_request_that_does_not_say_its_capabilities_is_refused() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    session.revision = None;
    let response = session.request(
        "tools/list",
        json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": MODERN } }),
    );
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

// An unknown tool is a protocol error, whose example code is -32602; input
// that fails validation is a tool execution error, so that the model can
// correct itself. (2026-07-28, tools)
#[test]
fn an_unknown_tool_is_a_protocol_error_and_unfit_arguments_a_tool_error() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let response = session.request(
        "tools/call",
        json!({ "name": "machine_run", "arguments": {} }),
    );
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(
        response["error"]["message"],
        format!(
            "no tool is called machine_run; the tools are {}",
            TOOLS.join(", ")
        ),
        "{response}"
    );
    let refusal = session.refusal("machine_open", json!({}));
    assert!(refusal.contains("`machine`"), "{refusal}");
    let refusal = session.refusal("machine_list", json!({ "machine": "cpc6128" }));
    assert!(refusal.contains("`machine`"), "{refusal}");
}
