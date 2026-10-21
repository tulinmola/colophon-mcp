// The server as a client meets it: the built binary, spoken to over its
// standard input and output in raw lines, and graded against the
// specification's own sentences, quoted where they are used. Revision
// 2026-07-28 is at https://modelcontextprotocol.io/specification/2026-07-28,
// and 2025-11-25 and 2025-06-18 beside it; the text quoted was read in the
// specification repository at tag 2026-07-28.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use colophon_mcp::machines::MACHINES;
use serde_json::{Value, json};
use sha2::Digest;

const MODERN: &str = "2026-07-28";
const LEGACY: [&str; 2] = ["2025-06-18", "2025-11-25"];
const TOOLS: [&str; 21] = [
    "call",
    "disassemble",
    "disc_insert",
    "inspect",
    "journal",
    "machine_close",
    "machine_list",
    "machine_open",
    "monitor",
    "peek",
    "poke",
    "run",
    "screen",
    "set",
    "snapshot_save",
    "state_drop",
    "state_keep",
    "state_restore",
    "step",
    "tape_insert",
    "type",
];
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

    // Every stand-in, one of them beginning with a program the test wrote.
    fn programmed(file: &str, program: &[u8]) -> Roms {
        let roms = Roms::all();
        let path = roms.0.join(file);
        let mut contents = fs::read(&path).expect("a stand-in is read");
        contents[..program.len()].copy_from_slice(program);
        fs::write(path, contents).expect("a stand-in is written");
        roms
    }

    // Bytes written into a stand-in at an offset of its own.
    fn write_at(&self, file: &str, offset: usize, bytes: &[u8]) {
        let path = self.0.join(file);
        let mut contents = fs::read(&path).expect("a stand-in is read");
        contents[offset..offset + bytes.len()].copy_from_slice(bytes);
        fs::write(path, contents).expect("a stand-in is written");
    }

    // A file of the test's own beside the stand-ins.
    fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("a file is written");
        path
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
    // The tools as tools/list gave them, once asked.
    tools: Option<Value>,
}

impl Session {
    fn start(roms: &Roms) -> Session {
        Session::reading(&roms.0)
    }

    fn reading(roms: &Path) -> Session {
        Session::spawned(server().arg("--roms").arg(roms))
    }

    // A server whose working directory is the stand-ins', so that a relative
    // path, its own --roms among them, names a file beside them.
    fn beside(roms: &Roms) -> Session {
        Session::spawned(server().arg("--roms").arg(".").current_dir(&roms.0))
    }

    fn spawned(server: &mut Command) -> Session {
        let mut child = server
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
            tools: None,
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
        self.conforming(tool, &result)
    }

    // "If an output schema is provided: Servers MUST provide structured
    // results that conform to this schema." (2026-07-28, tools)
    fn conforming(&mut self, tool: &str, result: &Value) -> Value {
        assert_eq!(result["isError"], false, "{result}");
        let answer = result["structuredContent"].clone();
        if self.tools.is_none() {
            self.tools = Some(self.result("tools/list", json!({}))["tools"].clone());
        }
        let tools = self.tools.as_ref().expect("the tools are known");
        let schema = &tools
            .as_array()
            .expect("a list")
            .iter()
            .find(|listed| listed["name"] == tool)
            .expect("the tool is listed")["outputSchema"];
        if let Err(problem) = known(schema, tool).and_then(|()| fits(&answer, schema, schema, tool))
        {
            panic!("{problem}: {answer}");
        }
        answer
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

// The keywords fits reads, and those that constrain nothing; a schema using
// any other is refused, so that nothing in it goes unchecked.
const KEYWORDS: &[&str] = &[
    "$ref",
    "anyOf",
    "oneOf",
    "type",
    "enum",
    "const",
    "minimum",
    "maximum",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "$defs",
    "$schema",
    "title",
    "description",
    "format",
];

// Whether a schema uses only the keywords in KEYWORDS, in every part of it.
fn known(schema: &Value, at: &str) -> Result<(), String> {
    let keywords = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(keywords) => keywords,
        _ => return Err(format!("{at}: {schema} is no schema")),
    };
    if let Some(unknown) = keywords
        .keys()
        .find(|keyword| !KEYWORDS.contains(&keyword.as_str()))
    {
        return Err(format!(
            "{at}: the schema uses {unknown}, which fits does not read"
        ));
    }
    for keyword in ["properties", "$defs"] {
        for (name, part) in schema[keyword].as_object().into_iter().flatten() {
            known(part, &format!("{at}.{keyword}.{name}"))?;
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        for (index, part) in schema[keyword].as_array().into_iter().flatten().enumerate() {
            known(part, &format!("{at}.{keyword}[{index}]"))?;
        }
    }
    for keyword in ["items", "additionalProperties"] {
        if let Some(part) = schema.get(keyword) {
            known(part, &format!("{at}.{keyword}"))?;
        }
    }
    Ok(())
}

// Whether a value fits a schema, as far as JSON Schema's keywords in KEYWORDS
// go; known() has seen that the schema uses no other.
fn fits(value: &Value, schema: &Value, root: &Value, at: &str) -> Result<(), String> {
    if let Value::Bool(admits) = schema {
        return if *admits {
            Ok(())
        } else {
            Err(format!("{at}: {value} meets a schema that admits nothing"))
        };
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let defined = reference
            .strip_prefix("#/$defs/")
            .and_then(|name| root["$defs"].get(name))
            .ok_or_else(|| format!("{at}: a reference to nothing, {reference}"))?;
        fits(value, defined, root, at)?;
    }
    let branches = schema["anyOf"].as_array();
    if branches.is_some_and(|branches| {
        !branches
            .iter()
            .any(|branch| fits(value, branch, root, at).is_ok())
    }) {
        return Err(format!("{at}: {value} fits none of {schema}"));
    }
    if let Some(kinds) = schema.get("type") {
        let kinds: Vec<&str> = match kinds {
            Value::String(kind) => vec![kind.as_str()],
            Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => return Err(format!("{at}: a type that is no type, {kinds}")),
        };
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_f64() => "number",
            Value::Number(_) => "integer",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        let admitted = kinds.contains(&kind) || (kind == "integer" && kinds.contains(&"number"));
        if !admitted {
            return Err(format!("{at}: {value} is not of type {kinds:?}"));
        }
    }
    let one_of = schema["oneOf"].as_array();
    if one_of.is_some_and(|branches| {
        branches
            .iter()
            .filter(|branch| fits(value, branch, root, at).is_ok())
            .count()
            != 1
    }) {
        return Err(format!("{at}: {value} fits not exactly one of {schema}"));
    }
    if schema
        .get("const")
        .is_some_and(|constant| constant != value)
    {
        return Err(format!("{at}: {value} is not {}", schema["const"]));
    }
    let choices = schema["enum"].as_array();
    if choices.is_some_and(|choices| !choices.contains(value)) {
        return Err(format!("{at}: {value} is none of {schema}"));
    }
    if let Some(number) = value.as_f64() {
        let below = schema["minimum"]
            .as_f64()
            .is_some_and(|least| number < least);
        let above = schema["maximum"].as_f64().is_some_and(|most| number > most);
        if below || above {
            return Err(format!("{at}: {value} is out of the bounds of {schema}"));
        }
    }
    if let Value::Object(fields) = value {
        for required in schema["required"].as_array().into_iter().flatten() {
            let name = required.as_str().unwrap_or_default();
            if !fields.contains_key(name) {
                return Err(format!("{at}: {name} is required"));
            }
        }
        for (name, field) in fields {
            let at = format!("{at}.{name}");
            match schema["properties"].get(name) {
                Some(property) => fits(field, property, root, &at)?,
                None if schema["additionalProperties"] == false => {
                    return Err(format!("{at} is not among the properties"));
                }
                None => {
                    if let Some(rest) = schema
                        .get("additionalProperties")
                        .filter(|rest| rest.is_object())
                    {
                        fits(field, rest, root, &at)?;
                    }
                }
            }
        }
    }
    if let Value::Array(elements) = value {
        let count = elements.len() as u64;
        let fewer = schema["minItems"]
            .as_u64()
            .is_some_and(|least| count < least);
        let more = schema["maxItems"].as_u64().is_some_and(|most| count > most);
        if fewer || more {
            return Err(format!(
                "{at}: {value} has {count} items, out of the bounds of {schema}"
            ));
        }
    }
    if let (Value::Array(elements), Some(items)) = (value, schema.get("items")) {
        for (index, element) in elements.iter().enumerate() {
            fits(element, items, root, &format!("{at}[{index}]"))?;
        }
    }
    Ok(())
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

// Every description in a schema, wherever it stands.
fn descriptions<'a>(value: &'a Value, into: &mut Vec<&'a str>) {
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                match (name.as_str(), field) {
                    ("description", Value::String(text)) => into.push(text),
                    _ => descriptions(field, into),
                }
            }
        }
        Value::Array(values) => {
            for field in values {
                descriptions(field, into);
            }
        }
        _ => {}
    }
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
    // A description is read as one paragraph, whatever lines its header kept
    // it on.
    let mut texts = Vec::new();
    descriptions(&first, &mut texts);
    for text in texts {
        assert!(!text.contains('\n'), "{text:?}");
    }
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

// Programs the tests write, in the Z80's own bytes.
const ADD_A_B: u8 = 0x80;
const AND_N: u8 = 0xE6;
const CP_E: u8 = 0xBB;
const DI: u8 = 0xF3;
const EI: u8 = 0xFB;
const EX_SP_IX: [u8; 2] = [0xDD, 0xE3];
const HALT: u8 = 0x76;
const IM_1: [u8; 2] = [0xED, 0x56];
const IN_A_C: [u8; 2] = [0xED, 0x78];
const IN_A_N: u8 = 0xDB;
const INC_A: u8 = 0x3C;
const DEC_BC: u8 = 0x0B;
const INC_BC: u8 = 0x03;
const INC_HL: u8 = 0x23;
const INC_AT_HL: u8 = 0x34;
const INC_L: u8 = 0x2C;
const JR: u8 = 0x18;
const JR_NZ: u8 = 0x20;
const JR_Z: u8 = 0x28;
const LD_A: u8 = 0x3E;
const LD_A_AT_HL: u8 = 0x7E;
const LD_A_B: u8 = 0x78;
const LD_A_FROM: u8 = 0x3A;
const LD_AT_HL: u8 = 0x36;
const LD_AT_HL_A: u8 = 0x77;
const LD_BC: u8 = 0x01;
const LD_E: u8 = 0x1E;
const LD_E_A: u8 = 0x5F;
const LD_HL: u8 = 0x21;
const LD_HL_TO: u8 = 0x22;
const LD_I_A: [u8; 2] = [0xED, 0x47];
const LD_SP: u8 = 0x31;
const LD_TO_A: u8 = 0x32;
const NOP: u8 = 0x00;
const OUT_C_A: [u8; 2] = [0xED, 0x79];
const OUT_C_C: [u8; 2] = [0xED, 0x49];
const OR_C: u8 = 0xB1;
const OUT_N_A: u8 = 0xD3;
const POP_AF: u8 = 0xF1;
const POP_DE: u8 = 0xD1;
const PREFIX_DD: u8 = 0xDD;
const PUSH_AF: u8 = 0xF5;
const PUSH_HL: u8 = 0xE5;
const RET: u8 = 0xC9;
const RETN: [u8; 2] = [0xED, 0x45];

// A standard frame is 312 lines of 64µs, "exactly 19968 μsec" (ch. 4.1), and
// the Gate Array "clocks the Z80A at 4 MHz" (ch. 4.4.4): "The Amstrad CPC
// CRTC Compendium" v1.11, https://shaker.logonsystem.eu/ACCC1.11-EN.pdf
const CPC_FRAME_TICKS: u64 = 19968 * 4;

// 224 T-states to a line and 312 lines to a frame on a 48K's Ferranti ULA:
// libspectrum's timings.c,
// https://sourceforge.net/p/fuse-emulator/libspectrum/ci/master/tree/timings.c
const SPECTRUM_FRAME_TICKS: u64 = 224 * 312;

// INC BC with I in contended memory is charged by the ULA wherever the beam
// stands, so how long each pass takes depends on everything the machine has
// done before it ("Contended memory", Sinclair Wiki,
// https://sinclair.wiki.zxnet.co.uk/wiki/Contended_memory).
const CONTENDED_LOOP: [u8; 8] = [DI, LD_A, 0x40, LD_I_A[0], LD_I_A[1], INC_BC, JR, 0xFD];

// From HL on, round again incrementing the byte at HL and HL itself.
const COUNTING_LOOP: [u8; 4] = [INC_AT_HL, INC_HL, JR, 0xFC];

// An address or a port as an answer gives it, in hexadecimal after &.
fn ampersand(address: u16) -> Value {
    json!(format!("&{address:04X}"))
}

// The number an answer gives in hexadecimal after &, in the digits it writes
// for that kind of number: four for an address or a port, two for a byte.
fn after_ampersand(value: &Value, digits: usize) -> u64 {
    let written = value
        .as_str()
        .and_then(|text| text.strip_prefix('&'))
        .filter(|written| {
            written.len() == digits
                && written
                    .chars()
                    .all(|digit| digit.is_ascii_digit() || ('A'..='F').contains(&digit))
        })
        .unwrap_or_else(|| panic!("{value} is no {digits} hexadecimal digits after &"));
    u64::from_str_radix(written, 16).expect("hexadecimal")
}

impl Session {
    fn run(&mut self, handle: &str, arguments: Value) -> Value {
        let mut arguments = arguments;
        arguments["handle"] = json!(handle);
        self.answer("run", arguments)
    }

    fn step(&mut self, handle: &str, instructions: u32) -> Value {
        self.answer(
            "step",
            json!({ "handle": handle, "instructions": instructions }),
        )
    }

    fn inspect(&mut self, handle: &str, path: &str) -> Value {
        self.answer("inspect", json!({ "handle": handle, "path": path }))["value"].clone()
    }

    fn set(&mut self, handle: &str, values: Value) -> Value {
        self.answer("set", json!({ "handle": handle, "values": values }))
    }

    fn peek(&mut self, handle: &str, address: Value, length: u16) -> String {
        let peeked = self.answer(
            "peek",
            json!({ "handle": handle, "address": address, "length": length }),
        );
        peeked["bytes"].as_str().expect("bytes").to_owned()
    }

    fn poke(&mut self, handle: &str, address: Value, bytes: &str) {
        self.answer(
            "poke",
            json!({ "handle": handle, "address": address, "bytes": bytes }),
        );
    }

    fn open_from(&mut self, machine: &str, snapshot: &Path) -> String {
        let opened = self.answer(
            "machine_open",
            json!({ "machine": machine, "snapshot": snapshot }),
        );
        opened["handle"].as_str().expect("a handle").to_owned()
    }

    // Runs a program to the HALT it ends on with interrupts disabled, where
    // its machine waits for good and a run stops, then the frames asked,
    // which run whole from there.
    fn run_past_its_halt(&mut self, handle: &str, frames: u32) -> Value {
        let halting = self.run(handle, json!({ "frames": frames }));
        assert_eq!(halting["stopped"], "halted", "{halting}");
        self.run(handle, json!({ "frames": frames }))
    }

    // Sends a call and answers its id, without waiting for the answer.
    fn send_call(&mut self, tool: &str, arguments: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN,
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }));
        id
    }
}

fn listed(tools: &Value, name: &str) -> Value {
    tools
        .as_array()
        .expect("a list")
        .iter()
        .find(|tool| tool["name"] == name)
        .expect("the tool is listed")
        .clone()
}

// "If true, the tool does not modify its environment" (readOnlyHint); "If
// true, the tool may perform destructive updates to its environment. If
// false, the tool performs only additive updates" (destructiveHint); "If true,
// calling the tool repeatedly with the same arguments will have no additional
// effect on its environment" (idempotentHint); "If false, the tool's domain of
// interaction is closed" (openWorldHint), which is true unless said: the
// 2026-07-28 schema's ToolAnnotations. A machine's state overwritten is a
// destructive update, and a machine stood up, a state kept or a file written
// where none was an additive one.
#[test]
fn every_tool_says_what_it_does_to_the_machines_and_that_nothing_else_is_reached() {
    let roms = Roms::all();
    let tools = tools(&mut Session::start(&roms));
    for name in TOOLS {
        let annotations = &listed(&tools, name)["annotations"];
        assert_eq!(annotations["openWorldHint"], false, "{name}: {annotations}");
        let reads = [
            "disassemble",
            "inspect",
            "journal",
            "machine_list",
            "monitor",
            "peek",
            "screen",
        ]
        .contains(&name);
        assert_eq!(annotations["readOnlyHint"], reads, "{name}: {annotations}");
        if !reads {
            let destroys = !["machine_open", "snapshot_save", "state_keep"].contains(&name);
            // A machine stood up again is another machine, and a state kept
            // again another state; every other request that changes a
            // machine, save a restore, is entered in its journal, which a
            // repeat lengthens. A handle put away names nothing again, so
            // putting it away again changes nothing, and so does a state
            // dropped; a state restored again makes the same machine and the
            // same journal; a snapshot asked for again at its path is
            // refused, since a file is there.
            let repeats = [
                "machine_close",
                "snapshot_save",
                "state_restore",
                "state_drop",
            ]
            .contains(&name);
            assert_eq!(
                annotations["destructiveHint"], destroys,
                "{name}: {annotations}"
            );
            assert_eq!(
                annotations["idempotentHint"], repeats,
                "{name}: {annotations}"
            );
        }
    }
}

#[test]
fn a_run_by_frames_ends_at_a_retrace_and_one_by_ticks_runs_exactly_that_many() {
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    // Halted for good, where every run runs as asked.
    let halting = session.step(&handle, 2)["tick"].as_u64().unwrap();
    // The monitor locks on over the first two.
    let first = session.run(&handle, json!({ "frames": 2 }));
    assert_eq!(first["frames"], 2, "{first}");
    assert_eq!(first["settled"], true, "{first}");
    assert_eq!(first["stopped"], "frames", "{first}");
    assert_eq!(
        first["tick"],
        halting + first["ticks"].as_u64().unwrap(),
        "{first}"
    );
    // Halted in the ROM, it fetches where nothing is contended, four T-states
    // at a time, so every count here is a whole number of fetches.
    let second = session.run(&handle, json!({ "frames": 1 }));
    assert_eq!(second["ticks"], SPECTRUM_FRAME_TICKS, "{second}");
    let ticks = 3 * SPECTRUM_FRAME_TICKS + 1000;
    let ticked = session.run(&handle, json!({ "ticks": ticks }));
    assert_eq!(
        (&ticked["ticks"], &ticked["frames"], &ticked["stopped"]),
        (&json!(ticks), &json!(3), &json!("ticks")),
        "{ticked}"
    );
    assert_eq!(
        ticked["tick"].as_u64(),
        Some(first["tick"].as_u64().unwrap() + SPECTRUM_FRAME_TICKS + ticks)
    );
}

// A CPC whose 6845 nobody programs never retraces, and a run by frames is
// held to twice a standard frame's ticks for each frame asked.
#[test]
fn a_run_by_frames_that_never_come_ends_at_its_limit() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    let run = session.run(&handle, json!({ "frames": 3 }));
    assert_eq!(run["stopped"], "limit", "{run}");
    assert_eq!(run["frames"], 0, "{run}");
    let ticks = run["ticks"].as_u64().unwrap();
    assert!(
        (6 * CPC_FRAME_TICKS..6 * CPC_FRAME_TICKS + 32).contains(&ticks),
        "{run}"
    );
}

// The frames end where they end however the run is cut, so three runs of a
// frame stand the machine where one of three does.
#[test]
fn a_run_cut_into_frames_ends_where_the_whole_run_does() {
    let roms = Roms::programmed("spectrum48.rom", &CONTENDED_LOOP);
    let mut session = Session::start(&roms);
    let whole = session.open("spectrum48");
    let cut = session.open("spectrum48");
    session.step(&whole, 3);
    session.step(&cut, 3);
    let once = session.run(&whole, json!({ "frames": 3 }));
    let mut thrice = Value::Null;
    for _ in 0..3 {
        thrice = session.run(&cut, json!({ "frames": 1 }));
    }
    assert_eq!(once["tick"], thrice["tick"]);
    assert_eq!(session.inspect(&whole, "cpu"), session.inspect(&cut, "cpu"));
}

// LD BC,nn takes 10 T-states, EX (SP),IX 23 and NOP 4 (Z80 CPU User Manual,
// Zilog UM0080, https://www.zilog.com/docs/z80/um0080.pdf), from the ROM,
// where nothing is contended.
#[test]
fn a_step_runs_whole_instructions() {
    let program = [LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1], NOP];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let one = session.answer("step", json!({ "handle": handle }));
    assert_eq!(
        (&one["ticks"], &one["pc"], &one["stopped"]),
        (&json!(10), &ampersand(3), &json!("instructions")),
        "{one}"
    );
    let two = session.step(&handle, 2);
    assert_eq!(
        (&two["ticks"], &two["pc"]),
        (&json!(23 + 4), &ampersand(6)),
        "{two}"
    );
    assert_eq!(two["tick"], 10 + 23 + 4, "{two}");
}

#[test]
fn a_run_or_a_step_caught_in_a_chain_of_prefixes_says_so() {
    let roms = Roms::programmed("spectrum48.rom", &[PREFIX_DD; 0x4000]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let step = session.step(&handle, 1);
    assert_eq!(
        (&step["settled"], &step["stopped"]),
        (&json!(false), &json!("unsettled")),
        "{step}"
    );
    let run = session.run(&handle, json!({ "ticks": 100 }));
    assert_eq!(
        (&run["settled"], &run["stopped"]),
        (&json!(false), &json!("unsettled")),
        "{run}"
    );
    // Steps asked of a twin stop at the first that does not end.
    let twin = session.open("spectrum48");
    let three = session.step(&twin, 3);
    assert_eq!(three["ticks"], step["ticks"], "{three}");
}

#[test]
fn what_a_run_or_a_step_may_ask_is_said_when_it_asks_otherwise() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    let one_of_two = "a run takes frames or ticks, one of the two: {\"frames\": 50} runs to the 50th \
                      retrace, {\"ticks\": 3993600} runs that many ticks";
    assert_eq!(session.refusal("run", json!({ "handle": cpc })), one_of_two);
    assert_eq!(
        session.refusal("run", json!({ "handle": cpc, "frames": 1, "ticks": 1 })),
        one_of_two
    );
    for frames in [-1, 0, 1501] {
        assert_eq!(
            session.refusal("run", json!({ "handle": cpc, "frames": frames })),
            format!(
                "frames is {frames}, and a run takes from 1 to 1500 frames; run again to go \
                 further"
            )
        );
    }
    assert_eq!(
        session.refusal(
            "run",
            json!({ "handle": cpc, "ticks": 1500 * CPC_FRAME_TICKS + 1 })
        ),
        "ticks is 119808001, and a run on a cpc6128 takes from 1 to 119808000 ticks, the ticks \
         of 1500 standard frames; run again to go further"
    );
    for instructions in [-5, 0, 100_001] {
        assert_eq!(
            session.refusal(
                "step",
                json!({ "handle": cpc, "instructions": instructions })
            ),
            format!(
                "instructions is {instructions}, and a step takes from 1 to 100000 \
                 instructions; step again to go further"
            )
        );
    }
    let refusal = session.refusal("run", json!({ "handle": cpc, "frame": 1 }));
    assert!(refusal.contains("`frame`"), "{refusal}");
}

// Each tool given what it takes, and a handle no machine has.
#[test]
fn every_tool_refuses_a_handle_never_given_with_the_handles_there_are() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    for (tool, arguments) in [
        ("run", json!({ "frames": 1 })),
        ("step", json!({})),
        ("inspect", json!({})),
        ("set", json!({ "values": {} })),
        ("peek", json!({ "address": 0 })),
        ("poke", json!({ "address": 0, "bytes": "00" })),
        ("monitor", json!({})),
        ("screen", json!({})),
        ("disassemble", json!({ "address": 0 })),
        ("machine_close", json!({})),
    ] {
        let mut arguments = arguments;
        arguments["handle"] = json!("cpc6128-9");
        assert_eq!(
            session.refusal(tool, arguments),
            format!(
                "no machine has the handle cpc6128-9; the machines standing have the handles \
                 {cpc}"
            ),
            "{tool}"
        );
    }
}

#[test]
fn machine_open_and_machine_close_refuse_what_they_do_not_take() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let refusal = session.refusal(
        "machine_open",
        json!({ "machine": "cpc464", "snapshots": "game.sna" }),
    );
    assert!(refusal.contains("`snapshots`"), "{refusal}");
    assert!(session.handles().is_empty());
    let handle = session.open("cpc464");
    let refusal = session.refusal("machine_close", json!({ "handle": handle, "force": true }));
    assert!(refusal.contains("`force`"), "{refusal}");
    assert_eq!(session.handles(), [handle]);
}

// What a machine is asked, in order.
fn calls(handle: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("step", json!({ "handle": handle, "instructions": 3 })),
        ("run", json!({ "handle": handle, "frames": 2 })),
        ("step", json!({ "handle": handle, "instructions": 1000 })),
        ("run", json!({ "handle": handle, "ticks": 12345 })),
        ("inspect", json!({ "handle": handle, "path": "cpu" })),
    ]
}

#[test]
fn running_one_machine_never_disturbs_another() {
    let roms = Roms::programmed("spectrum48.rom", &CONTENDED_LOOP);
    let alone: Vec<Value> = {
        let mut session = Session::start(&roms);
        let handle = session.open("spectrum48");
        calls(&handle)
            .into_iter()
            .map(|(tool, arguments)| session.answer(tool, arguments))
            .collect()
    };
    // Call by call with a twin, and with a CPC run between.
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    let first = session.open("spectrum48");
    let second = session.open("spectrum48");
    for (index, ((tool, mine), (_, twins))) in
        calls(&first).into_iter().zip(calls(&second)).enumerate()
    {
        let mine = session.answer(tool, mine);
        session.run(&cpc, json!({ "frames": 1 }));
        let twins = session.answer(tool, twins);
        assert_eq!(mine, alone[index], "{tool}");
        assert_eq!(twins, alone[index], "{tool}");
    }
    // Asked at once, so that the two machines run side by side.
    let third = session.open("spectrum48");
    let fourth = session.open("spectrum48");
    for (index, ((tool, theirs), (_, others))) in
        calls(&third).into_iter().zip(calls(&fourth)).enumerate()
    {
        let ids = [
            session.send_call(tool, theirs),
            session.send_call(tool, others),
        ];
        let received = [session.receive(), session.receive()];
        for id in ids {
            let answer = received
                .iter()
                .find(|answer| answer["id"] == id)
                .expect("each call is answered");
            assert_eq!(
                answer["result"]["structuredContent"], alone[index],
                "{answer}"
            );
        }
    }
}

// While a CPC runs, a CPC cannot be built, and the server answers what else
// it is asked meanwhile; a machine built sooner than one opened before it is
// listed after it all the same.
#[test]
fn a_machine_waiting_to_be_built_holds_nothing_else_up() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let running = session.open("cpc6128");
    let ran = session.send_call("run", json!({ "handle": running, "frames": 150 }));
    // Time for the run to begin and take the lock the build will wait on: a
    // pause of the test's own, which moves no machine.
    thread::sleep(Duration::from_millis(100));
    let waiting = session.send_call("machine_open", json!({ "machine": "cpc464" }));
    let built = session.send_call("machine_open", json!({ "machine": "spectrum48" }));
    let listed = session.send_call("machine_list", json!({}));
    let answers: Vec<Value> = (0..4).map(|_| session.receive()).collect();
    let order: Vec<&Value> = answers.iter().map(|answer| &answer["id"]).collect();
    let at = |id: u64| {
        order
            .iter()
            .position(|answered| **answered == id)
            .expect("answered")
    };
    assert!(at(built) < at(waiting) && at(listed) < at(ran), "{order:?}");
    assert_eq!(
        session.handles(),
        [running, "cpc464-2".to_owned(), "spectrum48-3".to_owned()]
    );
}

// Putting a machine away waits for what it was asked before, and holds
// nothing else up meanwhile.
#[test]
fn a_machine_put_away_while_it_runs_holds_nothing_else_up() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let running = session.open("spectrum48");
    let other = session.open("spectrum48");
    let ran = session.send_call("run", json!({ "handle": running, "frames": 200 }));
    let closed = session.send_call("machine_close", json!({ "handle": running }));
    let stepped = session.send_call("step", json!({ "handle": other }));
    let order: Vec<Value> = (0..3).map(|_| session.receive()["id"].clone()).collect();
    assert_eq!(order[0], stepped, "{order:?}");
    let at = |id: u64| {
        order
            .iter()
            .position(|answered| *answered == id)
            .expect("answered")
    };
    assert!(at(ran) < at(closed), "{order:?}");
}

// What a machine holds is read by the path a C expression gives it on the
// emulator's struct, in the order of the struct's fields.
#[test]
fn inspect_reads_the_whole_machine_or_the_part_a_path_names() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    session.set(&handle, json!({ "crtc.registers[12]": 0x2C }));
    let whole = session.call("inspect", json!({ "handle": handle }));
    let text = whole["content"][0]["text"].as_str().expect("text");
    // In the order of the header's fields: A, F and B, not alphabetically.
    assert!(
        text.starts_with(r#"{"path":"","value":{"cpu":{"a":255,"f":255,"b":"#),
        "{text}"
    );
    let machine = &whole["structuredContent"]["value"];
    assert_eq!(machine["ram_size"], 0x20000);
    assert_eq!(machine["disc_interface"], true);
    assert_eq!(machine["fdc"]["phase"], "UPD765_PHASE_IDLE");
    assert_eq!(machine["crtc"]["registers"][12], 0x2C);
    assert!(machine.get("ram").is_none(), "pointers are left out");
    assert_eq!(session.inspect(&handle, "crtc.registers[12]"), 0x2C);
    // "AF and SP are always set to FFFFh after a reset": Sean Young, "The
    // Undocumented Z80 Documented", 2.4, http://www.z80.info/zip/z80-documented.pdf
    assert_eq!(session.inspect(&handle, "cpu.sp"), 0xFFFF);
    assert_eq!(
        session
            .inspect(&handle, "gate_array.inks")
            .as_array()
            .map(Vec::len),
        Some(17)
    );
    assert_eq!(
        session.refusal(
            "inspect",
            json!({ "handle": handle, "path": "crtc.registers[18]" })
        ),
        "crtc.registers[18]: [18] is past the end of an array of 18, whose elements are [0] to \
         [17]"
    );
}

// The machine runs on from what set leaves it: at a program counter set, the
// processor fetches there.
#[test]
fn set_writes_what_a_machine_holds_and_it_runs_from_there() {
    let program = [LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1], NOP];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let written = session.set(&handle, json!({ "cpu.pc": "&0003", "cpu.b": 7 }));
    assert_eq!(written, json!({ "values": { "cpu.pc": 3, "cpu.b": 7 } }));
    let step = session.step(&handle, 1);
    assert_eq!(
        (&step["ticks"], &step["pc"]),
        (&json!(23), &ampersand(5)),
        "{step}"
    );
    assert_eq!(session.inspect(&handle, "cpu.b"), 7);
}

// A Spectrum's beam is reckoned from the T-state of its frame, which is what
// is written; it stands where a twin run to that T-state has it.
#[test]
fn a_spectrum_written_a_tick_of_its_frame_stands_its_beam_there() {
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let run = session.open("spectrum48");
    let written = session.open("spectrum48");
    session.step(&run, 2);
    let ran = session.run(&run, json!({ "ticks": 23456 }));
    assert_eq!(ran["stopped"], "ticks", "{ran}");
    let ula = session.inspect(&run, "ula");
    session.set(&written, json!({ "ula.frame_tick": ula["frame_tick"] }));
    assert_eq!(session.inspect(&written, "ula.line"), ula["line"]);
    assert_eq!(session.inspect(&written, "ula.column"), ula["column"]);
}

#[test]
fn set_writes_a_structure_by_some_of_its_fields_and_an_array_whole() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    let registers: Vec<u8> = (0..18).collect();
    session.set(
        &handle,
        json!({ "cpu": { "b": 1, "c": 2 }, "crtc.registers": registers }),
    );
    assert_eq!(session.inspect(&handle, "cpu.b"), 1);
    assert_eq!(session.inspect(&handle, "cpu.c"), 2);
    assert_eq!(session.inspect(&handle, "crtc.registers"), json!(registers));
}

#[test]
fn set_writes_every_value_or_none() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    let before = session.inspect(&handle, "cpu.a");
    assert_eq!(
        session.refusal(
            "set",
            json!({ "handle": handle, "values": { "cpu.a": 5, "cpu.pc": 65536 } })
        ),
        "cpu.pc: expects an integer from 0 to 65535, or hexadecimal from &0000 to &FFFF, written \
         with &, #, $ or 0x before it, not 65536"
    );
    assert_eq!(session.inspect(&handle, "cpu.a"), before);
}

// The core indexes the seventeen inks by the selected pen without checking
// it, as nothing it does can select an eighteenth (gate_array.c's INKR); a
// pen written from outside is held to the same seventeen.
#[test]
fn set_refuses_what_the_emulator_could_not_run_from() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    assert_eq!(
        session.refusal(
            "set",
            json!({ "handle": handle, "values": { "cpu.pc": 5, "gate_array.pen": 17 } })
        ),
        "gate_array.pen selects one of the seventeen inks, 0 to 16"
    );
    assert_eq!(session.inspect(&handle, "cpu.pc"), 0);
    session.set(&handle, json!({ "gate_array.pen": 16 }));
}

// A processor selects a RAM configuration by writing &C0 and its number, and
// "Ram management" (Kevin Thacker's cpctech),
// https://cpctech.cpcwiki.de/docs/rampage.html, gives the blocks each puts
// where: configuration 0 the first 64K's 0, 1, 2 and 3; 2 the second 64K's,
// all four; 4 the second's block 0 at &4000, among the first's 0, 2 and 3.
#[test]
fn setting_the_ram_configuration_moves_the_banking() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    session.set(&handle, json!({ "mmr": 0xC4 }));
    session.poke(&handle, json!("&4000"), "A5");
    session.set(&handle, json!({ "mmr": 0xC0 }));
    assert_eq!(session.peek(&handle, json!("&4000"), 1), "00");
    session.set(
        &handle,
        json!({ "mmr": 0xC2, "gate_array.lower_rom_enabled": false }),
    );
    assert_eq!(session.peek(&handle, json!("&0000"), 1), "A5");
    session.set(&handle, json!({ "mmr": 0xC4 }));
    assert_eq!(session.peek(&handle, json!("&4000"), 1), "A5");
}

// When a CPC's upper ROM is enabled "all CPU read operations between #C000 to
// #FFFF will read the ROM" and "all CPU write operations between #C000 to
// #FFFF will affect the 'underlying' RAM", as its lower ROM does at #0000 to
// #3FFF: "The Gate Array" (Grim), Upper ROM and Lower ROM,
// https://web.archive.org/web/20260710174425/https://www.grimware.org/doku.php/documentations/devices/gatearray. A
// Spectrum's ROM takes no write at all.
#[test]
fn memory_is_read_and_written_as_the_processor_would() {
    let roms = Roms::programmed("cpc464.rom", &[0x11, 0x22, 0x33]);
    roms.write_at("cpc464.rom", 0x4000, &[0x44]);
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc464");
    assert_eq!(session.peek(&cpc, json!(0), 3), "112233");
    session.poke(&cpc, json!("#0001"), "AB CD");
    assert_eq!(session.peek(&cpc, json!("$0000"), 3), "112233");
    session.set(&cpc, json!({ "gate_array.lower_rom_enabled": false }));
    assert_eq!(session.peek(&cpc, json!("0x0000"), 3), "00ABCD");
    assert_eq!(session.peek(&cpc, json!("&C000"), 1), "44");
    session.poke(&cpc, json!("&C000"), "EE");
    assert_eq!(session.peek(&cpc, json!("&C000"), 1), "44");
    session.set(&cpc, json!({ "gate_array.upper_rom_enabled": false }));
    assert_eq!(session.peek(&cpc, json!("&C000"), 1), "EE");
    let spectrum = session.open("spectrum48");
    session.poke(&spectrum, json!(0), "FF");
    session.poke(&spectrum, json!(0x8000), "FF");
    assert_eq!(session.peek(&spectrum, json!(0), 1), "00");
    assert_eq!(session.peek(&spectrum, json!("&8000"), 1), "FF");
}

#[test]
fn memory_ends_at_ffff_and_says_so() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    assert_eq!(session.peek(&handle, json!("&FFFF"), 1).len(), 2);
    assert_eq!(
        session.refusal(
            "peek",
            json!({ "handle": handle, "address": "&FFF0", "length": 17 })
        ),
        "17 bytes from &FFF0 run past &FFFF, where the processor's memory ends; 16 fit from there"
    );
    for length in [-1, 0, 4097] {
        assert_eq!(
            session.refusal(
                "peek",
                json!({ "handle": handle, "address": 0, "length": length })
            ),
            format!("{length} bytes is not from 1 to 4096")
        );
    }
    for address in [
        json!(65536),
        json!("C000"),
        json!("&10000"),
        json!("&"),
        json!("&+12"),
    ] {
        let refusal = session.refusal("peek", json!({ "handle": handle, "address": address }));
        assert!(
            refusal.ends_with(
                "an address is a number from 0 to 65535, or hexadecimal from &0000 to &FFFF, \
                 written with &, #, $ or 0x before it"
            ),
            "{refusal}"
        );
    }
    for bytes in ["ABC", "+F", "GG"] {
        assert_eq!(
            session.refusal(
                "poke",
                json!({ "handle": handle, "address": 0, "bytes": bytes })
            ),
            format!(
                "{bytes} is not bytes: bytes are written as pairs of hexadecimal digits, as \
                 3E05C9 or 3E 05 C9"
            )
        );
    }
}

// A CPC's RAM read by its own addresses, as its video hardware reads it:
// under the lower ROM at &0000, and the second 64K's first bank, which RAM
// configuration 4 pages in at &4000 (cpctech's "Ram management"), at &10000
// whatever is paged in.
#[test]
fn peek_reads_a_cpcs_ram_by_its_own_addresses() {
    let roms = Roms::programmed("cpc6128.rom", &[0x11]);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    session.poke(&handle, json!(0), "AA");
    session.set(&handle, json!({ "mmr": 0xC4 }));
    session.poke(&handle, json!("&4000"), "BB");
    session.set(&handle, json!({ "mmr": 0xC0 }));
    let peek = |session: &mut Session, arguments: Value| {
        let mut arguments = arguments;
        arguments["handle"] = json!(handle);
        session.answer("peek", arguments)
    };
    assert_eq!(
        peek(&mut session, json!({ "address": 0 })),
        json!({ "address": "&0000", "bytes": "11" })
    );
    assert_eq!(
        peek(&mut session, json!({ "ram": 0 })),
        json!({ "ram": "&0000", "bytes": "AA" })
    );
    assert_eq!(
        peek(&mut session, json!({ "ram": "&10000", "length": 2 })),
        json!({ "ram": "&10000", "bytes": "BB00" })
    );
    assert_eq!(
        peek(&mut session, json!({ "ram": "&1FFFF" })),
        json!({ "ram": "&1FFFF", "bytes": "00" })
    );
    assert_eq!(
        peek(&mut session, json!({ "ram": 65535 })),
        json!({ "ram": "&FFFF", "bytes": "00" })
    );
    let refused = |session: &mut Session, handle: &str, arguments: Value| {
        let mut arguments = arguments;
        arguments["handle"] = json!(handle);
        session.refusal("peek", arguments)
    };
    assert_eq!(
        refused(
            &mut session,
            &handle,
            json!({ "ram": "&1FFFF", "length": 2 })
        ),
        "2 bytes from &1FFFF run past &1FFFF, where the machine's RAM ends; 1 fit from there"
    );
    assert_eq!(
        refused(&mut session, &handle, json!({ "ram": 0, "length": 4097 })),
        "4097 bytes is not from 1 to 4096"
    );
    let either = "peek reads from address or from ram, one of the two: {\"address\": \"&C000\"} \
                  reads as the processor does, through the banking and the ROMs, and {\"ram\": \
                  \"&1C000\"} reads a CPC's RAM by its own addresses";
    assert_eq!(refused(&mut session, &handle, json!({})), either);
    assert_eq!(
        refused(&mut session, &handle, json!({ "address": 0, "ram": 0 })),
        either
    );
    assert!(
        refused(&mut session, &handle, json!({ "ram": "&20000" })).ends_with(
            "an address in the RAM is a number from 0 to 131071, or hexadecimal from &0000 to \
             &1FFFF, written with &, #, $ or 0x before it"
        )
    );
    let cpc464 = session.open("cpc464");
    assert_eq!(
        refused(&mut session, &cpc464, json!({ "ram": "&10000" })),
        "&10000 is past &FFFF, where the machine's RAM ends"
    );
    let spectrum = session.open("spectrum48");
    assert_eq!(
        refused(&mut session, &spectrum, json!({ "ram": "&4000" })),
        "ram reads a CPC's RAM by its own addresses, past its banking and its ROMs; a Spectrum's \
         RAM stands at &4000 to &FFFF, where address reads it"
    );
}

// A snapshot of version 1, laid out by "Snapshot (.SNA) file format" (Kevin
// Thacker's cpctech), https://cpctech.cpcwiki.de/docs/snapshot.html, each
// register a value of its own: the processor's from &11, "IFF0" and "IFF1" —
// the flip-flops the Z80's own manual calls IFF1 and IFF2 — at &1B and &1C,
// the Gate Array's pen and palette from &2E, its multi configuration at &40,
// the RAM configuration at &41, the 6845's from &42, the ROM selection at
// &55, the PPI's from &56, the PSG's from &5A, the size of the RAM dump in
// kilobytes at &6B, and the dump from &100. IFF0 clear and IFF1 set is a
// machine caught in its NMI handler.
fn cpc_snapshot(kilobytes: u16, version: u8) -> Vec<u8> {
    let mut snapshot = vec![0; 0x100 + usize::from(kilobytes) * 1024];
    snapshot[..8].copy_from_slice(b"MV - SNA");
    snapshot[0x10] = version;
    snapshot[0x11..0x1B]
        .copy_from_slice(&[0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0xD5, 0x3F]);
    snapshot[0x1B] = 0;
    snapshot[0x1C] = 1;
    snapshot[0x1D..0x25].copy_from_slice(&[0x12, 0x22, 0x42, 0x44, 0x00, 0xC0, 0x34, 0x12]);
    snapshot[0x25] = 1;
    snapshot[0x26..0x2E].copy_from_slice(&[0x92, 0x91, 0x94, 0x93, 0x96, 0x95, 0x98, 0x97]);
    snapshot[0x2E] = 5;
    for pen in 0..17 {
        snapshot[0x2F + pen] = 0x40 + u8::try_from(pen).unwrap();
    }
    // Mode 1 in its lowest two bits, and the lower and upper ROMs paged off by
    // bits 2 and 3, as "The Gate Array" (Grim), RMR, gives them; bit 7 set,
    // as cpctech's page asks, "for CPCEMU compatibility".
    snapshot[0x40] = 0x8D;
    snapshot[0x42] = 12;
    snapshot[0x43..0x55].copy_from_slice(&[
        63, 40, 46, 0x8E, 38, 0, 25, 30, 0, 7, 0, 0, 0x30, 0, 0, 0, 0, 0,
    ]);
    snapshot[0x55] = 7;
    snapshot[0x56..0x5A].copy_from_slice(&[0xA6, 0xB7, 0xC8, 0x82]);
    snapshot[0x5A] = 9;
    for register in 0..16 {
        snapshot[0x5B + register] = u8::try_from(register).unwrap();
    }
    snapshot[0x6B..0x6D].copy_from_slice(&kilobytes.to_le_bytes());
    snapshot[0x100 + 0x1234] = 0xC9;
    if kilobytes == 128 {
        snapshot[0x100 + 0x10000 + 0x0123] = 0xB4;
    }
    snapshot
}

// A CPC's snapshot is read for the fields of version 1, and the counters a
// version 3 carries are not, among them the 6845's character, row and line
// counters at &A9, &AB and &AC, and the Gate Array's interrupt counter at &B3
// ("Snapshot (.SNA) file format", cpctech). The machine starts its frame from
// the top, those counters and the monitor's beam at 0. The snapshot programs
// the firmware's screen: lines of 64 characters (R0 63), 64µs, 256 ticks at
// the Gate Array's 4MHz (Compendium ch. 4.1 and 4.4.4), rows of eight lines
// (R9 7), and the 6845's vertical sync at row 30 (R7), line 240. The Gate
// Array's frame sync begins a pixel after the end of the second of the
// 6845's line syncs within it, character 60 of line 241 (R2 46 and R3's
// width 14, ch. 16.2.2 and 16.2.3), and the monitor retraces once it has held
// a sync for 12µs (ch. 16.2.4, CPC_FRAME_SYNC_SAMPLES in the emulator's
// src/cpc.h), at character 8 of line 242, where the first run by frames ends
// with the NOP it stands in; the next takes a standard frame. The beam's
// line is 0 from the retrace until the monitor sees a line's sync begin
// again, at character 52 of line 242, where the line pulse the frame sync
// inverts ends (ch. 16.2.2), and from line 246 at character 48, two after
// the 6845's; so it counts 312 - 242 + 1 = 71 lines past the 6845's line
// once a line's sync has begun, and 70 before it. Before the retrace it
// counts one past for the line's sync, and one more for a line sync the Gate
// Array raises two characters after the load, its own counter H06 counting
// from 0 as it stands at power-on (the emulator's src/gate_array.c).
#[test]
fn a_cpc_opened_from_its_snapshot_starts_its_frame_from_the_top() {
    let roms = Roms::all();
    let mut snapshot = cpc_snapshot(64, 3);
    for (offset, counter) in [(0xA9, 20), (0xAB, 10), (0xAC, 3), (0xB3, 30)] {
        snapshot[offset] = counter;
    }
    let path = roms.write("program.sna", &snapshot);
    let mut session = Session::start(&roms);
    let handle = session.open_from("cpc464", &path);
    let counters = ["crtc.c0", "crtc.c4", "crtc.c9", "gate_array.r52"];
    for path in counters.iter().chain(&["monitor.beam_x", "monitor.beam_y"]) {
        assert_eq!(session.inspect(&handle, path), 0, "{path}");
    }
    let beam_past_the_6845 = |session: &mut Session| {
        let line = session.inspect(&handle, "crtc.c4").as_u64().unwrap() * 8
            + session.inspect(&handle, "crtc.c9").as_u64().unwrap();
        (session.inspect(&handle, "monitor.beam_y").as_u64().unwrap() + 312 - line) % 312
    };
    for _ in 0..5 {
        session.run(&handle, json!({ "ticks": 11111 }));
        assert!((1..=2).contains(&beam_past_the_6845(&mut session)));
    }
    let first = session.run(&handle, json!({ "frames": 1 }));
    assert_eq!(first["frames"], 1, "{first}");
    let retrace = 242 * 256 + 8 * 4;
    assert!(
        (retrace..retrace + 5).contains(&first["tick"].as_u64().unwrap()),
        "{first}"
    );
    for _ in 0..5 {
        session.run(&handle, json!({ "ticks": 11111 }));
        assert!((70..=71).contains(&beam_past_the_6845(&mut session)));
    }
    session.run(&handle, json!({ "frames": 1 }));
    let standard = session.run(&handle, json!({ "frames": 1 }));
    assert!(
        (CPC_FRAME_TICKS..CPC_FRAME_TICKS + 4).contains(&standard["ticks"].as_u64().unwrap()),
        "{standard}"
    );
}

// The 48K format of "Emulator file formats" (World of Spectrum FAQ),
// https://worldofspectrum.org/faq/reference/formats.htm: I, then HL', DE',
// BC' and AF', then HL, DE, BC, IY and IX, the interrupt byte, "bit 2 contains
// IFF2", R, AF and SP, the interrupt mode and the border, in 27 bytes, and
// the RAM from 16384. The program counter is on the stack, for "a RETN
// command is required to start the program".
fn spectrum_snapshot() -> Vec<u8> {
    let mut snapshot = vec![0; 27 + 49152];
    snapshot[0] = 0x3F;
    for (at, word) in [
        (1, 0x1A1Bu16),
        (3, 0x2A2B),
        (5, 0x3A3B),
        (7, 0x4A4B),
        (9, 0x5C5D),
        (11, 0x6A6B),
        (13, 0x7A7B),
        (15, 0x8A8B),
        (17, 0x9A9B),
        (21, 0x0123),
        (23, 0xFF00),
    ] {
        snapshot[at..at + 2].copy_from_slice(&u16::to_le_bytes(word));
    }
    snapshot[19] = 0b100;
    // R with bit 7 set, which no instruction counts, and bit 2 clear, unlike
    // the IFF2 beside it.
    snapshot[20] = 0xC9;
    snapshot[25] = 2;
    snapshot[26] = 5;
    snapshot[27 + 0xFF00 - 0x4000..27 + 0xFF02 - 0x4000].copy_from_slice(&0x8000u16.to_le_bytes());
    snapshot[27 + 0x8000 - 0x4000] = 0xC9;
    snapshot
}

fn a_cpc_opens_from_its_snapshot_of(version: u8) {
    let roms = Roms::all();
    let mut snapshot = cpc_snapshot(128, version);
    // RAM configuration 4: the second 64K's block 0 at &4000.
    snapshot[0x41] = 4;
    if version == 3 {
        // A 6128, at &6D, and a chunk of eight bytes under a name no reader
        // knows.
        snapshot[0x6D] = 2;
        snapshot.extend(b"MCPT");
        snapshot.extend(8u32.to_le_bytes());
        snapshot.extend([0; 8]);
    }
    let path = roms.write("program.sna", &snapshot);
    let mut session = Session::start(&roms);
    let opened = session.answer(
        "machine_open",
        json!({ "machine": "cpc6128", "snapshot": path }),
    );
    let handle = opened["handle"].as_str().expect("a handle").to_owned();
    let files: Vec<&str> = opened["files"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|file| file["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(
        files,
        [
            roms.0.join("cpc6128.rom").to_str().unwrap(),
            roms.0.join("amsdos.rom").to_str().unwrap(),
            path.to_str().unwrap(),
        ]
    );
    let cpu = session.inspect(&handle, "cpu");
    let registers =
        ["f", "a", "c", "b", "e", "d", "l", "h", "r", "i"].map(|name| cpu[name].clone());
    assert_eq!(
        registers,
        [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0xD5, 0x3F].map(|value| json!(value))
    );
    let pairs = ["ixl", "ixh", "iyl", "iyh", "sp", "pc", "im"].map(|name| cpu[name].clone());
    assert_eq!(
        pairs,
        [
            json!(0x12),
            json!(0x22),
            json!(0x42),
            json!(0x44),
            json!(0xC000),
            json!(0x1234),
            json!(1)
        ]
    );
    let alternates = ["af_", "bc_", "de_", "hl_"].map(|name| cpu[name].clone());
    assert_eq!(
        alternates,
        [0x9192, 0x9394, 0x9596, 0x9798].map(|value| json!(value))
    );
    assert_eq!((&cpu["iff1"], &cpu["iff2"]), (&json!(false), &json!(true)));
    let gate_array = session.inspect(&handle, "gate_array");
    assert_eq!(gate_array["pen"], 5);
    let inks: Vec<u8> = (0x40..0x51).map(|ink| ink & 0x1F).collect();
    assert_eq!(gate_array["inks"], json!(inks));
    assert_eq!(gate_array["mode"], 1);
    assert_eq!(
        (
            &gate_array["lower_rom_enabled"],
            &gate_array["upper_rom_enabled"]
        ),
        (&json!(false), &json!(false))
    );
    assert_eq!(session.inspect(&handle, "crtc.address_register"), 12);
    assert_eq!(
        session.inspect(&handle, "crtc.registers"),
        json!([
            63, 40, 46, 0x8E, 38, 0, 25, 30, 0, 7, 0, 0, 0x30, 0, 0, 0, 0, 0
        ])
    );
    assert_eq!(session.inspect(&handle, "upper_rom_number"), 7);
    assert_eq!(session.inspect(&handle, "psg.selected"), 9);
    let registers: Vec<u8> = (0..16).collect();
    assert_eq!(session.inspect(&handle, "psg.registers"), json!(registers));
    // "This byte represents the inputs to PPI port A", and to B, and "the
    // outputs from port C", "regardless of the input/output setting".
    let ppi = session.inspect(&handle, "ppi");
    assert_eq!(
        (&ppi["input"][0], &ppi["input"][1], &ppi["output"][2]),
        (&json!(0xA6), &json!(0xB7), &json!(0xC8))
    );
    assert_eq!(ppi["control"], 0x82);
    assert_eq!(session.inspect(&handle, "mmr"), 0xC4);
    assert_eq!(session.peek(&handle, json!("&1234"), 1), "C9");
    assert_eq!(session.peek(&handle, json!("&4123"), 1), "B4");
    let step = session.step(&handle, 1);
    // The RET there returns to the word on the stack at &C000, which is 0.
    assert_eq!(
        (&step["pc"], &step["tick"]),
        (&ampersand(0), &step["ticks"]),
        "{step}"
    );
}

#[test]
fn a_cpc_opens_from_its_snapshot() {
    a_cpc_opens_from_its_snapshot_of(1);
}

// "Version 3 is the most recent", and adds fields of its own past those the
// three versions share, which are read as version 1's. "Immediatly following
// the memory dump there is optional data which is seperated into chunks", and
// "If a emulator finds a chunk which it does not support then it should skip
// the chunk" ("Snapshot (.SNA) file format").
#[test]
fn a_cpc_snapshot_of_version_3_opens_for_the_fields_of_version_1() {
    a_cpc_opens_from_its_snapshot_of(3);
}

#[test]
fn a_spectrum_opens_from_its_snapshot() {
    let roms = Roms::all();
    let path = roms.write("program.sna", &spectrum_snapshot());
    let mut session = Session::start(&roms);
    let handle = session.open_from("spectrum48", &path);
    let cpu = session.inspect(&handle, "cpu");
    let pairs = ["hl_", "de_", "bc_", "af_"].map(|name| cpu[name].clone());
    assert_eq!(
        pairs,
        [0x1A1B, 0x2A2B, 0x3A3B, 0x4A4B].map(|value| json!(value))
    );
    let registers = [
        "l", "h", "e", "d", "c", "b", "iyl", "iyh", "ixl", "ixh", "f", "a", "i", "r", "im",
    ]
    .map(|name| cpu[name].clone());
    assert_eq!(
        registers,
        [
            0x5D, 0x5C, 0x6B, 0x6A, 0x7B, 0x7A, 0x8B, 0x8A, 0x9B, 0x9A, 0x23, 0x01, 0x3F, 0xC9, 2
        ]
        .map(|value| json!(value))
    );
    assert_eq!(
        (&cpu["pc"], &cpu["sp"]),
        (&json!(0x8000), &json!(0xFF02)),
        "{cpu}"
    );
    assert_eq!((&cpu["iff1"], &cpu["iff2"]), (&json!(true), &json!(true)));
    assert_eq!(session.inspect(&handle, "ula.border"), 5);
    assert_eq!(session.peek(&handle, json!("&8000"), 1), "C9");
}

#[test]
fn a_snapshot_the_machine_cannot_take_is_refused_with_what_it_can() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc_takes = "a CPC opens an SNA snapshot of version 1, 2 or 3 holding no more RAM than it \
                     has, in the dump that follows its header: 128K on a cpc6128, 64K on a \
                     cpc664 or a cpc464, which have no PAL and take only RAM configuration 0";
    let not_a_snapshot = roms.write("notes.sna", b"nothing to see");
    assert_eq!(
        session.refusal(
            "machine_open",
            json!({ "machine": "cpc6128", "snapshot": not_a_snapshot })
        ),
        format!(
            "{} too short to hold a snapshot header; {cpc_takes}",
            not_a_snapshot.display()
        )
    );
    let six_one_two_eight = roms.write("6128.sna", &cpc_snapshot(128, 1));
    let refusal = session.refusal(
        "machine_open",
        json!({ "machine": "cpc464", "snapshot": six_one_two_eight }),
    );
    assert_eq!(
        refusal,
        format!(
            "{} holds more RAM than this machine has; {cpc_takes}",
            six_one_two_eight.display()
        )
    );
    assert_eq!(
        session.refusal(
            "machine_open",
            json!({ "machine": "spectrum48", "snapshot": six_one_two_eight })
        ),
        format!(
            "{} is not a file of at most 49179 bytes; a spectrum48 opens a 48K .sna snapshot of \
             49179 bytes",
            six_one_two_eight.display()
        )
    );
    assert_eq!(
        session.refusal(
            "machine_open",
            json!({ "machine": "cpc6128", "snapshot": roms.0 })
        ),
        format!(
            "{} is not a file of at most 8388608 bytes; {cpc_takes}",
            roms.0.display()
        )
    );
    // RAM configuration 4 is the second 64K's ("Ram management"), which a
    // 64K machine has not got.
    let mut configured = cpc_snapshot(64, 1);
    configured[0x41] = 4;
    let configured = roms.write("configured.sna", &configured);
    assert_eq!(
        session.refusal(
            "machine_open",
            json!({ "machine": "cpc464", "snapshot": configured })
        ),
        format!(
            "{} needs a PAL this machine does not have; {cpc_takes}",
            configured.display()
        )
    );
    let missing = roms.0.join("missing.sna");
    let refusal = session.refusal(
        "machine_open",
        json!({ "machine": "cpc6128", "snapshot": missing }),
    );
    assert!(
        refusal.starts_with(&format!("cannot read {}", missing.display())),
        "{refusal}"
    );
    assert!(session.handles().is_empty());
}

// What a run or a call answered it matched or gathered, each as its kind, its
// address or port, its byte and where the instruction that made it begins.
fn seen(list: &Value) -> Vec<(&'static str, u64, Option<u64>, Option<u64>)> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|seen| {
            let kind = ["execute", "read", "write", "in", "out"]
                .into_iter()
                .find(|kind| seen.get(*kind).is_some())
                .expect("a kind");
            (
                kind,
                after_ampersand(&seen[kind], 4),
                seen.get("value").map(|value| after_ampersand(value, 2)),
                seen.get("pc").map(|pc| after_ampersand(pc, 4)),
            )
        })
        .collect()
}

// Writes from &0000 on, whose places the expected matches name: LD (HL),n at
// &0007, PUSH HL at &0009, high byte first, and LD (nn),HL at &000A, low byte
// first (Z80 CPU User Manual, UM0080).
const WRITING: [u8; 14] = [
    DI, LD_SP, 0x00, 0x80, LD_HL, 0x00, 0x90, LD_AT_HL, 0xAA, PUSH_HL, LD_HL_TO, 0x10, 0x90, HALT,
];

#[test]
fn a_write_names_the_instruction_that_made_it() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &WRITING);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let gathering = json!([
            { "write": "&7FFE", "to": "&7FFF" },
            { "write": "&9000", "to": "&90FF" },
        ]);
        let ran = session.run(&handle, json!({ "ticks": 200, "gather": gathering }));
        assert_eq!(ran["stopped"], "halted", "{ran}");
        assert_eq!(
            seen(&ran["gathered"]),
            [
                ("write", 0x9000, Some(0xAA), Some(0x0007)),
                ("write", 0x7FFF, Some(0x90), Some(0x0009)),
                ("write", 0x7FFE, Some(0x00), Some(0x0009)),
                ("write", 0x9010, Some(0x00), Some(0x000A)),
                ("write", 0x9011, Some(0x90), Some(0x000A)),
            ],
            "{machine}: {ran}"
        );
        let ticks: Vec<u64> = ran["gathered"]
            .as_array()
            .unwrap()
            .iter()
            .map(|seen| seen["tick"].as_u64().unwrap())
            .collect();
        assert!(
            ticks.is_sorted() && ticks[4] <= ran["tick"].as_u64().unwrap(),
            "{ran}"
        );
        for seen in ran["gathered"].as_array().unwrap() {
            assert!(seen["beam_x"].is_u64() && seen["beam_y"].is_u64(), "{seen}");
        }
        // Stopped by a write, the machine stands at the end of the
        // instruction that made it; what it gathered on the way is listed
        // apart from what stopped it.
        let twin = session.open(machine);
        let until = json!([{ "write": "&9010" }]);
        let gathering = json!([{ "write": "&9000" }]);
        let stopped = session.run(
            &twin,
            json!({ "ticks": 200, "until": until, "gather": gathering }),
        );
        assert_eq!(
            (&stopped["stopped"], &stopped["pc"]),
            (&json!("matched"), &ampersand(0x000D)),
            "{machine}: {stopped}"
        );
        assert_eq!(
            seen(&stopped["matched"]),
            [("write", 0x9010, Some(0x00), Some(0x000A))],
            "{machine}: {stopped}"
        );
        assert_eq!(
            seen(&stopped["gathered"]),
            [("write", 0x9000, Some(0xAA), Some(0x0007))],
            "{machine}: {stopped}"
        );
    }
}

// LD A,n's n and LD A,(nn)'s nn are the instruction's own bytes; LD A,(nn) at
// &0002 reads the first byte of its own nn, &0003, as data, and the byte it
// gives is that one, 3, and not the 0 fetched just before it.
#[test]
fn an_instructions_own_bytes_are_not_read_as_data() {
    let program = [LD_A, 0x05, LD_A_FROM, 0x03, 0x00, HALT];
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &program);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let gathering = json!([{ "read": 0, "to": 255 }]);
        let ran = session.run(&handle, json!({ "ticks": 100, "gather": gathering }));
        assert_eq!(
            seen(&ran["gathered"]),
            [("read", 0x0003, Some(0x03), Some(0x0002))],
            "{machine}: {ran}"
        );
    }
}

// From &9000 on, round again incrementing the byte at HL and HL itself; the
// loop begins at &0004.
const COUNTING_FROM_9000: [u8; 8] = [DI, LD_HL, 0x00, 0x90, INC_AT_HL, INC_HL, JR, 0xFC];

// A run that begins where it was stopped makes one pass of the loop, INC
// (HL), INC HL and JR, before it stops again: 11, 6 and 12 T-states from a
// Spectrum's ROM, where nothing is contended (UM0080). A CPC's board holds
// its instructions to whole microseconds, and where on its four-tick grid
// one ends moves with the instruction stopped after, so its pass is told by
// the byte it counts.
#[test]
fn a_run_stopped_before_an_instruction_runs_it_first_when_it_runs_on() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &COUNTING_FROM_9000);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let until = json!([{ "execute": "&0004" }]);
        let first = session.run(&handle, json!({ "ticks": 10000, "until": until }));
        assert_eq!(
            (&first["stopped"], &first["pc"]),
            (&json!("matched"), &ampersand(4)),
            "{machine}: {first}"
        );
        assert_eq!(
            seen(&first["matched"]),
            [("execute", 4, None, None)],
            "{machine}: {first}"
        );
        assert_eq!(
            first["matched"][0]["tick"], first["tick"],
            "{machine}: {first}"
        );
        assert_eq!(session.peek(&handle, json!("&9000"), 1), "00", "{machine}");
        let second = session.run(&handle, json!({ "ticks": 10000, "until": until }));
        assert_eq!(second["stopped"], "matched", "{machine}: {second}");
        if machine == "spectrum48" {
            assert_eq!(second["ticks"], 11 + 6 + 12, "{second}");
        }
        assert_eq!(
            seen(&second["matched"]),
            [("execute", 4, None, None)],
            "{machine}: {second}"
        );
        assert_eq!(
            session.peek(&handle, json!("&9000"), 2),
            "0100",
            "{machine}"
        );
    }
}

// Each pass reads a byte as data and writes it back one more, sends it to a
// port, and pushes and pops HL, through a stack at &8000; the pass begins at
// &000A, and the instruction at &000D sends the byte.
fn passing(port: u16) -> Vec<u8> {
    let [low, high] = port.to_le_bytes();
    vec![
        DI, LD_SP, 0x00, 0x80, LD_HL, 0x00, 0x90, LD_BC, low, high, LD_A_AT_HL, INC_A, LD_AT_HL_A,
        OUT_C_A[0], OUT_C_A[1], INC_L, PUSH_HL, POP_DE, JR, 0xF6,
    ]
}

// A run stopped at each match in turn, and its twin gathering over the same
// ticks, see the same matches. The instruction the last stop stands before
// has not begun, so the gathering, which ends there too, has not seen it.
#[test]
fn gathering_a_stretch_is_stopping_at_every_match_across_it() {
    for (file, machine, port) in [
        (
            "cpc6128.rom",
            "cpc6128",
            json!({ "out": "&7F00", "mask": "&C000" }),
        ),
        (
            "spectrum48.rom",
            "spectrum48",
            json!({ "out": "&FE", "mask": "&01" }),
        ),
    ] {
        let port_address = if machine == "cpc6128" { 0x7F00 } else { 0x00FE };
        let roms = Roms::programmed(file, &passing(port_address));
        let mut session = Session::start(&roms);
        let conditions = json!([
            { "execute": "&000D" },
            { "read": "&9000", "to": "&90FF" },
            { "write": "&9000", "to": "&90FF" },
            { "write": "&7FFE", "to": "&7FFF" },
            port,
        ]);
        let stopping = session.open(machine);
        let mut stops = Vec::new();
        let mut ticks = 0;
        for _ in 0..40 {
            let stopped = session.run(&stopping, json!({ "ticks": 10000, "until": conditions }));
            assert_eq!(stopped["stopped"], "matched", "{machine}: {stopped}");
            ticks = stopped["tick"].as_u64().unwrap();
            stops.extend(stopped["matched"].as_array().unwrap().iter().cloned());
        }
        let last = stops
            .iter()
            .rposition(|seen| seen.get("execute").is_none() || seen["tick"] != ticks)
            .unwrap();
        stops.truncate(last + 1);
        let gathering = session.open(machine);
        let gathered = session.run(&gathering, json!({ "ticks": ticks, "gather": conditions }));
        assert_eq!(gathered["tick"], ticks, "{machine}: {gathered}");
        assert_eq!(gathered["gathered"], json!(stops), "{machine}");
        assert!(
            seen(&gathered["gathered"])
                .iter()
                .any(|(kind, _, _, _)| *kind == "out"),
            "{machine}: {gathered}"
        );
    }
}

#[test]
fn a_gathering_that_fills_stops_and_the_next_run_gathers_on() {
    let roms = Roms::programmed("cpc6128.rom", &COUNTING_FROM_9000);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    let gathering = json!([{ "write": "&9000", "to": "&FFFF" }]);
    let mut next = 0x9000;
    for _ in 0..3 {
        let ran = session.run(&handle, json!({ "frames": 1, "gather": gathering }));
        assert_eq!(ran["stopped"], "full", "{ran}");
        let writes = seen(&ran["gathered"]);
        assert_eq!(writes.len(), 128, "{ran}");
        for (kind, address, value, pc) in writes {
            assert_eq!(
                (kind, address, value, pc),
                ("write", next, Some(1), Some(4))
            );
            next += 1;
        }
    }
}

// One byte written by LD (nn),A, and then PUSH HL over and over: the 128th
// write is the first of a PUSH's two, and the second is kept with it.
#[test]
fn a_gathering_fills_at_the_end_of_the_instruction_that_filled_it() {
    let program = [
        DI, LD_SP, 0x00, 0x90, LD_TO_A, 0x00, 0x80, PUSH_HL, JR, 0xFD,
    ];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let gathering = json!([{ "write": "&4000", "to": "&FFFF" }]);
    let ran = session.run(&handle, json!({ "ticks": 10000, "gather": gathering }));
    let writes = seen(&ran["gathered"]);
    assert_eq!(
        (&ran["stopped"], writes.len()),
        (&json!("full"), 129),
        "{ran}"
    );
    assert_eq!(writes[127].3, writes[128].3, "{ran}");
    assert_eq!(ran["pc"], ampersand(8), "{ran}");
}

// A CPC's Gate Array answers &7Fxx, A15 low and A14 high ("The Gate Array",
// Grim, https://web.archive.org/web/20260710174425/https://www.grimware.org/doku.php/documentations/devices/gatearray),
// and "Every even I/O address will address the ULA" of a Spectrum (comp.sys.sinclair
// FAQ, 16K/48K reference, https://worldofspectrum.org/faq/reference/48kreference.htm).
#[test]
fn a_port_matches_on_the_bits_its_device_decodes() {
    let cpc_program = [
        DI, LD_BC, 0x10, 0x7F, OUT_C_C[0], OUT_C_C[1], LD_BC, 0x54, 0x7E, OUT_C_C[0], OUT_C_C[1],
        LD_BC, 0x01, 0xBC, OUT_C_C[0], OUT_C_C[1], HALT,
    ];
    let roms = Roms::programmed("cpc6128.rom", &cpc_program);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    let gathering = json!([{ "out": "&7F00", "mask": "&C000" }]);
    let ran = session.run(&handle, json!({ "ticks": 200, "gather": gathering }));
    assert_eq!(
        seen(&ran["gathered"]),
        [
            ("out", 0x7F10, Some(0x10), Some(0x0004)),
            ("out", 0x7E54, Some(0x54), Some(0x0009)),
        ],
        "{ran}"
    );
    let spectrum_program = [
        DI, LD_A, 0x07, OUT_N_A, 0xFE, OUT_N_A, 0xFF, IN_A_N, 0xFE, HALT,
    ];
    let roms = Roms::programmed("spectrum48.rom", &spectrum_program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let gathering = json!([
        { "out": "&FE", "mask": "&01" },
        { "in": "&FE", "mask": "&01" },
    ]);
    let ran = session.run(&handle, json!({ "ticks": 200, "gather": gathering }));
    let ports: Vec<_> = seen(&ran["gathered"])
        .into_iter()
        .map(|(kind, port, _, pc)| (kind, port, pc))
        .collect();
    assert_eq!(
        ports,
        [("out", 0x07FE, Some(0x0003)), ("in", 0x07FE, Some(0x0007))],
        "{ran}"
    );
    // The byte read is the one IN A,(n) left in A.
    let read = &ran["gathered"][1]["value"];
    assert_eq!(
        json!(after_ampersand(read, 2)),
        session.inspect(&handle, "cpu.a"),
        "{ran}"
    );
}

// Interrupt mode 1 pushes the program counter and runs from &0038 (UM0080),
// where the handler writes a byte with LD (HL),n, takes interrupts again and
// returns.
#[test]
fn what_the_response_to_an_interrupt_writes_names_no_instruction() {
    let mut program = vec![
        LD_SP, 0x00, 0x80, LD_HL, 0x00, 0x90, IM_1[0], IM_1[1], EI, HALT, JR,
    ];
    program.push(0xFD);
    program.resize(0x38, NOP);
    program.extend([LD_AT_HL, 0x55, EI, RET]);
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let gathering = json!([{ "write": "&7FFE", "to": "&7FFF" }, { "write": "&9000" }]);
    let ran = session.run(&handle, json!({ "frames": 2, "gather": gathering }));
    let writes = seen(&ran["gathered"]);
    assert!(writes.len() >= 3 && writes.len().is_multiple_of(3), "{ran}");
    for answered in writes.chunks(3) {
        assert_eq!(
            answered,
            [
                ("write", 0x7FFF, Some(0x00), None),
                ("write", 0x7FFE, Some(0x0A), None),
                ("write", 0x9000, Some(0x55), Some(0x0038)),
            ],
            "{ran}"
        );
    }
}

// A processor halted on HALT idles on NOPs until an interrupt wakes it
// (UM0080), and none of them is an instruction the program begins.
#[test]
fn a_halted_processor_begins_no_instruction_while_it_idles() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &[DI, HALT]);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let gathering = json!([{ "execute": 0, "to": "&FFFF" }]);
        let ran = session.run(&handle, json!({ "ticks": 1000, "gather": gathering }));
        assert_eq!(
            (&ran["stopped"], seen(&ran["gathered"])),
            (
                &json!("halted"),
                vec![("execute", 0, None, None), ("execute", 1, None, None)]
            ),
            "{machine}: {ran}"
        );
        let idled = session.run(&handle, json!({ "ticks": 1000, "gather": gathering }));
        assert_eq!(
            (&idled["stopped"], &idled["gathered"]),
            (&json!("ticks"), &json!([])),
            "{machine}: {idled}"
        );
        let until = json!([{ "execute": 0, "to": "&FFFF" }]);
        let idled = session.run(&handle, json!({ "ticks": 1000, "until": until }));
        assert_eq!(idled["stopped"], "ticks", "{machine}: {idled}");
    }
}

// HALT with interrupts disabled waits for good on either machine: IFF1 shuts
// out INT, and neither raises NMI. A run stops at the end of it and says so,
// and one that begins there runs as asked; with interrupts enabled, an
// interrupt wakes the processor, and a run goes on. DI 4 T-states, LD A,n 7
// and HALT 4 (UM0080), from a Spectrum's ROM, where nothing is contended.
#[test]
fn a_run_stops_where_its_processor_halts_for_good() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &[DI, LD_A, 0x05, HALT]);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let halted = session.run(&handle, json!({ "frames": 5 }));
        assert_eq!(
            (&halted["stopped"], &halted["frames"], &halted["settled"]),
            (&json!("halted"), &json!(0), &json!(true)),
            "{machine}: {halted}"
        );
        if machine == "spectrum48" {
            assert_eq!(halted["ticks"], 4 + 7 + 4, "{halted}");
        }
        assert_eq!(session.inspect(&handle, "cpu.halted"), true, "{machine}");
        let ran = session.run(&handle, json!({ "ticks": 1000 }));
        assert_eq!(
            (&ran["stopped"], &ran["pc"]),
            (&json!("ticks"), &halted["pc"]),
            "{machine}: {ran}"
        );
        // A halted processor idles on fetches of 4 T-states (UM0080, HALT
        // Exit), a microsecond each on a CPC too.
        assert_eq!(ran["ticks"], 1000, "{machine}: {ran}");
    }
    // IFF1 gates INT and IFF2 only keeps a copy of it, which accepting an NMI
    // leaves as it clears IFF1 (UM0080, Interrupt Enable/Disable), so IFF2
    // set beside IFF1 clear, as an NMI taken with interrupts enabled leaves
    // them, wakes nothing.
    let roms = Roms::programmed("spectrum48.rom", &[HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.set(&handle, json!({ "cpu.iff2": true }));
    let halted = session.run(&handle, json!({ "frames": 2 }));
    assert_eq!(halted["stopped"], "halted", "{halted}");
    // An NMI written in with set wakes a processor halted for good: it
    // answers from &0066 (UM0080, Nonmaskable Interrupt Response), where RETN
    // returns to the address pushed, past the HALT, since the emulator's
    // z80.c holds the program counter after the HALT's fetch, and the next
    // HALT with interrupts disabled stops the run as it stops any.
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT, NOP, DI, HALT]);
    roms.write_at("spectrum48.rom", 0x66, &RETN);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let first = session.run(&handle, json!({ "ticks": 1000 }));
    session.set(&handle, json!({ "cpu.nmi_pending": true }));
    let second = session.run(&handle, json!({ "ticks": 1000 }));
    assert_eq!(
        (&first["stopped"], &second["stopped"]),
        (&json!("halted"), &json!("halted")),
        "{first} {second}"
    );
    assert!(after_ampersand(&second["pc"], 4) > after_ampersand(&first["pc"], 4));
    // With IFF1 set, each frame's interrupt wakes the processor, and its
    // handler returns to a JR back to the HALT, which halts it again.
    let roms = Roms::programmed("spectrum48.rom", &[IM_1[0], IM_1[1], EI, HALT, JR, 0xFD]);
    roms.write_at("spectrum48.rom", 0x38, &[EI, RET]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let ran = session.run(&handle, json!({ "frames": 3 }));
    assert_eq!(ran["stopped"], "frames", "{ran}");
}

// The 128th execute gathered is a HALT that leaves interrupts disabled, so
// the gathering fills and the processor halts for good at the same
// instruction's end; the answer gives the halt, since gathering on from there
// would gather nothing.
#[test]
fn a_halt_for_good_is_answered_before_a_gathering_it_fills() {
    let mut program = vec![DI];
    program.extend([NOP; 126]);
    program.push(HALT);
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let gathering = json!([{ "execute": 0, "to": "&FFFF" }]);
    let ran = session.run(&handle, json!({ "ticks": 10000, "gather": gathering }));
    assert_eq!(ran["stopped"], "halted", "{ran}");
    assert_eq!(ran["gathered"].as_array().unwrap().len(), 128, "{ran}");
}

// LD SP,0 and back to &0000: the machine comes to &0000 with its stack
// pointer at 0 again and again, and only a call returns.
#[test]
fn only_a_call_returns() {
    let roms = Roms::programmed("spectrum48.rom", &[LD_SP, 0x00, 0x00, JR, 0xFB]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let gathering = json!([{ "execute": 0 }]);
    let ran = session.run(&handle, json!({ "ticks": 200, "gather": gathering }));
    assert_eq!(ran["stopped"], "ticks", "{ran}");
}

// A routine at &0100 adds B to A and returns.
fn routine(file: &str, program: &[u8]) -> Roms {
    let roms = Roms::programmed(file, program);
    roms.write_at(file, 0x0100, &[ADD_A_B, RET]);
    roms
}

#[test]
fn a_called_routine_returns_with_its_registers_and_leaves_the_machine_where_it_was() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = routine(file, &COUNTING_FROM_9000);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        session.step(&handle, 3);
        let before = session.inspect(&handle, "cpu");
        let called = session.answer(
            "call",
            json!({
                "handle": handle,
                "address": "&0100",
                "values": { "cpu.a": 2, "cpu.b": 3, "cpu.sp": 0x8000 },
            }),
        );
        let pc = u16::try_from(before["pc"].as_u64().unwrap()).unwrap();
        assert_eq!(
            (&called["stopped"], &called["pc"]),
            (&json!("returned"), &ampersand(pc)),
            "{machine}: {called}"
        );
        // ADD A,r 4 T-states and RET 10 (UM0080), from a Spectrum's ROM, where
        // nothing is contended.
        if machine == "spectrum48" {
            assert_eq!(called["ticks"], 4 + 10, "{called}");
        }
        assert_eq!(session.inspect(&handle, "cpu.a"), 5, "{machine}");
        assert_eq!(session.inspect(&handle, "cpu.sp"), 0x8000, "{machine}");
        assert_eq!(
            session.peek(&handle, json!("&7FFE"), 2),
            "0500",
            "{machine}"
        );
        // It runs on from where it stood, at INC HL.
        let stepped = session.step(&handle, 1);
        assert_eq!(stepped["pc"], ampersand(6), "{machine}: {stepped}");
    }
}

// A call counts what its routine does, ADD A,B and RET at &0100 and the two
// bytes the RET reads, and not the instruction it returns to, which the
// machine then stands before.
#[test]
fn a_call_counts_what_its_routine_does() {
    let roms = routine("spectrum48.rom", &COUNTING_FROM_9000);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.step(&handle, 3);
    let called = session.answer(
        "call",
        json!({
            "handle": handle,
            "address": "&0100",
            "values": { "cpu.sp": 0x8000 },
            "count": [{ "execute": 0, "to": "&FFFF" }, { "read": "&7FFE", "to": "&7FFF" }],
        }),
    );
    assert_eq!(called["stopped"], "returned", "{called}");
    assert_eq!(
        called["counted"],
        json!([
            { "execute": "&0100", "count": 1 },
            { "execute": "&0101", "count": 1 },
            { "read": "&7FFE", "count": 1 },
            { "read": "&7FFF", "count": 1 },
        ])
    );
}

// The call stands the machine at &0200, where a RET lies. The routine calls
// &0200 itself, which returns to it, before it loads A and returns; arriving
// at &0200 the first time, deeper in the stack, is not the call's return.
#[test]
fn a_call_ends_at_its_own_return_and_not_at_another_arrival_there() {
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT]);
    roms.write_at(
        "spectrum48.rom",
        0x0100,
        &[0xCD, 0x00, 0x02, LD_A, 0x42, RET],
    );
    roms.write_at("spectrum48.rom", 0x0200, &[RET]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let called = session.answer(
        "call",
        json!({ "handle": handle, "address": 256, "values": { "cpu.pc": 512 } }),
    );
    assert_eq!(
        (&called["stopped"], &called["pc"]),
        (&json!("returned"), &ampersand(0x0200)),
        "{called}"
    );
    assert_eq!(session.inspect(&handle, "cpu.a"), 0x42);
}

#[test]
fn a_processor_halted_when_called_is_halted_again_once_the_routine_returns() {
    let roms = routine("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 100 }));
    assert_eq!(session.inspect(&handle, "cpu.halted"), true);
    let pc = session.inspect(&handle, "cpu.pc");
    let called = session.answer(
        "call",
        json!({ "handle": handle, "address": 256, "values": { "cpu.a": 1, "cpu.b": 1 } }),
    );
    assert_eq!(called["stopped"], "returned", "{called}");
    assert_eq!(session.inspect(&handle, "cpu.a"), 2);
    assert_eq!(session.inspect(&handle, "cpu.halted"), true);
    assert_eq!(session.inspect(&handle, "cpu.pc"), pc);
}

// A routine that waits on HALT for the next frame, called where IFF1 is
// clear, as from inside an interrupt's handler, waits for good, and the call
// stops there rather than at its frames, with the return address still on
// the stack. LD A,n 7 T-states and HALT 4 (UM0080).
#[test]
fn a_call_stops_where_its_routine_halts_for_good() {
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &[DI, HALT]);
        roms.write_at(file, 0x0100, &[LD_A, 0x05, HALT, RET]);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        let called = session.answer(
            "call",
            json!({ "handle": handle, "address": "&0100", "values": { "cpu.sp": "&8000" } }),
        );
        assert_eq!(
            (&called["stopped"], &called["frames"]),
            (&json!("halted"), &json!(0)),
            "{machine}: {called}"
        );
        if machine == "spectrum48" {
            assert_eq!(called["ticks"], 7 + 4, "{called}");
        }
        assert_eq!(session.inspect(&handle, "cpu.a"), 5, "{machine}");
        assert_eq!(session.inspect(&handle, "cpu.halted"), true, "{machine}");
        assert_eq!(session.inspect(&handle, "cpu.sp"), 0x7FFE, "{machine}");
    }
}

#[test]
fn a_call_that_cannot_return_is_refused_and_changes_nothing() {
    // A CPC stands up with its upper ROM enabled and its stack pointer at
    // &FFFF, under it.
    let roms = routine("cpc6128.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    assert_eq!(
        session.inspect(&handle, "gate_array.upper_rom_enabled"),
        true
    );
    assert_eq!(session.inspect(&handle, "cpu.sp"), 0xFFFF);
    let before = session.inspect(&handle, "cpu");
    let refusal = session.refusal(
        "call",
        json!({ "handle": handle, "address": "&0100", "values": { "cpu.a": 9 } }),
    );
    assert_eq!(
        refusal,
        "cannot call &0100: the return address would be pushed under a ROM, which answers the \
         processor's reads there, and the routine's return would not read it back; give cpu.sp \
         in values, pointing into RAM that no ROM enabled covers"
    );
    assert_eq!(session.inspect(&handle, "cpu"), before);
    let roms = routine("spectrum48.rom", &[DI, LD_SP, 0x00, 0x40, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.step(&handle, 2);
    let refusal = session.refusal("call", json!({ "handle": handle, "address": 256 }));
    assert!(refusal.contains("pushed into the ROM"), "{refusal}");
    for (file, machine) in [("cpc6128.rom", "cpc6128"), ("spectrum48.rom", "spectrum48")] {
        let roms = Roms::programmed(file, &[PREFIX_DD; 0x4000]);
        let mut session = Session::start(&roms);
        let handle = session.open(machine);
        session.step(&handle, 1);
        let refusal = session.refusal(
            "call",
            json!({ "handle": handle, "address": 256, "values": { "cpu.sp": 0x8000 } }),
        );
        assert_eq!(
            refusal,
            "cannot call &0100: the processor stands inside a chain of prefixes, and a routine \
             is called only between instructions; step or run on to the end of the chain first",
            "{machine}"
        );
    }
}

// A loop that stands at &0006 with interrupts on in mode 1, whose handler at
// &0038 writes &55 to &9000 and keeps A; and a routine at &0100 that counts
// BC down from &2000, three frames' worth, then writes A to &9001.
fn waiting(file: &str) -> Roms {
    let mut program = vec![LD_SP, 0x00, 0x80, IM_1[0], IM_1[1], EI, JR, 0xFE];
    program.resize(0x38, NOP);
    program.extend([PUSH_AF, LD_A, 0x55, LD_TO_A, 0x00, 0x90, POP_AF, EI, RET]);
    let roms = Roms::programmed(file, &program);
    let routine = [
        LD_BC, 0x00, 0x20, DEC_BC, LD_A_B, OR_C, JR_NZ, 0xFB, LD_TO_A, 0x01, 0x90, RET,
    ];
    roms.write_at(file, 0x0100, &routine);
    roms
}

#[test]
fn a_call_runs_the_interrupts_handlers_within_it_and_gathers_what_they_do() {
    let roms = waiting("spectrum48.rom");
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.step(&handle, 3);
    let gathering = json!([{ "write": "&9000", "to": "&9001" }]);
    let called = session.answer(
        "call",
        json!({ "handle": handle, "address": 256, "frames": 10, "gather": gathering }),
    );
    assert_eq!(
        (&called["stopped"], &called["pc"]),
        (&json!("returned"), &ampersand(6)),
        "{called}"
    );
    let writes = seen(&called["gathered"]);
    let (last, handled) = writes.split_last().expect("writes");
    assert_eq!(*last, ("write", 0x9001, Some(0), Some(0x0108)), "{called}");
    assert!(handled.len() >= 2, "{called}");
    for write in handled {
        assert_eq!(
            *write,
            ("write", 0x9000, Some(0x55), Some(0x003B)),
            "{called}"
        );
    }
}

// A routine the call stopped waiting for runs on with the next run, which
// stops at its return when until asks for the address it was called from.
#[test]
fn a_routine_that_has_not_returned_runs_on_with_the_next_run() {
    let roms = waiting("spectrum48.rom");
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.step(&handle, 3);
    let called = session.answer(
        "call",
        json!({ "handle": handle, "address": 256, "frames": 1 }),
    );
    assert_eq!(called["stopped"], "frames", "{called}");
    let pc = after_ampersand(&called["pc"], 4);
    assert!(
        (0x0100..0x010C).contains(&pc) || (0x0038..0x0041).contains(&pc),
        "{called}"
    );
    let until = json!([{ "execute": 6 }]);
    let ran = session.run(&handle, json!({ "frames": 10, "until": until }));
    assert_eq!(
        (&ran["stopped"], &ran["pc"]),
        (&json!("matched"), &ampersand(6)),
        "{ran}"
    );
    assert_eq!(session.inspect(&handle, "cpu.sp"), 0x8000);
    assert_eq!(session.peek(&handle, json!("&9001"), 1), "00");
}

// Every argument but the handle and what to call is the agent's to leave out.
#[test]
fn run_and_call_ask_for_nothing_they_can_go_without() {
    let roms = Roms::all();
    let tools = tools(&mut Session::start(&roms));
    assert_eq!(input_schema(&tools, "run")["required"], json!(["handle"]));
    assert_eq!(
        input_schema(&tools, "call")["required"],
        json!(["handle", "address"])
    );
    // An argument left out has no value at all, not a null one.
    for tool in TOOLS {
        let properties = input_schema(&tools, tool)["properties"].clone();
        for (name, property) in properties.as_object().into_iter().flatten() {
            assert!(
                property.get("default").is_none(),
                "{tool}.{name}: {property}"
            );
        }
    }
}

#[test]
fn what_until_and_gather_take_is_said_when_they_are_given_otherwise() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let refused = |session: &mut Session, condition: Value| {
        session.refusal(
            "run",
            json!({ "handle": handle, "frames": 1, "until": [{ "execute": 1 }, condition] }),
        )
    };
    let forms = "a condition is {\"execute\": address}, {\"read\": address} or {\"write\": \
                 address}, each with \"to\" and the last address if it is a range, or {\"in\": \
                 port, \"mask\": bits} or {\"out\": port, \"mask\": bits}, the mask being the bits \
                 of the port's address that its device decodes";
    assert_eq!(
        refused(&mut session, json!({ "read": 5, "out": 6 })),
        format!(
            "until[1]: {{\"read\":5,\"out\":6}} names read and out, and a condition names one; {forms}"
        )
    );
    assert_eq!(
        refused(&mut session, json!({ "to": 5 })),
        format!("until[1]: {{\"to\":5}} names no kind; {forms}")
    );
    assert_eq!(
        refused(&mut session, json!({ "out": "&FE", "mask": 70000 })),
        format!(
            "until[1]: mask 70000 is past &FFFF; an address is a number from 0 to 65535, or \
             hexadecimal from &0000 to &FFFF, written with &, #, $ or 0x before it; {forms}"
        )
    );
    assert_eq!(
        session.refusal(
            "run",
            json!({ "handle": handle, "frames": 1, "until": { "execute": "&0038" } })
        ),
        format!("until is a list of conditions, as [{{\"execute\": \"&0038\"}}]; {forms}")
    );
    assert_eq!(
        refused(&mut session, json!({ "in": "&FE" })),
        format!("until[1]: {{\"in\":\"&FE\"}} has no mask; {forms}")
    );
    assert_eq!(
        refused(&mut session, json!({ "write": 10, "to": 5 })),
        format!(
            "until[1]: {{\"write\":10,\"to\":5}} runs back from &000A to &0005, and a range runs \
             upward; {forms}"
        )
    );
    assert_eq!(
        refused(&mut session, json!({ "execute": 1, "mask": 2 })),
        format!(
            "until[1]: {{\"execute\":1,\"mask\":2}} has mask, which execute does not take; \
             {forms}"
        )
    );
    let many = vec![json!({ "execute": 1 }); 17];
    assert_eq!(
        session.refusal(
            "run",
            json!({ "handle": handle, "ticks": 10, "gather": many })
        ),
        "gather has 17 conditions, and takes at most 16"
    );
    assert_eq!(
        session.refusal(
            "call",
            json!({ "handle": handle, "address": 0, "count": [{ "write": 3, "to": 2 }] })
        ),
        format!(
            "count[0]: {{\"write\":3,\"to\":2}} runs back from &0003 to &0002, and a range runs \
             upward; {forms}"
        )
    );
    assert_eq!(session.inspect(&handle, "cpu.pc"), 0);
}

// What a machine was asked before machine_close is done, and answered.
#[test]
fn a_machine_put_away_answers_what_it_was_asked_first() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let ran = session.send_call("run", json!({ "handle": handle, "frames": 50 }));
    session.send_call("machine_close", json!({ "handle": handle }));
    let answers = [session.receive(), session.receive()];
    let run = answers
        .iter()
        .find(|answer| answer["id"] == ran)
        .expect("the run is answered");
    assert_eq!(run["result"]["structuredContent"]["frames"], 50, "{run}");
    assert!(session.handles().is_empty());
}

// A picture as a tool answers it, read back out of its PNG by the png crate:
// each pixel's index, and the palette the indices name.
struct Picture {
    width: usize,
    height: usize,
    indices: Vec<u8>,
    palette: Vec<u32>,
}

impl Picture {
    fn at(&self, x: usize, y: usize) -> u8 {
        self.indices[y * self.width + x]
    }
}

// "Unstructured content is returned in the content field of a result, and can
// contain multiple content items of different types", an image among them as
// "type": "image", "data": "base64-encoded-data" and its "mimeType"
// (2026-07-28, server/tools, Tool Result).
fn picture_of(result: &Value) -> Picture {
    let content = result["content"].as_array().expect("content");
    assert_eq!(content.len(), 2, "{result}");
    let image = &content[1];
    assert_eq!(
        (&image["type"], &image["mimeType"]),
        (&json!("image"), &json!("image/png")),
        "{image}"
    );
    let png = STANDARD
        .decode(image["data"].as_str().expect("data"))
        .expect("base64");
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .expect("a PNG");
    let info = reader.info();
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Indexed, png::BitDepth::Eight)
    );
    let palette = info
        .palette
        .as_ref()
        .expect("a palette")
        .chunks(3)
        .map(|rgb| u32::from_be_bytes([0, rgb[0], rgb[1], rgb[2]]))
        .collect();
    let (width, height) = (info.width as usize, info.height as usize);
    let mut indices = vec![0; reader.output_buffer_size().expect("a size")];
    reader.next_frame(&mut indices).expect("the pixels");
    assert_eq!(indices.len(), width * height);
    Picture {
        width,
        height,
        indices,
        palette,
    }
}

impl Session {
    // A picture's answer, its JSON told as text first, and the picture.
    fn pictured(&mut self, tool: &str, arguments: Value) -> (Value, Picture) {
        let result = self.call(tool, arguments);
        let answer = self.conforming(tool, &result);
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().expect("text"))
                .expect("JSON");
        assert_eq!(text, answer);
        let picture = picture_of(&result);
        assert_eq!(
            (picture.width, picture.height),
            if tool == "monitor" {
                (
                    answer["width"].as_u64().unwrap() as usize,
                    answer["height"].as_u64().unwrap() as usize,
                )
            } else {
                (
                    answer["picture_width"].as_u64().unwrap() as usize,
                    answer["picture_height"].as_u64().unwrap() as usize,
                )
            }
        );
        (answer, picture)
    }

    fn fill(&mut self, handle: &str, address: u16, bytes: &[u8]) {
        for (offset, chunk) in (0..).step_by(4096).zip(bytes.chunks(4096)) {
            let written: String = chunk.iter().map(|byte| format!("{byte:02X}")).collect();
            self.poke(handle, json!(address + offset), &written);
        }
    }
}

// The firmware's 50Hz screen, as its table at &05C5 in the 6128's ROM sets
// the 6845 ("The Amstrad CPC CRTC Compendium" v1.11, ch. 4.1): every register
// it sets to other than zero, save R14's &C0, which belongs to the cursor.
const STANDARD_SCREEN: [(u8, u8); 9] = [
    (0, 63),
    (1, 40),
    (2, 46),
    (3, 0x8E),
    (4, 38),
    (6, 25),
    (7, 30),
    (9, 7),
    (12, 0x30),
];

// Sixteen inks all told apart, and a border unlike any of them.
// Pen 0's is not 20, the black the Gate Array shows where it blanks.
const INKS: [u8; 16] = [4, 21, 28, 24, 29, 12, 5, 13, 22, 6, 23, 30, 0, 31, 14, 20];
const BORDER: u8 = 18;

// A CPC whose 6845 is set to the firmware's screen and whose inks and border
// are set, then halted for good: OUT (C),C to the 6845's select and data
// ports, &BCxx and &BDxx, and to the Gate Array's, &7Fxx, PENR choosing a pen
// and INKR, &40 and a colour code, setting it ("The Gate Array", Grim).
fn cpc_screen_program() -> Vec<u8> {
    let mut program = vec![DI];
    let mut out = |port: u8, value: u8| {
        program.extend([LD_BC, value, port, OUT_C_C[0], OUT_C_C[1]]);
    };
    for (register, value) in STANDARD_SCREEN {
        out(0xBC, register);
        out(0xBD, value);
    }
    for (pen, ink) in (0..).zip(INKS.iter().chain([&BORDER])) {
        out(0x7F, pen);
        out(0x7F, 0x40 | ink);
    }
    program.push(HALT);
    program
}

// Bytes that run through every value in each 256 of them, a step further on
// in each, so that no pen and no pixel goes unused.
fn varied(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| (index * 37 + index / 256) as u8)
        .collect()
}

#[test]
fn a_picture_comes_with_its_answer_told_as_text_first() {
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let ran = session.run_past_its_halt(&handle, 2);
    let (shown, _) = session.pictured("monitor", json!({ "handle": handle }));
    assert_eq!(
        (&shown["handle"], &shown["machine"], &shown["tick"]),
        (&json!(handle), &json!("spectrum48"), &ran["tick"])
    );
}

// Each INKR command and the colour a 40010 was measured giving it, as "The
// Gate Array" (Grim) tabulates them, INKR color-codes: the twenty-seven
// colours and the five that ghost them.
const GATE_ARRAY_COLOURS: [(u8, u32); 32] = [
    (0x54, 0x000201),
    (0x44, 0x00026B),
    (0x55, 0x0C02F4),
    (0x5C, 0x6C0201),
    (0x58, 0x690268),
    (0x5D, 0x6C02F2),
    (0x4C, 0xF30506),
    (0x45, 0xF00268),
    (0x4D, 0xF302F4),
    (0x56, 0x027801),
    (0x46, 0x007868),
    (0x57, 0x0C7BF4),
    (0x5E, 0x6E7B01),
    (0x40, 0x6E7D6B),
    (0x5F, 0x6E7BF6),
    (0x4E, 0xF37D0D),
    (0x47, 0xF37D6B),
    (0x4F, 0xFA80F9),
    (0x52, 0x02F001),
    (0x42, 0x00F36B),
    (0x53, 0x0FF3F2),
    (0x5A, 0x71F504),
    (0x59, 0x71F36B),
    (0x5B, 0x71F3F4),
    (0x4A, 0xF3F30D),
    (0x43, 0xF3F36D),
    (0x4B, 0xFFF3F9),
    (0x41, 0x6E7B6D),
    (0x48, 0xF30268),
    (0x49, 0xF3F36B),
    (0x50, 0x000268),
    (0x51, 0x02F36B),
];

// The window is the command line's, cut where its CPC_CROP_* and
// SPECTRUM_CROP_* say, and its picture is what the machine put on the cable:
// a CPC's border and its display, whose first byte is shown at sample 272 of
// line 70 (the comment over those constants in the emulator's cli/main.c), and
// a Spectrum's border, set by OUT (&FE) ("16K/48K reference", comp.sys.
// sinclair FAQ, https://worldofspectrum.org/faq/reference/48kreference.htm),
// and its picture, at sample 144 of line 56, coloured by its attributes,
// which follow the display file's 6144 bytes line by line from &5800, each in
// ATTR's form: 64 if bright, 8 times the paper's colour, and the ink's (ZX
// Spectrum BASIC programming, ch. 16 and 24,
// https://worldofspectrum.org/ZXBasicManual/zxmanchap16.html). A run by
// frames leaves the beam at the top of the frame.
#[test]
fn the_monitor_shows_what_the_machine_put_on_the_cable() {
    let roms = Roms::programmed("cpc464.rom", &cpc_screen_program());
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc464");
    session.run_past_its_halt(&cpc, 3);
    let (shown, monitor) = session.pictured("monitor", json!({ "handle": cpc }));
    assert_eq!(
        (
            &shown["width"],
            &shown["height"],
            &shown["beam_y"],
            &shown["whole"]
        ),
        (&json!(768), &json!(544), &json!(0), &json!(true))
    );
    for (inkr, colour) in GATE_ARRAY_COLOURS {
        assert_eq!(
            monitor.palette[usize::from(inkr & 0x1F)],
            colour,
            "&{inkr:02X}"
        );
    }
    let display = (272 - 208, (70 - 34) * 2);
    assert_eq!(monitor.at(0, 0), BORDER);
    assert_eq!(monitor.at(display.0 - 1, display.1), BORDER);
    assert_eq!(monitor.at(display.0, display.1 - 1), BORDER);
    assert_eq!(monitor.at(display.0, display.1), INKS[0]);
    let program = [DI, LD_A, 0x05, OUT_N_A, 0xFE, HALT];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let spectrum = session.open("spectrum48");
    // The first character's attribute: bright, white paper, black ink.
    session.poke(&spectrum, json!("&5800"), "78");
    session.run_past_its_halt(&spectrum, 2);
    let (shown, monitor) = session.pictured("monitor", json!({ "handle": spectrum }));
    assert_eq!(
        (
            &shown["width"],
            &shown["height"],
            &shown["beam_y"],
            &shown["whole"]
        ),
        (&json!(352), &json!(264), &json!(0), &json!(true))
    );
    let picture = (144 - 96, 56 - 20);
    assert_eq!(monitor.at(0, 0), 5);
    assert_eq!(monitor.at(picture.0 - 1, picture.1), 5);
    assert_eq!(monitor.at(picture.0, picture.1 - 1), 5);
    assert_eq!(monitor.at(picture.0, picture.1), 15);
    assert_eq!(monitor.at(picture.0 + 8, picture.1), 0);
    // Brightness lifts every gun of white, and the guns stay level.
    let [_, red, green, blue] = monitor.palette[15].to_be_bytes();
    let [_, dim_red, dim_green, dim_blue] = monitor.palette[7].to_be_bytes();
    assert!(red == green && green == blue && dim_red == dim_green && dim_green == dim_blue);
    assert!(red > dim_red, "{red} over {dim_red}");
    // Where the beam stands, as the machine counts it, once it has left the
    // corner a retrace sends it to.
    session.step(&spectrum, 1000);
    let (shown, _) = session.pictured("monitor", json!({ "handle": spectrum }));
    assert_eq!(
        (&shown["beam_x"], &shown["beam_y"]),
        (
            &session.inspect(&spectrum, "monitor.beam_x"),
            &session.inspect(&spectrum, "monitor.beam_y")
        )
    );
    assert!(shown["beam_y"].as_u64() > Some(0), "{shown}");
}

// The monitor paints the sample its beam stands on and then steps the beam
// (the emulator's src/monitor.c, monitor_receive), so no pass is partway
// through the picture while the beam stands at or before its first sample,
// or past its last: on a CPC sample 208 of line 34 and sample 975 of line
// 305, on a Spectrum sample 96 of line 20 and sample 447 of line 283, the
// window the emulator's command line crops to (CPC_CROP_* and SPECTRUM_CROP_*
// in its cli/main.c).
#[test]
fn the_monitor_says_whether_its_beam_is_partway_through_its_picture() {
    let roms = Roms::programmed("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let whole = |session: &mut Session, handle: &str| {
        session.pictured("monitor", json!({ "handle": handle })).0["whole"].clone()
    };
    session.run_past_its_halt(&handle, 2);
    assert_eq!(whole(&mut session, &handle), true);
    let partway = session.run(&handle, json!({ "ticks": 20000 }));
    assert_eq!(whole(&mut session, &handle), false, "{partway}");
    let cpc = session.open("cpc464");
    for (handle, first, last) in [
        (&handle, (20, 96), (283, 447)),
        (&cpc, (34, 208), (305, 975)),
    ] {
        for ((line, sample), one_frame) in [
            ((first.0 - 1, last.1), true),
            (first, true),
            ((first.0, first.1 + 1), false),
            (last, false),
            ((last.0, last.1 + 1), true),
            ((last.0 + 1, 0), true),
        ] {
            session.set(
                handle,
                json!({ "monitor.beam_y": line, "monitor.beam_x": sample }),
            );
            assert_eq!(
                whole(&mut session, handle),
                one_frame,
                "{handle}: sample {sample} of line {line}"
            );
        }
    }
}

// The monitor's picture where the display falls, held to the memory the 6845
// is displaying, as screen reads it with nothing asked: pen for colour, every
// pixel of every line. With the firmware's horizontal timing and its frame
// sync at line 240, the display's first byte is shown at sample 272 of line
// 70 (the comment over CPC_CROP_* in the emulator's cli/main.c).
fn the_screen_is_the_monitors_picture(session: &mut Session, handle: &str) -> Value {
    let display = (272 - 208, (70 - 34) * 2);
    let (_, monitor) = session.pictured("monitor", json!({ "handle": handle }));
    let (drawn, screen) = session.pictured("screen", json!({ "handle": handle }));
    let inks: Vec<u8> = drawn["palette"]
        .as_array()
        .expect("a palette")
        .iter()
        .map(|code| code.as_u64().unwrap() as u8)
        .collect();
    let colours: Vec<u32> = inks
        .iter()
        .map(|code| {
            let (_, colour) = GATE_ARRAY_COLOURS
                .iter()
                .find(|(inkr, _)| inkr & 0x1F == *code)
                .expect("a colour code");
            *colour
        })
        .collect();
    assert_eq!(screen.palette, colours);
    for y in 0..screen.height {
        let shown = &monitor.indices[(display.1 + y) * monitor.width + display.0..][..screen.width];
        let drawn: Vec<u8> = screen.indices[y * screen.width..][..screen.width]
            .iter()
            .map(|pen| inks[usize::from(*pen)])
            .collect();
        assert_eq!(shown, drawn.as_slice(), "line {y}");
    }
    drawn
}

// The 6845 programmed otherwise, each time keeping 312 lines to the frame and
// its sync at line 240: rows of five lines, R5's two lines more, and a start
// partway into its 2K whose rows wrap inside it; rows of sixteen lines, which
// the board's three raster lines cannot tell apart from rows of eight, in
// the 16K at &8000, where a fourth would reach another; and a
// screen of 27 rows that starts where R12's bits 3 and 2, MA11 and MA10,
// carry its rows from the 16K at &8000 on into the one at &C000, as a screen
// of 32K does.
#[test]
fn the_screen_follows_the_6845_wherever_it_starts_and_however_it_counts() {
    let roms = Roms::programmed("cpc464.rom", &cpc_screen_program());
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    session.run_past_its_halt(&handle, 3);
    session.fill(&handle, 0x8000, &varied(0x8000));
    for (registers, expected) in [
        (
            json!({
                "crtc.registers[4]": 61, "crtc.registers[5]": 2, "crtc.registers[6]": 40,
                "crtc.registers[7]": 48, "crtc.registers[9]": 4, "crtc.registers[12]": 0x33,
                "crtc.registers[13]": 0xD1,
            }),
            (0xC7A2, 200, 5),
        ),
        (
            json!({
                "crtc.registers[4]": 18, "crtc.registers[5]": 8, "crtc.registers[6]": 12,
                "crtc.registers[7]": 15, "crtc.registers[9]": 15, "crtc.registers[12]": 0x21,
                "crtc.registers[13]": 0x28,
            }),
            (0x8250, 192, 16),
        ),
        (
            json!({
                "crtc.registers[4]": 38, "crtc.registers[5]": 0, "crtc.registers[6]": 27,
                "crtc.registers[7]": 30, "crtc.registers[9]": 7, "crtc.registers[12]": 0x2C,
                "crtc.registers[13]": 0xF0,
            }),
            (0x81E0, 216, 8),
        ),
    ] {
        session.set(&handle, registers);
        session.run(&handle, json!({ "frames": 3 }));
        let drawn = the_screen_is_the_monitors_picture(&mut session, &handle);
        assert_eq!(
            (&drawn["base"], &drawn["height"], &drawn["rasters"]),
            (
                &ampersand(expected.0),
                &json!(expected.1),
                &json!(expected.2)
            )
        );
    }
}

// The memory the 6845 is displaying, read by screen with nothing asked, is
// the monitor's picture where the display falls, pen for colour, in each of
// the Gate Array's modes: what the emulator's own video put on the cable is
// the measure. The mode is set where the machine stands at a retrace, so the
// whole frame after it is drawn in it.
#[test]
fn the_screen_the_6845_displays_is_the_picture_on_the_monitor_in_every_mode() {
    let roms = Roms::programmed("cpc464.rom", &cpc_screen_program());
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    session.run_past_its_halt(&handle, 3);
    session.fill(&handle, 0xC000, &varied(0x4000));
    for mode in 0..4 {
        // The mode in force is read, and not one written and still to come.
        session.set(
            &handle,
            json!({ "gate_array.mode": mode, "gate_array.mode_pending": (mode + 1) % 4 }),
        );
        let (drawn, _) = session.pictured("screen", json!({ "handle": handle }));
        assert_eq!(drawn["mode"], mode);
        session.set(&handle, json!({ "gate_array.mode_pending": mode }));
        session.run(&handle, json!({ "frames": 1 }));
        let drawn = the_screen_is_the_monitors_picture(&mut session, &handle);
        assert_eq!(
            drawn,
            json!({
                "handle": handle,
                "machine": "cpc464",
                "tick": drawn["tick"],
                "reading": "video",
                "base": "&C000",
                "width": 80,
                "height": 200,
                "rasters": 8,
                "mode": mode,
                "palette": INKS,
                "picture_width": 640,
                "picture_height": 400,
            })
        );
    }
    // A mode past 3 and an ink past five bits, which only set can write, are
    // drawn by the Gate Array as mode 3 and as the ink's five bits, and read
    // so; a height not given is R6's rows of the rasters given.
    session.set(
        &handle,
        json!({
            "gate_array.mode": 7, "gate_array.mode_pending": 7,
            "gate_array.inks[0]": 0x40 | INKS[0],
        }),
    );
    session.run(&handle, json!({ "frames": 1 }));
    let drawn = the_screen_is_the_monitors_picture(&mut session, &handle);
    assert_eq!(
        (&drawn["mode"], &drawn["palette"]),
        (&json!(3), &json!(INKS))
    );
    let (drawn, _) = session.pictured("screen", json!({ "handle": handle, "rasters": 4 }));
    assert_eq!(drawn["height"], 25 * 4);
}

// "The Gate Array" (Grim), Byte/Pixel structure: each mode's byte from bit 7
// to bit 0, each bit named by the pixel it belongs to, A the leftmost, and by
// the bit of that pixel's pen it is; mode 2's bits are each a whole pen, and
// mode 3 reads none of the bits written x.
const BYTE_PIXEL_STRUCTURE: [&str; 4] = [
    "A0 B0 A2 B2 A1 B1 A3 B3",
    "A0 B0 C0 D0 A1 B1 C1 D1",
    "A B C D E F G H",
    "A0 B0 x x A1 B1 x x",
];

#[test]
fn a_byte_becomes_pixels_as_the_gate_array_reads_it() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    let pens: Vec<u8> = (0..16).collect();
    for (mode, structure) in BYTE_PIXEL_STRUCTURE.iter().enumerate() {
        let bits: Vec<&str> = structure.split(' ').collect();
        let pixels = bits
            .iter()
            .filter(|bit| **bit != "x")
            .map(|bit| bit.as_bytes()[0])
            .collect::<BTreeSet<u8>>()
            .len();
        for (place, bit) in (0..8).rev().zip(&bits) {
            session.poke(&handle, json!("&4000"), &format!("{:02X}", 1u8 << place));
            let (_, picture) = session.pictured(
                "screen",
                json!({
                    "handle": handle, "reading": "linear", "base": "&4000", "width": 1,
                    "height": 1, "mode": mode, "palette": pens,
                }),
            );
            let mut expected = vec![0; 8];
            if *bit != "x" {
                let pixel = usize::from(bit.as_bytes()[0] - b'A');
                let pen = bit[1..].parse::<u32>().map_or(1, |digit| 1 << digit);
                let wide = 8 / pixels;
                expected[pixel * wide..][..wide].fill(pen);
            }
            let expected = [expected.clone(), expected].concat();
            assert_eq!(picture.indices, expected, "mode {mode}, bit {place}");
        }
    }
}

// Mode 2 lights a pixel for each bit set, so a byte with one bit set shows
// where screen placed it, each a bit further right than the last. The bytes
// from &0000 lie under the lower ROM, which the processor reads instead, and
// which a screen does not.
#[test]
fn linear_and_columns_lay_out_the_bytes_and_wrap_at_the_end_of_the_ram() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    session.poke(&handle, json!("&FFFD"), "804020");
    session.poke(&handle, json!(0), "100804");
    assert_eq!(session.peek(&handle, json!(0), 3), "000000");
    let lit = |session: &mut Session, reading: &str| {
        let (_, picture) = session.pictured(
            "screen",
            json!({
                "handle": handle, "reading": reading, "base": "&FFFD", "width": 3,
                "height": 2, "mode": 2,
            }),
        );
        (0..picture.height)
            .step_by(2)
            .map(|y| {
                (0..picture.width)
                    .filter(|x| picture.at(*x, y) == 1)
                    .collect()
            })
            .collect::<Vec<Vec<usize>>>()
    };
    assert_eq!(
        lit(&mut session, "linear"),
        [vec![0, 9, 18], vec![3, 12, 21]]
    );
    assert_eq!(
        lit(&mut session, "columns"),
        [vec![0, 10, 20], vec![1, 11, 21]]
    );
}

// A row read as the video hardware reads it wraps inside its 2K, "rather
// than running on into the next one", and its next line lies 2K on
// (colophon-player's docs/debugger/screen.en.md, "The readings"); an odd
// base begins at the second byte of its character. Mode 2 lights a pixel for
// each bit set.
#[test]
fn a_video_row_wraps_inside_its_2k_and_may_begin_at_an_odd_byte() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    session.poke(&handle, json!("&C7FE"), "8040");
    session.poke(&handle, json!("&C000"), "2010");
    session.poke(&handle, json!("&CFFE"), "0804");
    session.poke(&handle, json!("&C800"), "0201");
    let lit = |session: &mut Session, base: &str, width: u32, height: u32| {
        let (_, picture) = session.pictured(
            "screen",
            json!({
                "handle": handle, "reading": "video", "base": base, "width": width,
                "height": height, "rasters": 8, "mode": 2,
            }),
        );
        (0..picture.height)
            .step_by(2)
            .map(|y| {
                (0..picture.width)
                    .filter(|x| picture.at(*x, y) == 1)
                    .collect()
            })
            .collect::<Vec<Vec<usize>>>()
    };
    assert_eq!(
        lit(&mut session, "&C7FE", 4, 2),
        [vec![0, 9, 18, 27], vec![4, 13, 22, 31]]
    );
    assert_eq!(lit(&mut session, "&C7FF", 2, 1), [vec![1, 10]]);
}

// RAM configuration 4 pages the second 64K's first bank in at &4000 ("Ram
// management", cpctech, https://cpctech.cpcwiki.de/docs/rampage.html), and a
// screen reads it at &10000, past the processor's reach.
#[test]
fn a_screen_reads_a_6128s_second_64k_at_its_own_addresses() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    session.set(&handle, json!({ "mmr": 0xC4 }));
    session.poke(&handle, json!("&4000"), "80");
    session.set(&handle, json!({ "mmr": 0xC0 }));
    assert_eq!(session.peek(&handle, json!("&4000"), 1), "00");
    for base in [json!("&10000"), json!(65536)] {
        let (drawn, picture) = session.pictured(
            "screen",
            json!({
                "handle": handle, "reading": "linear", "base": base, "width": 1,
                "height": 1, "mode": 2,
            }),
        );
        assert_eq!(drawn["base"], "&10000");
        assert_eq!(picture.indices[..8], [1, 0, 0, 0, 0, 0, 0, 0]);
    }
}

#[test]
fn screen_says_what_would_have_worked() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc464");
    let spectrum = session.open("spectrum48");
    let refused = |session: &mut Session, arguments: Value| {
        let mut arguments = arguments;
        if arguments.get("handle").is_none() {
            arguments["handle"] = json!(cpc);
        }
        session.refusal("screen", arguments)
    };
    assert_eq!(
        refused(&mut session, json!({ "handle": spectrum })),
        "screen reads a CPC's memory as its video hardware does, and a Spectrum's picture is read \
         by monitor"
    );
    assert_eq!(
        refused(&mut session, json!({})),
        "R1 is 0, which gives a width of 0, and a screen reads from 1 to 128; give width"
    );
    assert_eq!(
        refused(&mut session, json!({ "width": 80 })),
        "R6 is 0, which with rasters of 1 gives a height of 0, and a screen reads from 1 to 512; \
         give height"
    );
    assert_eq!(
        refused(&mut session, json!({ "reading": "linear", "rasters": 8 })),
        "rasters is read by the video reading alone: a flat buffer or one in columns has rows of \
         one line; give it with the video reading, or leave it out"
    );
    assert_eq!(
        refused(
            &mut session,
            json!({ "base": "&C800", "width": 2, "height": 2 })
        ),
        "&C800 is no display start: the video reading reads the base 64K, and there A13 to A11 \
         carry the raster line, so a base is &0000 to &07FF, &4000 to &47FF, &8000 to &87FF or \
         &C000 to &C7FF; linear and columns read the rest"
    );
    assert_eq!(
        refused(
            &mut session,
            json!({ "reading": "columns", "base": "&10000", "width": 2, "height": 2 })
        ),
        "&10000 is past the RAM, which ends at &FFFF"
    );
    assert_eq!(
        refused(&mut session, json!({ "base": "&200000" })),
        "\"&200000\" is no address in the RAM; an address in the RAM is a number from 0 to \
         131071, or hexadecimal from &0000 to &1FFFF, written with &, #, $ or 0x before it"
    );
    assert_eq!(
        refused(&mut session, json!({ "width": 129 })),
        "width is 129, and a screen reads from 1 to 128"
    );
    assert_eq!(
        refused(&mut session, json!({ "height": 0 })),
        "height is 0, and a screen reads from 1 to 512"
    );
    assert_eq!(
        refused(&mut session, json!({ "mode": 4 })),
        "mode is 4, and the Gate Array's modes are 0 to 3"
    );
    assert_eq!(
        refused(&mut session, json!({ "palette": [1, 2, 32] })),
        "palette is [1, 2, 32], and a palette is sixteen hardware colour codes, one for each pen, \
         each from 0 to 31"
    );
}

// Zilog's mnemonics for these bytes (Z80 CPU User Manual, UM0080), read
// through the lower ROM the processor sees at &0000 rather than the RAM
// beneath it, a relative jump's target worked out from the end of the jump,
// and ED FF named as WinAPE names it ("Using the Debugger",
// http://www.winape.net/help/debug.html).
#[test]
fn a_listing_reads_memory_as_the_processor_would() {
    let program = [
        DI, LD_HL, 0x34, 0x12, PREFIX_DD, LD_HL, 0x78, 0x56, JR, 0xFE, PREFIX_DD, LD_A_AT_HL, 0xFB,
        0xED, 0xFF,
    ];
    let roms = Roms::programmed("cpc464.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    session.poke(&handle, json!(0), "C9C9C9C9");
    let listed = session.answer(
        "disassemble",
        json!({ "handle": handle, "address": 0, "instructions": 6 }),
    );
    assert_eq!(
        listed,
        json!({ "instructions": [
            { "address": "&0000", "bytes": "F3", "text": "DI" },
            { "address": "&0001", "bytes": "213412", "text": "LD HL,&1234" },
            { "address": "&0004", "bytes": "DD217856", "text": "LD IX,&5678" },
            { "address": "&0008", "bytes": "18FE", "text": "JR &0008" },
            { "address": "&000A", "bytes": "DD7EFB", "text": "LD A,(IX-&05)" },
            { "address": "&000D", "bytes": "EDFF", "text": "BRK" },
        ]})
    );
    for (asked, given) in [(None, 16), (Some(256), 256)] {
        let mut arguments = json!({ "handle": handle, "address": 15 });
        if let Some(asked) = asked {
            arguments["instructions"] = json!(asked);
        }
        let listed = session.answer("disassemble", arguments);
        assert_eq!(listed["instructions"].as_array().unwrap().len(), given);
    }
}

// An instruction at &FFFF reads on from &0000, as the program counter goes.
#[test]
fn a_listing_runs_past_ffff_as_the_program_counter_does() {
    let roms = Roms::programmed("spectrum48.rom", &[0x34, 0x12]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.poke(&handle, json!("&FFFF"), "21");
    let listed = session.answer(
        "disassemble",
        json!({ "handle": handle, "address": "&FFFF", "instructions": 2 }),
    );
    assert_eq!(
        listed,
        json!({ "instructions": [
            { "address": "&FFFF", "bytes": "213412", "text": "LD HL,&1234" },
            { "address": "&0002", "bytes": "00", "text": "NOP" },
        ]})
    );
}

// What a step runs is one instruction listed, a chain of prefixes and all.
#[test]
fn a_chain_of_prefixes_is_listed_as_the_one_instruction_a_step_runs() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.poke(&handle, json!("&8000"), "FDDDFD213412");
    session.set(&handle, json!({ "cpu.pc": 0x8000 }));
    let listed = session.answer(
        "disassemble",
        json!({ "handle": handle, "address": "&8000", "instructions": 1 }),
    );
    assert_eq!(
        listed["instructions"],
        json!([{ "address": "&8000", "bytes": "FDDDFD213412", "text": "LD IY,&1234" }])
    );
    let stepped = session.step(&handle, 1);
    assert_eq!(stepped["pc"], "&8006", "{stepped}");
    assert_eq!(
        (
            session.inspect(&handle, "cpu.iyh"),
            session.inspect(&handle, "cpu.ixh")
        ),
        (json!(0x12), json!(0))
    );
}

#[test]
fn disassemble_says_what_would_have_worked() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    for instructions in [0, 257] {
        assert_eq!(
            session.refusal(
                "disassemble",
                json!({ "handle": handle, "address": 0, "instructions": instructions })
            ),
            format!(
                "instructions is {instructions}, and a listing takes from 1 to 256; disassemble \
                 again from where it ends to go further"
            )
        );
    }
    session.fill(&handle, 0x8000, &[PREFIX_DD; 4096]);
    assert_eq!(
        session.refusal(
            "disassemble",
            json!({ "handle": handle, "address": "&8000" })
        ),
        "&8000 begins a chain of DD and FD prefixes that runs on to &8FFF, longer than the 4096 \
         bytes a listing reads; the last of them rules the instruction they prefix, so \
         disassemble from &8FFF"
    );
    // With its last three bytes an instruction, the chain and the
    // instruction it prefixes come to the 4096 bytes a listing reads.
    session.poke(&handle, json!("&8FFD"), "213412");
    let listed = session.answer(
        "disassemble",
        json!({ "handle": handle, "address": "&8000", "instructions": 2 }),
    );
    assert_eq!(
        listed["instructions"].as_array().unwrap().len(),
        1,
        "{listed:.200}"
    );
    assert_eq!(listed["instructions"][0]["text"], "LD IX,&1234");
}

const KINDS: [&str; 5] = ["execute", "read", "write", "in", "out"];

// What a run or a call counted, in the order it answered: each place as its
// kind, by its place among KINDS, and its address or port, and how many
// times it matched.
fn places(list: &Value) -> Vec<((usize, u64), u64)> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|counted| {
            let kind = KINDS
                .iter()
                .position(|kind| counted.get(*kind).is_some())
                .expect("a kind");
            (
                (kind, after_ampersand(&counted[KINDS[kind]], 4)),
                counted["count"].as_u64().unwrap(),
            )
        })
        .collect()
}

// A machine gathering over a stretch, and its twin counting over the same
// ticks, run again wherever either fills, see the same matches: the
// gathering, tallied by place, is the count. Each pass of the program walks
// 256 bytes, so both fill on the way.
#[test]
fn counting_a_stretch_is_tallying_a_gathering_of_it() {
    for (file, machine, port, port_address) in [
        (
            "cpc6128.rom",
            "cpc6128",
            json!({ "out": "&7F00", "mask": "&C000" }),
            0x7F00,
        ),
        (
            "spectrum48.rom",
            "spectrum48",
            json!({ "out": "&FE", "mask": "&01" }),
            0x00FE,
        ),
    ] {
        let roms = Roms::programmed(file, &passing(port_address));
        let mut session = Session::start(&roms);
        let conditions = json!([
            { "execute": 0, "to": "&3FFF" },
            { "read": "&9000", "to": "&90FF" },
            { "write": "&7F00", "to": "&90FF" },
            port,
        ]);
        let stretch = 3 * SPECTRUM_FRAME_TICKS;
        let mut over = |list: &str| {
            let handle = session.open(machine);
            let (mut tick, mut runs) = (0, 0);
            let mut tally: BTreeMap<(String, u64), u64> = BTreeMap::new();
            while tick < stretch {
                let mut arguments = json!({ "ticks": stretch - tick });
                arguments[list] = conditions.clone();
                let ran = session.run(&handle, arguments);
                tick = ran["tick"].as_u64().unwrap();
                runs += 1;
                let seen: Vec<((String, u64), u64)> = if list == "gather" {
                    seen(&ran["gathered"])
                        .into_iter()
                        .map(|(kind, address, _, _)| ((kind.to_owned(), address), 1))
                        .collect()
                } else {
                    let counted = places(&ran["counted"]);
                    assert!(ran.get("gathered").is_none(), "{ran}");
                    // Executes first, then reads, writes and ports, each
                    // kind by address, and a full count holds 512 places
                    // and the rest of the instruction that filled it.
                    assert!(
                        counted.windows(2).all(|pair| pair[0].0 < pair[1].0),
                        "{ran}"
                    );
                    if ran["stopped"] == "full" {
                        assert!(counted.len() >= 512, "{ran}");
                    }
                    counted
                        .into_iter()
                        .map(|((kind, address), count)| ((KINDS[kind].to_owned(), address), count))
                        .collect()
                };
                for (place, count) in seen {
                    *tally.entry(place).or_insert(0) += count;
                }
            }
            (tick, runs, tally)
        };
        let (gathered_to, gatherings, gathered) = over("gather");
        let (counted_to, countings, counted) = over("count");
        assert_eq!(gathered_to, counted_to, "{machine}");
        assert_eq!(gathered, counted, "{machine}");
        assert!(
            gatherings > 10 && countings > 1,
            "{machine}: {gatherings} gatherings, {countings} countings"
        );
    }
}

// Each place is counted by its whole address, so two values sent to the
// Spectrum's ULA with OUT (&FE),A, whose high byte is A, are two places of one
// condition; the instruction a run stops before is not yet counted; and a JR
// to itself, 12 T-states each time from the ROM, where nothing holds the
// processor (UM0080), is counted 1000 times in 12000 ticks, and gathered and
// counted alike where one run does both.
#[test]
fn a_count_counts_each_place_by_its_whole_address_however_often() {
    let program = [
        DI, LD_A, 0x05, OUT_N_A, 0xFE, LD_A, 0x06, OUT_N_A, 0xFE, JR, 0xFE,
    ];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let ran = session.run(
        &handle,
        json!({
            "ticks": 1000,
            "until": [{ "execute": 9 }],
            "count": [{ "execute": 0, "to": 9 }, { "out": "&FE", "mask": "&01" }],
        }),
    );
    assert_eq!(ran["stopped"], "matched", "{ran}");
    assert_eq!(
        ran["counted"],
        json!([
            { "execute": "&0000", "count": 1 },
            { "execute": "&0001", "count": 1 },
            { "execute": "&0003", "count": 1 },
            { "execute": "&0005", "count": 1 },
            { "execute": "&0007", "count": 1 },
            { "out": "&05FE", "count": 1 },
            { "out": "&06FE", "count": 1 },
        ])
    );
    let jump = json!([{ "execute": 9 }]);
    let ran = session.run(&handle, json!({ "ticks": 12000, "count": jump }));
    assert_eq!(
        (&ran["ticks"], &ran["counted"]),
        (
            &json!(12000),
            &json!([{ "execute": "&0009", "count": 1000 }])
        ),
        "{ran}"
    );
    let ran = session.run(
        &handle,
        json!({ "ticks": 120, "count": jump, "gather": jump }),
    );
    assert_eq!(seen(&ran["gathered"]).len(), 10, "{ran}");
    assert_eq!(ran["counted"], json!([{ "execute": "&0009", "count": 10 }]));
}

// The stand-in firmware is zeros, NOPs from &0000 on, each beginning at an
// address of its own; interrupts are disabled at power-on.
#[test]
fn a_count_that_fills_stops_and_the_next_run_counts_on() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("cpc464");
    let counting = json!([{ "execute": 0, "to": "&3FFF" }]);
    for first in [0, 512] {
        let ran = session.run(&handle, json!({ "frames": 1, "count": counting }));
        assert_eq!(
            (&ran["stopped"], &ran["pc"]),
            (&json!("full"), &ampersand(first + 512)),
            "{ran}"
        );
        let expected: Vec<Value> = (first..first + 512)
            .map(|address| json!({ "execute": ampersand(address), "count": 1 }))
            .collect();
        assert_eq!(ran["counted"], json!(expected));
    }
    let ran = session.run(&handle, json!({ "ticks": 4 }));
    assert!(ran.get("counted").is_none(), "{ran}");
}

fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// A Spectrum's keyboard, read by a program the test writes: port &7FFE's
// half-row, then &FDFE's, each into a byte of its own, again and again. "A
// zero on one of these lines selects a particular half-row of five keys",
// 0x7ffe's "SPACE, SYM SHFT, M, N, B" and 0xfdfe's "A, S, D, F, G", and "a
// zero in one of the five lowest bits means that the corresponding key is
// pressed" (comp.sys.sinclair FAQ, 16K/48K reference,
// https://worldofspectrum.org/faq/reference/48kreference.htm).
const READING_TWO_HALF_ROWS: [u8; 17] = [
    DI, LD_A, 0x7F, IN_A_N, 0xFE, LD_TO_A, 0x00, 0x80, LD_A, 0xFD, IN_A_N, 0xFE, LD_TO_A, 0x01,
    0x80, JR, 0xF0,
];

fn half_rows(session: &mut Session, handle: &str, at: u16) -> (u8, u8) {
    let bytes = bytes_of(&session.peek(handle, json!(at), 2));
    (bytes[0] & 0x1F, bytes[1] & 0x1F)
}

#[test]
fn keys_are_held_down_through_a_run_and_let_go_at_its_end() {
    let roms = Roms::programmed("spectrum48.rom", &READING_TWO_HALF_ROWS);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 1000, "keys": ["SPACE", "A"] }));
    assert_eq!(half_rows(&mut session, &handle, 0x8000), (0x1E, 0x1E));
    session.run(&handle, json!({ "ticks": 1000 }));
    assert_eq!(half_rows(&mut session, &handle, 0x8000), (0x1F, 0x1F));
    session.run(
        &handle,
        json!({ "ticks": 1000, "keys": ["SPACE", "SYM", "M", "N", "B"] }),
    );
    assert_eq!(half_rows(&mut session, &handle, 0x8000), (0x00, 0x1F));
    // A key the keyboard holds already, as set wrote it, is held on top of
    // and held still once the run is done.
    session.set(&handle, json!({ "keyboard.lines[7]": 0xFD }));
    session.run(&handle, json!({ "ticks": 1000, "keys": ["SPACE"] }));
    assert_eq!(half_rows(&mut session, &handle, 0x8000), (0x1C, 0x1F));
    session.run(&handle, json!({ "ticks": 1000 }));
    assert_eq!(half_rows(&mut session, &handle, 0x8000), (0x1D, 0x1F));
    assert_eq!(session.inspect(&handle, "keyboard.lines[7]"), 0xFD);
}

#[test]
fn a_key_neither_named_nor_numbered_is_refused_with_the_keys_there_are() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    let refused = session.refusal(
        "run",
        json!({ "handle": cpc, "ticks": 100, "keys": ["SPACE", "FIRE"] }),
    );
    assert!(
        refused.starts_with(
            "no key of a cpc6128 is \"FIRE\"; a key is given by its name, or by its number from \
             0 to 79 as the firmware numbers it"
        ),
        "{refused}"
    );
    assert!(
        refused.contains("47 SPACE") && refused.contains("76 (FIRE 2)"),
        "{refused}"
    );
    let refused = session.refusal("run", json!({ "handle": cpc, "ticks": 100, "keys": [80] }));
    assert!(
        refused.starts_with("no key of a cpc6128 is 80;"),
        "{refused}"
    );
    let spectrum = session.open("spectrum48");
    let refused = session.refusal(
        "run",
        json!({ "handle": spectrum, "ticks": 100, "keys": [56] }),
    );
    assert!(
        refused.starts_with(
            "no key of a spectrum48 is 56; a key is given by its name: CAPS, Z, X, C, V, A"
        ),
        "{refused}"
    );
    for handle in [cpc, spectrum] {
        let ran = session.run(&handle, json!({ "ticks": 1 }));
        assert_eq!(ran["tick"], ran["ticks"], "{handle} ran before");
    }
}

// Three half-rows logged once a frame, a byte each: 0xfefe's "SHIFT, Z, X,
// C, V", 0xdffe's "P, O, I, U, Y" and 0x7ffe's (16K/48K reference). The ULA's
// interrupt, in mode 1, wakes the program from HALT at the start of each
// frame, and the restart at &0038, which the test writes, enables
// interrupts again and returns.
const LOGGING_THREE_HALF_ROWS: [u8; 31] = [
    DI, LD_SP, 0x00, 0xFF, IM_1[0], IM_1[1], LD_HL, 0x00, 0x80, EI, HALT, LD_A, 0xFE, IN_A_N, 0xFE,
    LD_AT_HL_A, INC_HL, LD_A, 0xDF, IN_A_N, 0xFE, LD_AT_HL_A, INC_HL, LD_A, 0x7F, IN_A_N, 0xFE,
    LD_AT_HL_A, INC_HL, JR, 0xEB,
];

// A double quote is P with SYM SHFT, for the ROM's "(e) Symbol code. Letter
// keys and symbol shift." gives P, its sixteenth letter, &22, and a capital
// P is P with CAPS SHIFT, for "(a) The main key table - L mode and CAPS
// SHIFT." holds the letters as capitals (The Complete Spectrum ROM
// Disassembly, https://skoolkid.github.io/rom/asm/0205.html). The command
// line holds a key for 3 frames and lets it go for 6
// (keys_are_held_and_let_go_as_the_command_line_types holds the server to
// its source). M, held by set beforehand, stays held throughout.
#[test]
fn a_typed_key_is_held_and_let_go_for_the_frames_the_command_line_takes() {
    let roms = Roms::programmed("spectrum48.rom", &LOGGING_THREE_HALF_ROWS);
    roms.write_at("spectrum48.rom", 0x38, &[EI, RET]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "frames": 1 }));
    let cpu = session.inspect(&handle, "cpu");
    let from =
        u16::try_from(cpu["h"].as_u64().unwrap() * 256 + cpu["l"].as_u64().unwrap()).unwrap();
    session.set(&handle, json!({ "keyboard.lines[7]": 0xFB }));
    let typed = session.answer("type", json!({ "handle": handle, "text": "\"P" }));
    assert_eq!(typed["stopped"], "typed", "{typed}");
    let logged: Vec<Vec<u8>> = bytes_of(&session.peek(&handle, json!(from), 3 * 19))
        .chunks(3)
        .map(|frame| frame.iter().map(|half_row| half_row & 0x1F).collect())
        .collect();
    let frames = |count: usize, half_rows: [u8; 3]| vec![half_rows.to_vec(); count];
    assert_eq!(
        logged,
        [
            frames(3, [0x1F, 0x1E, 0x19]),
            frames(6, [0x1F, 0x1F, 0x1B]),
            frames(3, [0x1E, 0x1E, 0x1B]),
            frames(6, [0x1F, 0x1F, 0x1B]),
            frames(1, [0, 0, 0]),
        ]
        .concat()
    );
    assert_eq!(session.inspect(&handle, "keyboard.lines[7]"), 0xFB);
    // Each stretch is measured from where the typing began, so the typing
    // runs the ticks one run of its 18 frames' ticks runs, which the keys do
    // not change, since the program reads them and goes on alike.
    let twin = session.open("spectrum48");
    session.run(&twin, json!({ "frames": 1 }));
    let ran = session.run(&twin, json!({ "ticks": 18 * SPECTRUM_FRAME_TICKS }));
    assert_eq!(typed["ticks"], ran["ticks"], "{typed}");
}

// Typing takes its ticks whatever the program does: one that halts for good
// partway through the last stretch, a key's letting go, does not end the
// typing there. The loop counts BC down from &3495, 26 T-states a pass
// (UM0080), some five frames, and a key is held for 3 and let go for 6.
#[test]
fn typing_takes_its_ticks_though_the_processor_halts_for_good() {
    let program = [
        DI, LD_BC, 0x95, 0x34, DEC_BC, LD_A_B, OR_C, JR_NZ, 0xFB, HALT,
    ];
    let roms = Roms::programmed("spectrum48.rom", &program);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let typed = session.answer("type", json!({ "handle": handle, "text": "a" }));
    assert_eq!(typed["stopped"], "typed", "{typed}");
    let ticks = typed["ticks"].as_u64().unwrap();
    assert!(
        (9 * SPECTRUM_FRAME_TICKS..9 * SPECTRUM_FRAME_TICKS + 4).contains(&ticks),
        "{typed}"
    );
    assert_eq!(session.inspect(&handle, "cpu.halted"), true);
}

// A stretch of typing that ends in a chain of prefixes ends the typing
// there, as it ends a run, with the keyboard as it stood: here the chain is
// all of memory, so the first stretch ends in it.
#[test]
fn typing_caught_in_a_chain_of_prefixes_stops_there() {
    let roms = Roms::programmed("spectrum48.rom", &[PREFIX_DD; 0x4000]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.fill(&handle, 0x4000, &[PREFIX_DD; 0xC000]);
    let typed = session.answer("type", json!({ "handle": handle, "text": "ab" }));
    assert_eq!(
        (&typed["settled"], &typed["stopped"]),
        (&json!(false), &json!("unsettled")),
        "{typed}"
    );
    assert!(
        typed["ticks"].as_u64().unwrap() < 3 * SPECTRUM_FRAME_TICKS + 1000,
        "{typed}"
    );
    assert!(
        session
            .inspect(&handle, "keyboard.lines")
            .as_array()
            .unwrap()
            .iter()
            .all(|line| *line == 0xFF)
    );
}

#[test]
fn text_a_keyboard_cannot_type_is_refused_before_a_key_goes_down() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    let spectrum = session.open("spectrum48");
    for (handle, text, refused) in [
        (
            &cpc,
            "PRINT £5".to_owned(),
            "'£' is no ASCII character, and text is typed from ASCII; a key it has is held \
             through run's keys",
        ),
        (
            &spectrum,
            "p2\t2".to_owned(),
            "no key of a spectrum48 types '\\t', alone or with a shift; any key is held through \
             run's keys",
        ),
        (
            &cpc,
            String::new(),
            "text holds 0 characters, and from 1 to 250 are typed at a cpc6128 at once, 1500 \
             frames' worth; give a character or more",
        ),
        (
            &cpc,
            "a".repeat(251),
            "text holds 251 characters, and from 1 to 250 are typed at a cpc6128 at once, 1500 \
             frames' worth; type the rest in another request",
        ),
        (
            &spectrum,
            "a".repeat(167),
            "text holds 167 characters, and from 1 to 166 are typed at a spectrum48 at once, 1500 \
             frames' worth; type the rest in another request",
        ),
    ] {
        assert_eq!(
            session.refusal("type", json!({ "handle": handle, "text": text })),
            refused
        );
    }
    for handle in [cpc, spectrum] {
        let ran = session.run(&handle, json!({ "ticks": 1 }));
        assert_eq!(ran["tick"], ran["ticks"], "{handle} ran before");
        assert!(
            session
                .inspect(&handle, "keyboard.lines")
                .as_array()
                .unwrap()
                .iter()
                .all(|line| *line == 0xFF),
            "{handle}"
        );
    }
}

// A blank disc of AMSDOS's DATA format, laid out as "Extended DiSK image
// definition" (Marco Vieth, Ulrich Doewich and Kevin Thacker, with
// extensions by John Elliott and Simon Owen,
// https://cpctech.cpcwiki.de/docs/extdsk.html) lays an image out: a disc
// information block of &100 bytes, "EXTENDED CPC DSK File\r\nDisk-Info\r\n",
// the name of its creator, the number of tracks at &30 and of sides at &31,
// and a track size table from &34 of each track's "high byte of track
// length", which "includes the size of the TRACK INFORMATION BLOCK (256
// bytes)"; then each track's information block, "Track-Info\r\n", the track
// and the side at &10 and &11, the sector size at &14, the number of sectors
// at &15, GAP#3 at &16 and the filler at &17, and a sector information list
// from &18 of eight bytes a sector, C, H, R, N, the two status registers and
// the data's length, followed by the sectors' data. The format is SOFT
// 968's, The Amstrad 6128 Firmware Manual
// (https://archive.org/details/SOFT968TheAmstrad6128FirmwareManual): in
// section 9.4, "Single sided", "512 byte physical sector size", "40 tracks
// numbered 0 to 39", and for the DATA format "9 sectors per track numbered
// #C1 to #C9", "0 reserved tracks" and "2 to 1 sector interleave"; in its
// table of the format in section 9.9, 82 for "gap length (format)", "179
// number of blocks-1" and "2 directory blocks", which leave 178 blocks of
// 1K free. Every byte is &E5, which AMSDOS's ROM takes as the "user number
// for deleted files" (its disassembly,
// https://cpctech.cpcwiki.de/docs/amsdos.asm), so that the directory, in the
// disc's first blocks, holds nothing; SOFT 968 prints the format's filler as
// #E9, which the ROM's own table of the format does not hold.
fn blank_data_disc() -> Vec<u8> {
    const TRACKS: u8 = 40;
    const SECTORS: [u8; 9] = [0xC1, 0xC6, 0xC2, 0xC7, 0xC3, 0xC8, 0xC4, 0xC9, 0xC5];
    let track_length = 0x100 + SECTORS.len() * 512;
    let mut disc = vec![0; 0x100];
    disc[..0x22].copy_from_slice(b"EXTENDED CPC DSK File\r\nDisk-Info\r\n");
    disc[0x22..0x30].copy_from_slice(b"colophon-mcp  ");
    disc[0x30] = TRACKS;
    disc[0x31] = 1;
    for track in 0..TRACKS {
        disc[0x34 + usize::from(track)] = u8::try_from(track_length / 0x100).unwrap();
        let mut block = vec![0; 0x100];
        block[..0x0C].copy_from_slice(b"Track-Info\r\n");
        block[0x10] = track;
        block[0x14] = 2;
        block[0x15] = u8::try_from(SECTORS.len()).unwrap();
        block[0x16] = 82;
        block[0x17] = 0xE5;
        for (index, sector) in SECTORS.iter().enumerate() {
            let at = 0x18 + 8 * index;
            block[at..at + 8].copy_from_slice(&[track, 0, *sector, 2, 0, 0, 0x00, 0x02]);
        }
        block.resize(track_length, 0xE5);
        disc.extend(block);
    }
    disc
}

#[test]
fn a_disc_goes_into_a_cpcs_drive_named_by_its_hash() {
    let roms = Roms::all();
    let disc = blank_data_disc();
    let path = roms.write("blank.dsk", &disc);
    let mut session = Session::start(&roms);
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "ticks": 100 }));
    let tick = session.run(&handle, json!({ "ticks": 1 }))["tick"].clone();
    for (drive, asked) in [("A", None), ("B", Some("B")), ("A", Some("A"))] {
        let mut arguments = json!({ "handle": handle, "path": path });
        if let Some(asked) = asked {
            arguments["drive"] = json!(asked);
        }
        assert_eq!(
            session.answer("disc_insert", arguments),
            json!({
                "handle": handle,
                "drive": drive,
                "tick": tick,
                "path": path,
                "sha256": sha256(&disc),
            })
        );
    }
    assert_eq!(fs::read(&path).unwrap(), disc);
}

#[test]
fn a_disc_is_refused_where_it_cannot_go_with_what_would_take_it() {
    let roms = Roms::all();
    let not_a_disc = roms.write("not.dsk", &[0; 512]);
    let missing = roms.0.join("missing.dsk");
    let mut session = Session::start(&roms);
    let spectrum = session.open("spectrum48");
    let cpc464 = session.open("cpc464");
    let cpc6128 = session.open("cpc6128");
    let refusal = |session: &mut Session, handle: &str, path: &Path, drive: &str| {
        session.refusal(
            "disc_insert",
            json!({ "handle": handle, "path": path, "drive": drive }),
        )
    };
    assert_eq!(
        refusal(&mut session, &spectrum, &not_a_disc, "A"),
        "a spectrum48 has no disc drive; a cpc664 or a cpc6128 takes a disc, and a Spectrum a tape"
    );
    assert_eq!(
        refusal(&mut session, &cpc464, &not_a_disc, "A"),
        "a cpc464 is built here as the emulator's command line builds one given no disc, without \
         a disc interface, and nothing here fits one afterwards; a cpc664 or a cpc6128 takes a \
         disc"
    );
    assert_eq!(
        refusal(&mut session, &cpc6128, &not_a_disc, "B"),
        format!(
            "{}: the image does not begin like a disc image; a disc is a DSK image, of the \
             original layout or the extended one",
            not_a_disc.display()
        )
    );
    assert_eq!(
        refusal(&mut session, &cpc6128, &roms.0, "A"),
        format!(
            "{} is not a file of at most 16777216 bytes; a disc is a DSK image, of the original \
             layout or the extended one",
            roms.0.display()
        )
    );
    let refused = refusal(&mut session, &cpc6128, &missing, "A");
    assert!(
        refused.starts_with(&format!("cannot read {}: ", missing.display()))
            && refused.ends_with(&format!(
                "; a disc is named by its path on the file system the server runs on, and a \
                 relative path is taken from the server's working directory, {}",
                std::env::current_dir().unwrap().display()
            )),
        "{refused}"
    );
    let refused = refusal(&mut session, &cpc6128, &not_a_disc, "C");
    assert!(
        refused.contains("`A`") && refused.contains("`B`"),
        "{refused}"
    );
}

// A tape of one BASIC program, as the 48K's SAVE leaves it: "two blocks on
// tape: a 19 byte header block and a variable length data block", each
// with "a flag byte: this is 0x00 for header blocks and 0xff for data
// blocks", then "the actual data", then "a checksum byte, calculated such
// that XORing all the data bytes together (including the flag byte)
// produces 0x00"; the header's 17 bytes are "Type (0,1,2 or 3)", "Filename
// (padded with blanks)", "Length of data block", "Parameter 1" and
// "Parameter 2", the type 0 for "a PROGRAM", whose "parameter 1 holds the
// autostart line number" and "parameter 2 holds the start of the variable
// area relative to the start of the program" (16K/48K reference). In a .tap
// "all blocks start with two bytes specifying how many bytes will follow
// (not counting the two length bytes)" ("Emulator file formats", World of
// Spectrum FAQ, https://worldofspectrum.org/faq/reference/formats.htm).
fn program_tape(name: &[u8; 10], autostart: u16, program: &[u8]) -> Vec<u8> {
    let length = u16::try_from(program.len()).unwrap();
    let mut header = vec![0];
    header.extend(name);
    for word in [length, autostart, length] {
        header.extend(word.to_le_bytes());
    }
    [tape_block(0x00, &header), tape_block(0xFF, program)].concat()
}

fn tape_block(flag: u8, data: &[u8]) -> Vec<u8> {
    let mut block = vec![flag];
    block.extend(data);
    block.push(block.iter().fold(0, |checksum, byte| checksum ^ byte));
    let mut tap = u16::try_from(block.len()).unwrap().to_le_bytes().to_vec();
    tap.extend(block);
    tap
}

// Line 10, POKE 30000,42, as a 48K holds a line of BASIC: "the line number
// here is stored with its more significant byte first", then the length of
// what follows it, then its text and ENTER, and "a numerical constant in the
// program is followed by its binary form, using the character CHR$ 14
// followed by five bytes", of which for a whole number "the first byte is
// 0", "the second byte is 0 for a positive number", "the third and fourth
// bytes are the less and more significant bytes of the number" and "the
// fifth byte is 0" (ZX Spectrum BASIC programming, ch. 24,
// https://worldofspectrum.org/ZXBasicManual/zxmanchap24.html). POKE is
// character 244 (appendix A).
fn poking_program() -> Vec<u8> {
    let number = |digits: &[u8], value: u16| {
        let [low, high] = value.to_le_bytes();
        [digits, &[14, 0, 0, low, high, 0]].concat()
    };
    let text = [
        &[244][..],
        &number(b"30000", 30000),
        b",",
        &number(b"42", 42),
        &[0x0D],
    ]
    .concat();
    let mut line = vec![0x00, 10];
    line.extend(u16::try_from(text.len()).unwrap().to_le_bytes());
    line.extend(text);
    line
}

// Counts each change of the level the EAR socket hears, bit 6 of port &FE
// (16K/48K reference), into &8000: B at &FF selects no half-row of keys.
const COUNTING_EDGES: [u8; 23] = [
    DI, LD_HL, 0x00, 0x00, LD_E, 0x00, LD_BC, 0xFE, 0xFF, IN_A_C[0], IN_A_C[1], AND_N, 0x40, CP_E,
    JR_Z, 0xF9, LD_E_A, INC_HL, LD_HL_TO, 0x00, 0x80, JR, 0xF2,
];

// A header block opens with "a leader consisting of 8063 (for header
// blocks) or 3223 (data blocks) pulses, each of which has a duration of
// 2168 tstates" (16K/48K reference), each pulse an edge, so ten frames of
// PLAY from a tape's start hear 698880 / 2168 of them, 322.4, which is 322
// or 323 as the first pulse is low or high; the program counts each well
// inside its pulse. The emulator's reader plays a .tap's blocks as the TZX
// format's standard block, ID 10, which is followed by a "Pause after this
// block (ms.) {1000}" ("TZX format" v1.20,
// https://worldofspectrum.net/TZXformat.html), so 265 frames on, past the
// header's 250 of leader and its 19 bytes, the tape is silent for ten
// frames, and a tape put in in its place plays from its own start.
#[test]
fn a_tape_plays_from_the_tick_it_goes_into_the_deck() {
    let roms = Roms::programmed("spectrum48.rom", &COUNTING_EDGES);
    let tape = program_tape(b"TEST      ", 10, &poking_program());
    let path = roms.write("program.tap", &tape);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    // The edges counted over a run of these frames, and the tick it ends at.
    let edges = |session: &mut Session, frames: u64| {
        let before = bytes_of(&session.peek(&handle, json!("&8000"), 2));
        let ran = session.run(&handle, json!({ "ticks": frames * SPECTRUM_FRAME_TICKS }));
        let after = bytes_of(&session.peek(&handle, json!("&8000"), 2));
        let counted = u16::from_le_bytes([after[0], after[1]])
            .wrapping_sub(u16::from_le_bytes([before[0], before[1]]));
        (counted, ran["tick"].clone())
    };
    let (counted, tick) = edges(&mut session, 10);
    assert_eq!(counted, 0);
    assert_eq!(
        session.answer("tape_insert", json!({ "handle": handle, "path": path })),
        json!({ "handle": handle, "tick": tick, "path": path, "sha256": sha256(&tape) })
    );
    assert!([322, 323].contains(&edges(&mut session, 10).0));
    edges(&mut session, 255);
    assert_eq!(edges(&mut session, 10).0, 0);
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    assert!([322, 323].contains(&edges(&mut session, 10).0));
}

#[test]
fn a_tape_is_refused_where_it_cannot_go_with_what_would_take_it() {
    let roms = Roms::all();
    let not_a_tape = roms.write("not.tap", &[5, 0, 0xFF, 1]);
    let mut session = Session::start(&roms);
    let cpc = session.open("cpc6128");
    let spectrum = session.open("spectrum48");
    assert_eq!(
        session.refusal("tape_insert", json!({ "handle": cpc, "path": not_a_tape })),
        "a cpc6128 is given no tape here: the emulator has never loaded a tape through a CPC's \
         firmware, so nothing here could hold one to what the machine did; a spectrum48 takes a \
         tape, and a cpc664 or a cpc6128 a disc"
    );
    let refused = session.refusal(
        "tape_insert",
        json!({ "handle": spectrum, "path": not_a_tape }),
    );
    assert!(
        refused.starts_with(&format!("{} ", not_a_tape.display()))
            && refused.ends_with(
                "; a tape is a .tap, of bare blocks at the ROM's own timings, or a .tzx"
            ),
        "{refused}"
    );
}

#[test]
fn a_snapshot_saved_is_what_a_machine_read_from_it_saves_again() {
    let roms = Roms::all();
    let snapshot = spectrum_snapshot();
    let from = roms.write("spectrum.sna", &snapshot);
    let to = roms.0.join("spectrum-saved.sna");
    let mut session = Session::start(&roms);
    let handle = session.open_from("spectrum48", &from);
    assert_eq!(
        session.answer("snapshot_save", json!({ "handle": handle, "path": to })),
        json!({ "handle": handle, "tick": 0, "path": to, "sha256": sha256(&snapshot) })
    );
    assert_eq!(fs::read(&to).unwrap(), snapshot);
    // A CPC's, saved after a stretch of running, holds what a machine read
    // from it saves again.
    for (machine, kilobytes) in [("cpc464", 64), ("cpc6128", 128)] {
        let from = roms.write("cpc.sna", &cpc_snapshot(kilobytes, 1));
        let opened = session.open_from(machine, &from);
        session.run(&opened, json!({ "ticks": 1000 }));
        let saved = roms.0.join(format!("{machine}-saved.sna"));
        let answer = session.answer("snapshot_save", json!({ "handle": opened, "path": saved }));
        let saved = fs::read(&saved).unwrap();
        assert_eq!(answer["sha256"], sha256(&saved));
        let reopened = session.open_from(machine, &roms.write("reopened.sna", &saved));
        let again = roms.0.join(format!("{machine}-again.sna"));
        session.answer(
            "snapshot_save",
            json!({ "handle": reopened, "path": again }),
        );
        assert_eq!(fs::read(&again).unwrap(), saved, "{machine}");
    }
}

#[test]
fn a_snapshot_is_written_only_where_no_file_is_and_only_between_instructions() {
    let roms = Roms::programmed("spectrum48.rom", &[PREFIX_DD; 0x4000]);
    let taken = roms.write("taken.sna", b"kept");
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    assert_eq!(
        session.refusal("snapshot_save", json!({ "handle": handle, "path": taken })),
        format!(
            "{} is taken, and a file is written only where none is: give a path that names \
             nothing yet",
            taken.display()
        )
    );
    assert_eq!(fs::read(&taken).unwrap(), b"kept");
    let nowhere = roms.0.join("nowhere/saved.sna");
    let refused = session.refusal(
        "snapshot_save",
        json!({ "handle": handle, "path": nowhere }),
    );
    assert!(
        refused.starts_with(&format!("cannot write {}: ", nowhere.display()))
            && refused.ends_with(&format!(
                "; a file is written at a path on the file system the server runs on, in a \
                 directory that is there, and a relative path is taken from the server's working \
                 directory, {}",
                std::env::current_dir().unwrap().display()
            )),
        "{refused}"
    );
    let unsettled = roms.0.join("unsettled.sna");
    session.step(&handle, 1);
    assert_eq!(
        session.refusal(
            "snapshot_save",
            json!({ "handle": handle, "path": unsettled })
        ),
        format!(
            "{handle} stands inside an instruction, as a run or a step leaves it in a chain of \
             prefixes, and a snapshot keeps no instruction half run; step it on to the \
             instruction's end"
        )
    );
    assert!(!unsettled.exists());
    let twin = session.open("spectrum48");
    session.set(&twin, json!({ "cpu.sp": 0x3000 }));
    assert_eq!(
        session.refusal(
            "snapshot_save",
            json!({ "handle": twin, "path": unsettled })
        ),
        format!(
            "{twin} has its stack below the RAM, where the program counter cannot be pushed; a \
             Spectrum's snapshot keeps the program counter in the two bytes below the stack \
             pointer, so cpu.sp stands from &4002 to &FFFF"
        )
    );
    assert!(!unsettled.exists());
}

// The picture an answer carries beside its JSON.
fn picture_data(result: &Value) -> Value {
    assert_eq!(result["isError"], false, "{result}");
    let data = result["content"][1]["data"].clone();
    assert!(data.is_string(), "{result}");
    data
}

// Everything a client reads of a machine: every field, the memory as the
// processor reads it, the picture on its monitor, and on a CPC the whole of
// its RAM, read as a screen of one bit to a pixel, 64K at a time.
fn everything(session: &mut Session, handle: &str) -> Vec<Value> {
    let mut seen = vec![
        session.inspect(handle, ""),
        json!(memory_from(session, handle, 0)),
        picture_data(&session.call("monitor", json!({ "handle": handle }))),
    ];
    if handle.starts_with("cpc") {
        let ram_size = session.inspect(handle, "ram_size").as_u64().unwrap();
        for base in (0..ram_size).step_by(0x10000) {
            let read = session.call(
                "screen",
                json!({
                    "handle": handle, "reading": "linear", "base": base,
                    "width": 128, "height": 512, "mode": 2,
                }),
            );
            seen.push(picture_data(&read));
        }
    }
    seen
}

fn keep(session: &mut Session, handle: &str) -> String {
    let kept = session.answer("state_keep", json!({ "handle": handle }));
    kept["state"].as_str().expect("a state").to_owned()
}

fn restore(session: &mut Session, handle: &str, state: &str) -> Value {
    session.answer("state_restore", json!({ "handle": handle, "state": state }))
}

// From HL on, round again incrementing the byte at HL and HL itself: on a
// Spectrum from 16384, the first byte of the screen (16K/48K reference). On a
// 6128, once its screen is standard, the same from &4000 in RAM configuration
// 2, which puts the second 64K's blocks 4 to 7 in the processor's four
// quarters, and from &C000, the screen, with IX in configuration 0 ("Ram
// management", cpctech), by turns.
fn spectrum_counting_on_its_screen() -> Vec<u8> {
    [&[DI, LD_HL, 0x00, 0x40][..], &COUNTING_LOOP].concat()
}

fn cpc_counting_in_both_64ks() -> Vec<u8> {
    let mut program = cpc_screen_program();
    program.pop();
    program.extend([LD_HL, 0x00, 0x40, PREFIX_DD, LD_HL, 0x00, 0xC0]);
    program.extend([LD_BC, 0xC2, 0x7F, OUT_C_C[0], OUT_C_C[1], INC_AT_HL, INC_HL]);
    program.extend([LD_BC, 0xC0, 0x7F, OUT_C_C[0], OUT_C_C[1]]);
    program.extend([PREFIX_DD, INC_AT_HL, 0x00, PREFIX_DD, INC_HL, JR, 0xED]);
    program
}

// A machine restored stands as it was kept, though it ran on since, and runs
// on as it ran on from there: a Spectrum writing its screen, and a 6128
// writing its screen and its second 64K.
#[test]
fn a_machine_restored_stands_as_kept_and_runs_on_as_it_ran_on() {
    let roms = Roms::programmed("spectrum48.rom", &spectrum_counting_on_its_screen());
    roms.write_at("cpc6128.rom", 0, &cpc_counting_in_both_64ks());
    let mut session = Session::start(&roms);
    for machine in ["spectrum48", "cpc6128"] {
        let handle = session.open(machine);
        session.run(&handle, json!({ "frames": 3 }));
        let state = keep(&mut session, &handle);
        let kept = everything(&mut session, &handle);
        let stretch = json!({ "frames": 2 });
        session.run(&handle, stretch.clone());
        let ran_on = everything(&mut session, &handle);
        assert!(ran_on != kept, "{machine}");
        let restored = restore(&mut session, &handle, &state);
        assert_eq!(restored["state"], state.as_str());
        assert!(everything(&mut session, &handle) == kept, "{machine}");
        session.run(&handle, stretch);
        assert!(everything(&mut session, &handle) == ran_on, "{machine}");
    }
}

// Kept inside a chain of prefixes, a machine is kept inside its instruction,
// where a snapshot has no place to keep it, and runs on from there alike.
#[test]
fn a_machine_kept_inside_an_instruction_runs_on_from_inside_it() {
    let roms = Roms::programmed("spectrum48.rom", &[PREFIX_DD; 0x4000]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.fill(&handle, 0x4000, &[PREFIX_DD; 0xC000]);
    let ran = session.run(&handle, json!({ "ticks": 100 }));
    assert_eq!(ran["settled"], false, "{ran}");
    let state = keep(&mut session, &handle);
    let kept = everything(&mut session, &handle);
    session.run(&handle, json!({ "ticks": 20000 }));
    let ran_on = everything(&mut session, &handle);
    let restored = restore(&mut session, &handle, &state);
    assert_eq!(
        (&restored["tick"], &restored["pc"]),
        (&ran["tick"], &ran["pc"])
    );
    assert!(everything(&mut session, &handle) == kept);
    session.run(&handle, json!({ "ticks": 20000 }));
    assert!(everything(&mut session, &handle) == ran_on);
}

// What a stretch does, gathered and counted again from a state kept before
// it, is what was gathered and counted as it ran.
#[test]
fn a_stretch_gathered_again_from_a_state_is_the_stretch_gathered_live() {
    let roms = Roms::programmed("spectrum48.rom", &COUNTING_FROM_9000);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 3000 }));
    let state = keep(&mut session, &handle);
    let stretch = json!({
        "ticks": 20000,
        "gather": [{ "write": "&9000", "to": "&9FFF" }],
        "count": [{ "execute": "&0000", "to": "&3FFF" }],
    });
    let live = session.run(&handle, stretch.clone());
    restore(&mut session, &handle, &state);
    assert_eq!(session.run(&handle, stretch), live);
}

// Each state is a branch to come back to, and restoring one disturbs no
// other; the journal is the branch's.
#[test]
fn branches_kept_from_one_machine_stand_apart() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    let base = keep(&mut session, &handle);
    session.poke(&handle, json!("&8000"), "11");
    let first = keep(&mut session, &handle);
    restore(&mut session, &handle, &base);
    session.poke(&handle, json!("&8000"), "22");
    session.poke(&handle, json!("&8001"), "33");
    let second = keep(&mut session, &handle);
    let journal = |session: &mut Session| -> Vec<Value> {
        let journal = session.answer("journal", json!({ "handle": handle }));
        journal["entries"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|entry| entry["arguments"].clone())
            .collect()
    };
    for (state, memory, arguments) in [
        (
            &first,
            "1100",
            vec![json!({ "address": "&8000", "bytes": "11" })],
        ),
        (
            &second,
            "2233",
            vec![
                json!({ "address": "&8000", "bytes": "22" }),
                json!({ "address": "&8001", "bytes": "33" }),
            ],
        ),
        (&base, "0000", vec![]),
        (
            &first,
            "1100",
            vec![json!({ "address": "&8000", "bytes": "11" })],
        ),
    ] {
        restore(&mut session, &handle, state);
        assert_eq!(session.peek(&handle, json!("&8000"), 2), memory, "{state}");
        assert_eq!(journal(&mut session), arguments, "{state}");
    }
}

// A tape put in after a state was kept is taken out again by the restore,
// and the tape that was in plays on from where it stood then. 265 frames
// from its start it is silent, between its header and its program
// (a_tape_plays_from_the_tick_it_goes_into_the_deck); 60 frames on it plays
// its program's leader, and the tape put in since its own.
#[test]
fn a_tape_comes_back_with_a_state_where_it_had_played_to() {
    let roms = Roms::programmed("spectrum48.rom", &COUNTING_EDGES);
    let path = roms.write(
        "program.tap",
        &program_tape(b"TEST      ", 10, &poking_program()),
    );
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    session.run(&handle, json!({ "ticks": 265 * SPECTRUM_FRAME_TICKS }));
    let state = keep(&mut session, &handle);
    session.run(&handle, json!({ "ticks": 60 * SPECTRUM_FRAME_TICKS }));
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    restore(&mut session, &handle, &state);
    let before = bytes_of(&session.peek(&handle, json!("&8000"), 2));
    session.run(&handle, json!({ "ticks": 10 * SPECTRUM_FRAME_TICKS }));
    assert_eq!(bytes_of(&session.peek(&handle, json!("&8000"), 2)), before);
}

#[test]
fn a_state_is_named_kept_from_its_machine_and_refused_elsewhere() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let spectrum = session.open("spectrum48");
    let cpc = session.open("cpc6128");
    assert_eq!(
        session.refusal(
            "state_restore",
            json!({ "handle": spectrum, "state": "state-1" })
        ),
        format!(
            "no state is called state-1; no state is held of {spectrum}, and state_keep keeps \
             one"
        )
    );
    session.run(&spectrum, json!({ "ticks": 500 }));
    let first = keep(&mut session, &spectrum);
    let second = keep(&mut session, &spectrum);
    let cpcs = keep(&mut session, &cpc);
    assert_eq!(
        session.answer("machine_list", json!({}))["machines"],
        json!([
            {
                "handle": spectrum,
                "machine": "spectrum48",
                "states": [{ "state": first, "tick": 500 }, { "state": second, "tick": 500 }],
            },
            { "handle": cpc, "machine": "cpc6128", "states": [{ "state": cpcs, "tick": 0 }] },
        ])
    );
    assert_eq!(
        session.refusal("state_restore", json!({ "handle": cpc, "state": first })),
        format!(
            "{first} was kept from {spectrum}, and a state is restored into the machine it was \
             kept from and dropped from it alone"
        )
    );
    session.answer("state_drop", json!({ "handle": spectrum, "state": first }));
    assert_eq!(
        session.refusal(
            "state_restore",
            json!({ "handle": spectrum, "state": first })
        ),
        format!("no state is called {first}; the states kept from {spectrum} are {second}")
    );
    let mut names = BTreeSet::from([first.clone(), second.clone(), cpcs.clone()]);
    for _ in 1..64 {
        assert!(names.insert(keep(&mut session, &spectrum)));
    }
    let refused = session.refusal("state_keep", json!({ "handle": spectrum }));
    assert!(
        refused.starts_with(&format!(
            "{spectrum} has 64 states kept, the most one machine keeps; state_drop lets one of \
             them go: {second}, "
        )),
        "{refused}"
    );
    assert!(names.insert(keep(&mut session, &cpc)));
    session.answer("machine_close", json!({ "handle": spectrum }));
    let reopened = session.open("spectrum48");
    assert_eq!(
        session.refusal(
            "state_restore",
            json!({ "handle": reopened, "state": second })
        ),
        format!(
            "no state is called {second}; no state is held of {reopened}, and state_keep keeps \
             one"
        )
    );
    assert!(names.insert(keep(&mut session, &reopened)));
}

// A page of the journal holds as many entries as fit in 8192 characters,
// each as the answer writes it, and at least one while any remain, however
// long: a poke of 4096 bytes is some 8200 characters on its own.
#[test]
fn the_journal_is_answered_a_page_at_a_time() {
    let roms = Roms::all();
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    // Entries of many lengths, so that some page's next entry would have
    // fitted a budget even a little larger than the page's.
    let lengths = [
        vec![4096; 2],
        (1..=150).collect(),
        vec![4096],
        (1..=150).rev().collect(),
    ];
    for bytes in lengths.concat() {
        session.poke(&handle, json!("&8000"), &"00".repeat(bytes));
    }
    let mut entries = Vec::new();
    let mut from = 0;
    loop {
        let journal = session.answer("journal", json!({ "handle": handle, "from": from }));
        let page: Vec<Value> = journal["entries"].as_array().unwrap().clone();
        let lengths: Vec<usize> = page
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap().chars().count())
            .collect();
        let fits = lengths.iter().sum::<usize>() <= 8192 || page.len() == 1;
        assert!(fits && !page.is_empty(), "from {from}: {lengths:?}");
        entries.extend(page);
        if journal["next"].is_null() {
            assert_eq!(journal["count"], entries.len());
            break;
        }
        let next = journal["next"].as_u64().unwrap() as usize;
        assert_eq!(next, entries.len());
        let following = session.answer("journal", json!({ "handle": handle, "from": next }));
        let first = serde_json::to_string(&following["entries"][0])
            .unwrap()
            .chars()
            .count();
        assert!(lengths.iter().sum::<usize>() + first > 8192, "from {from}");
        from = next;
    }
    assert_eq!(entries.len(), 304);
    let count = entries.len();
    let end = session.answer("journal", json!({ "handle": handle, "from": count }));
    assert_eq!(
        (end["entries"].clone(), end["next"].clone()),
        (json!([]), Value::Null)
    );
    assert_eq!(
        session.refusal("journal", json!({ "handle": handle, "from": count + 1 })),
        format!(
            "from is {}, and the journal of {handle} holds {count} entries, so it is answered \
             from 0 to {count}",
            count + 1
        )
    );
}

// The journal names the build and the files a machine was built from, and
// lists what stood it up and changed it, each with its arguments, addresses
// after & and paths absolute, and the tick it began at; a request refused is
// not entered. The same requests made in its order, the first of them its
// machine_open, make a machine that stands as the first stands, a branch
// abandoned by a restore left out.
#[test]
fn the_journal_made_again_makes_the_machine_again() {
    let roms = routine("cpc6128.rom", &COUNTING_FROM_9000);
    let disc = roms.write("blank.dsk", &blank_data_disc());
    let not_a_disc = roms.write("not.dsk", &[0; 512]);
    let mut session = Session::start(&roms);
    let opened = session.answer("machine_open", json!({ "machine": "cpc6128" }));
    let handle = opened["handle"].as_str().unwrap().to_owned();
    let ask = |session: &mut Session, handle: &str, tool: &str, arguments: &Value| {
        let mut arguments = arguments.clone();
        arguments["handle"] = json!(handle);
        session.answer(tool, arguments)
    };
    let mut asked = vec![(
        json!("machine_open"),
        json!({ "machine": "cpc6128" }),
        json!(0),
    )];
    let mut now = json!(0);
    for (tool, arguments) in [
        ("set", json!({ "values": { "cpu.b": 7 } })),
        ("poke", json!({ "address": "&9800", "bytes": "AABB" })),
        ("run", json!({ "ticks": 5000, "keys": ["Q", "(FIRE 2)"] })),
        ("disc_insert", json!({ "path": disc, "drive": "B" })),
        ("type", json!({ "text": "a\n" })),
        (
            "call",
            json!({ "address": "&0100", "values": { "cpu.a": 1, "cpu.sp": 0xC000 } }),
        ),
        ("step", json!({ "instructions": 10 })),
        (
            "run",
            json!({
                "frames": 2,
                "until": [{ "write": "&9100" }],
                "gather": [{ "write": "&9000", "to": "&9010" }],
            }),
        ),
    ] {
        asked.push((json!(tool), arguments.clone(), now.clone()));
        let answer = ask(&mut session, &handle, tool, &arguments);
        if !answer["tick"].is_null() {
            now = answer["tick"].clone();
        }
    }
    let state = keep(&mut session, &handle);
    ask(
        &mut session,
        &handle,
        "poke",
        &json!({ "address": "&9900", "bytes": "CC" }),
    );
    restore(&mut session, &handle, &state);
    let count = session.answer("journal", json!({ "handle": handle }))["count"].clone();
    for (tool, arguments) in [
        ("set", json!({ "values": { "cpu.pc": 65536 } })),
        ("disc_insert", json!({ "path": not_a_disc })),
        (
            "call",
            json!({ "address": "&0100", "values": { "cpu.sp": 2 } }),
        ),
        ("type", json!({ "text": "£" })),
    ] {
        let mut arguments = arguments;
        arguments["handle"] = json!(handle);
        session.refusal(tool, arguments);
    }
    assert_eq!(
        session.answer("journal", json!({ "handle": handle }))["count"],
        count
    );
    asked.push((json!("run"), json!({ "ticks": 300 }), now));
    ask(&mut session, &handle, "run", &json!({ "ticks": 300 }));
    let journal = session.answer("journal", json!({ "handle": handle }));
    assert_eq!(journal["files"], opened["files"]);
    let named = session.call("machine_list", json!({}));
    assert_eq!(
        journal["build"],
        named["_meta"]["io.modelcontextprotocol/serverInfo"]["version"]
    );
    let entries = journal["entries"].as_array().unwrap().clone();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (
                entry["tool"].clone(),
                entry["arguments"].clone(),
                entry["tick"].clone()
            ))
            .collect::<Vec<_>>(),
        asked
    );
    assert_eq!(entries[4]["file"]["sha256"], sha256(&blank_data_disc()));
    let again = session.answer("machine_open", entries[0]["arguments"].clone())["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    for entry in &entries[1..] {
        ask(
            &mut session,
            &again,
            entry["tool"].as_str().unwrap(),
            &entry["arguments"],
        );
    }
    assert_eq!(
        session.run(&again, json!({ "ticks": 1 }))["tick"],
        session.run(&handle, json!({ "ticks": 1 }))["tick"]
    );
    assert!(everything(&mut session, &again) == everything(&mut session, &handle));
}

// A Spectrum's journal names the snapshot it was opened from and the tape it
// was given, each by its hash.
#[test]
fn the_journal_names_the_snapshot_and_the_tape_by_their_hashes() {
    let roms = Roms::all();
    let snapshot = spectrum_snapshot();
    let from = roms.write("spectrum.sna", &snapshot);
    let tape = program_tape(b"TEST      ", 10, &poking_program());
    let path = roms.write("program.tap", &tape);
    let mut session = Session::start(&roms);
    let handle = session.open_from("spectrum48", &from);
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    let journal = session.answer("journal", json!({ "handle": handle }));
    assert_eq!(
        journal["entries"],
        json!([
            {
                "tick": 0,
                "tool": "machine_open",
                "arguments": { "machine": "spectrum48", "snapshot": from },
                "file": { "path": from, "sha256": sha256(&snapshot) },
            },
            {
                "tick": 0,
                "tool": "tape_insert",
                "arguments": { "path": path },
                "file": { "path": path, "sha256": sha256(&tape) },
            },
        ])
    );
}

// The journal writes every address after &, whatever form a request gave it
// in, so that a finding reads alike however it was asked for; a range of one
// address is that address, and a port keeps its mask. Values set are kept as
// given.
#[test]
fn the_journal_writes_addresses_after_an_ampersand() {
    let roms = routine("spectrum48.rom", &[DI, HALT]);
    let mut session = Session::start(&roms);
    let handle = session.open("spectrum48");
    session.poke(&handle, json!(32768), "11");
    let poked = session.answer(
        "poke",
        json!({ "handle": handle, "address": "0x8001", "bytes": "22" }),
    );
    assert_eq!(poked, json!({ "address": "&8001", "length": 1 }));
    let ran = json!({
        "ticks": 100,
        "until": [{ "execute": 4096, "to": "#1FFF" }],
        "gather": [{ "write": "$9000", "to": "&9000" }, { "out": 254, "mask": 1 }],
        "count": [{ "in": "#FE", "mask": "0x1" }],
    });
    session.run(&handle, ran);
    session.answer(
        "call",
        json!({
            "handle": handle,
            "address": 256,
            "values": { "cpu.sp": "&8000", "cpu.b": 2 },
            "count": [{ "read": "0X7FFE", "to": 32767 }],
        }),
    );
    let journal = session.answer("journal", json!({ "handle": handle }));
    let arguments: Vec<Value> = journal["entries"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|entry| entry["arguments"].clone())
        .collect();
    assert_eq!(
        arguments,
        [
            json!({ "address": "&8000", "bytes": "11" }),
            json!({ "address": "&8001", "bytes": "22" }),
            json!({
                "ticks": 100,
                "until": [{ "execute": "&1000", "to": "&1FFF" }],
                "gather": [{ "write": "&9000" }, { "out": "&00FE", "mask": "&0001" }],
                "count": [{ "in": "&00FE", "mask": "&0001" }],
            }),
            json!({
                "address": "&0100",
                "values": { "cpu.sp": "&8000", "cpu.b": 2 },
                "count": [{ "read": "&7FFE", "to": "&7FFF" }],
            }),
        ]
    );
}

// A relative path is taken from the server's working directory, and the
// answers and the journal name each file by its absolute path, so that a
// journal is made again from anywhere.
#[test]
fn a_relative_path_is_taken_from_the_working_directory_and_answered_absolute() {
    let roms = Roms::all();
    roms.write("spectrum.sna", &spectrum_snapshot());
    roms.write(
        "program.tap",
        &program_tape(b"TEST      ", 10, &poking_program()),
    );
    roms.write("blank.dsk", &blank_data_disc());
    let directory = fs::canonicalize(&roms.0).unwrap();
    let mut session = Session::beside(&roms);
    let opened = session.answer(
        "machine_open",
        json!({ "machine": "spectrum48", "snapshot": "spectrum.sna" }),
    );
    let handle = opened["handle"].as_str().unwrap().to_owned();
    assert_eq!(
        (&opened["files"][0]["path"], &opened["files"][1]["path"]),
        (
            &json!(directory.join("spectrum48.rom")),
            &json!(directory.join("spectrum.sna"))
        )
    );
    let inserted = session.answer(
        "tape_insert",
        json!({ "handle": handle, "path": "program.tap" }),
    );
    assert_eq!(inserted["path"], json!(directory.join("program.tap")));
    let saved = session.answer(
        "snapshot_save",
        json!({ "handle": handle, "path": "saved.sna" }),
    );
    assert_eq!(saved["path"], json!(directory.join("saved.sna")));
    assert!(directory.join("saved.sna").is_file());
    let journal = session.answer("journal", json!({ "handle": handle }));
    assert_eq!(
        (
            &journal["entries"][0]["arguments"]["snapshot"],
            &journal["entries"][1]["arguments"]["path"]
        ),
        (
            &json!(directory.join("spectrum.sna")),
            &json!(directory.join("program.tap"))
        )
    );
    let cpc = session.open("cpc6128");
    let inserted = session.answer("disc_insert", json!({ "handle": cpc, "path": "blank.dsk" }));
    assert_eq!(inserted["path"], json!(directory.join("blank.dsk")));
    let journal = session.answer("journal", json!({ "handle": cpc }));
    assert_eq!(
        journal["entries"][1]["arguments"]["path"],
        json!(directory.join("blank.dsk"))
    );
    let refused = session.refusal(
        "machine_open",
        json!({ "machine": "spectrum48", "snapshot": "missing.sna" }),
    );
    assert!(
        refused.starts_with(&format!(
            "cannot read {}: ",
            directory.join("missing.sna").display()
        )) && refused.ends_with(&format!(
            "; a snapshot is named by its path on the file system the server runs on, and a \
             relative path is taken from the server's working directory, {}",
            directory.display()
        )),
        "{refused}"
    );
}

// The firmware tier: the emulator's own fetched firmware and its command line,
// which `make test-firmware` asks for.

fn emulator() -> PathBuf {
    PathBuf::from(env!("COLOPHON_MCP_EMULATOR_DIR"))
}

fn firmware() -> PathBuf {
    let roms = emulator().join("roms");
    assert!(
        roms.join("cpc6128.rom").exists(),
        "no firmware in {}: run `make roms` in the emulator",
        roms.display()
    );
    roms
}

// The emulator's command line, run on its own firmware, which must succeed.
fn command_line<S: AsRef<OsStr>>(arguments: impl IntoIterator<Item = S>) {
    let command_line = emulator().join("build/emulator");
    assert!(
        command_line.exists(),
        "no command line at {}: run `make` in the emulator",
        command_line.display()
    );
    let output = Command::new(command_line)
        .args(arguments)
        .arg("--roms")
        .arg(firmware())
        .output()
        .expect("the command line runs");
    assert!(output.status.success(), "{output:?}");
}

// The emulator's command line runs a snapshot by its frames, which are its
// standard frames' ticks, and saves the machine as it then stands.
fn command_line_run(machine: &str, snapshot: &Path, frames: u32, saved: &Path) {
    let frames = frames.to_string();
    command_line([
        OsStr::new("run"),
        snapshot.as_os_str(),
        OsStr::new("--machine"),
        OsStr::new(machine),
        OsStr::new("--frames"),
        OsStr::new(&frames),
        OsStr::new("--save"),
        saved.as_os_str(),
    ]);
}

fn bytes_of(hexadecimal: &str) -> Vec<u8> {
    hexadecimal
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII"), 16).expect("a byte")
        })
        .collect()
}

// From an address to &FFFF, as the processor sees it.
fn memory_from(session: &mut Session, handle: &str, from: u32) -> Vec<u8> {
    let mut memory = Vec::new();
    for address in (from..0x10000).step_by(4096) {
        let length = u16::try_from((0x10000 - address).min(4096)).unwrap();
        memory.extend(bytes_of(&session.peek(handle, json!(address), length)));
    }
    memory
}

// The hashes the emulator's tools/fetch-roms.sh pins the firmware to, checked
// there by shasum or sha256sum, each as the server names the file.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn every_firmware_file_is_named_by_the_hash_the_emulator_pins_it_to() {
    let script = fs::read_to_string(emulator().join("tools/fetch-roms.sh")).expect("the script");
    let pins: Vec<(String, String)> = script
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (file, hash) = (words.next()?, words.next()?);
            let pinned = file.ends_with(".rom") && hash.len() == 64;
            pinned.then(|| (file.to_owned(), hash.to_owned()))
        })
        .collect();
    assert_eq!(pins.len(), 5, "{pins:?}");
    let roms = firmware();
    let mut session = Session::reading(&roms);
    for machine in MACHINES {
        let opened = session.answer("machine_open", json!({ "machine": machine.name }));
        for file in opened["files"].as_array().expect("a list") {
            let path = PathBuf::from(file["path"].as_str().expect("a path"));
            let name = path.file_name().unwrap().to_str().unwrap();
            let (_, pinned) = pins
                .iter()
                .find(|(file, _)| file == name)
                .unwrap_or_else(|| panic!("{name} is pinned"));
            assert_eq!(file["sha256"], pinned.as_str(), "{name}");
        }
    }
}

// The firmware's own table of the 6845's registers for a 50Hz screen is what
// the 6845 holds once the firmware has started. "The Amstrad CPC CRTC
// Compendium" v1.11, ch. 4.1, https://shaker.logonsystem.eu/ACCC1.11-EN.pdf,
// gives the table's address as &5C5, which is where the 6128's ROM holds it;
// the 464's and the 664's hold it a little earlier. R14 and R15 are the
// cursor's, and kept to the widths the 6845 has.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_cpc_starts_on_the_firmwares_own_screen() {
    let roms = firmware();
    let table =
        fs::read(roms.join("cpc6128.rom")).expect("the firmware")[0x05C5..0x05C5 + 14].to_vec();
    let mut session = Session::reading(&roms);
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "frames": 100 }));
    let registers = session.inspect(&handle, "crtc.registers");
    assert_eq!(
        registers.as_array().unwrap()[..14],
        json!(table).as_array().unwrap()[..]
    );
}

// P-RAMT is the "Address of last byte of physical RAM", and FRAMES is
// "Incremented every 20ms", once a frame: ZX Spectrum BASIC programming,
// ch. 25, https://worldofspectrum.org/ZXBasicManual/zxmanchap25.html, where
// P-RAMT is at 23732 and FRAMES, three bytes with the least significant
// first, at 23672.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_starts_with_its_system_variables_as_its_manual_gives_them() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "frames": 150 }));
    assert_eq!(session.peek(&handle, json!(23732), 2), "FFFF");
    let frames = |session: &mut Session| {
        let bytes = bytes_of(&session.peek(&handle, json!(23672), 3));
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
    };
    let before = frames(&mut session);
    session.run(&handle, json!({ "frames": 50 }));
    assert_eq!(frames(&mut session), before + 50);
}

// A program the test writes counts through memory from &8000, through a run
// of ten frames on the server and on the emulator's command line, which must
// leave the same machine. The command line's frames are its standard frames'
// ticks, so the server runs by ticks. Every field of version 1 is held
// against it, at the places "Snapshot (.SNA) file format" gives them.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_cpc_run_from_a_snapshot_stands_where_the_command_line_leaves_it() {
    let scratch = Roms::new(&[]);
    let mut snapshot = cpc_snapshot(128, 1);
    snapshot[0x23..0x25].copy_from_slice(&0x4000u16.to_le_bytes());
    let program = [&[LD_HL, 0x00, 0x80][..], &COUNTING_LOOP].concat();
    snapshot[0x100 + 0x4000..0x100 + 0x4000 + program.len()].copy_from_slice(&program);
    let path = scratch.write("program.sna", &snapshot);
    let saved = scratch.0.join("saved.sna");
    command_line_run("cpc6128", &path, 10, &saved);
    let saved = fs::read(saved).expect("the command line's snapshot");
    let mut session = Session::reading(&firmware());
    let handle = session.open_from("cpc6128", &path);
    session.run(&handle, json!({ "ticks": 10 * CPC_FRAME_TICKS }));
    let cpu = session.inspect(&handle, "cpu");
    let byte = |at: usize| json!(saved[at]);
    let word = |at: usize| json!(u16::from_le_bytes([saved[at], saved[at + 1]]));
    for (name, value) in [
        ("f", byte(0x11)),
        ("a", byte(0x12)),
        ("c", byte(0x13)),
        ("b", byte(0x14)),
        ("e", byte(0x15)),
        ("d", byte(0x16)),
        ("l", byte(0x17)),
        ("h", byte(0x18)),
        ("r", byte(0x19)),
        ("i", byte(0x1A)),
        ("ixl", byte(0x1D)),
        ("ixh", byte(0x1E)),
        ("iyl", byte(0x1F)),
        ("iyh", byte(0x20)),
        ("sp", word(0x21)),
        ("pc", word(0x23)),
        ("im", byte(0x25)),
        ("af_", word(0x26)),
        ("bc_", word(0x28)),
        ("de_", word(0x2A)),
        ("hl_", word(0x2C)),
    ] {
        assert_eq!(cpu[name], value, "{name}");
    }
    assert_eq!(cpu["iff1"], json!(saved[0x1B] & 1 == 1));
    assert_eq!(cpu["iff2"], json!(saved[0x1C] & 1 == 1));
    let gate_array = session.inspect(&handle, "gate_array");
    assert_eq!(gate_array["pen"], byte(0x2E));
    assert_eq!(gate_array["inks"], json!(saved[0x2F..0x40]));
    assert_eq!(gate_array["mode"], json!(saved[0x40] & 0x03));
    assert_eq!(
        gate_array["lower_rom_enabled"],
        json!(saved[0x40] & 0x04 == 0)
    );
    assert_eq!(
        gate_array["upper_rom_enabled"],
        json!(saved[0x40] & 0x08 == 0)
    );
    let ppi = session.inspect(&handle, "ppi");
    assert_eq!(
        (
            &ppi["input"][0],
            &ppi["input"][1],
            &ppi["output"][2],
            &ppi["control"]
        ),
        (&byte(0x56), &byte(0x57), &byte(0x58), &byte(0x59))
    );
    assert_eq!(
        session.inspect(&handle, "crtc.address_register"),
        byte(0x42)
    );
    assert_eq!(
        session.inspect(&handle, "crtc.registers"),
        json!(saved[0x43..0x55])
    );
    assert_eq!(session.inspect(&handle, "upper_rom_number"), byte(0x55));
    assert_eq!(session.inspect(&handle, "psg.selected"), byte(0x5A));
    assert_eq!(
        session.inspect(&handle, "psg.registers"),
        json!(saved[0x5B..0x6B])
    );
    assert_eq!(session.inspect(&handle, "mmr"), json!(0xC0 | saved[0x41]));
    assert_eq!(memory_from(&mut session, &handle, 0), saved[0x100..0x10100]);
    // RAM configuration 2 puts the second 64K's four blocks in order.
    session.set(&handle, json!({ "mmr": 0xC2 }));
    assert_eq!(
        memory_from(&mut session, &handle, 0),
        saved[0x10100..0x20100]
    );
}

// The same on a Spectrum, from its 48K snapshot, whose program counter the
// command line pushes on the stack as it saves: "2 bytes of memory are
// overwritten" ("Emulator file formats"). Interrupts are on, so the ROM's
// handler runs too.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_run_from_a_snapshot_stands_where_the_command_line_leaves_it() {
    let scratch = Roms::new(&[]);
    let mut snapshot = spectrum_snapshot();
    // Interrupt mode 1, so that the ROM's handler is the one called.
    snapshot[25] = 1;
    let program = [&[LD_HL, 0x00, 0xC0][..], &COUNTING_LOOP].concat();
    snapshot[27 + 0x8000 - 0x4000..27 + 0x8000 - 0x4000 + program.len()].copy_from_slice(&program);
    let path = scratch.write("program.sna", &snapshot);
    let saved = scratch.0.join("saved.sna");
    command_line_run("spectrum48", &path, 10, &saved);
    let saved = fs::read(saved).expect("the command line's snapshot");
    let mut session = Session::reading(&firmware());
    let handle = session.open_from("spectrum48", &path);
    session.run(&handle, json!({ "ticks": 10 * SPECTRUM_FRAME_TICKS }));
    let cpu = session.inspect(&handle, "cpu");
    let word = |at: usize| u16::from_le_bytes([saved[at], saved[at + 1]]);
    let stacked = usize::from(word(23));
    let pushed = 27 + stacked - 0x4000;
    assert_eq!(cpu["sp"], json!(stacked + 2));
    assert_eq!(cpu["pc"], json!(word(pushed)));
    for (name, value) in [
        ("hl_", word(1)),
        ("de_", word(3)),
        ("bc_", word(5)),
        ("af_", word(7)),
        ("iyl", u16::from(saved[15])),
        ("iyh", u16::from(saved[16])),
        ("ixl", u16::from(saved[17])),
        ("ixh", u16::from(saved[18])),
        ("i", u16::from(saved[0])),
        ("r", u16::from(saved[20])),
        ("im", u16::from(saved[25])),
        ("l", u16::from(saved[9])),
        ("h", u16::from(saved[10])),
        ("e", u16::from(saved[11])),
        ("d", u16::from(saved[12])),
        ("c", u16::from(saved[13])),
        ("b", u16::from(saved[14])),
        ("f", u16::from(saved[21])),
        ("a", u16::from(saved[22])),
    ] {
        assert_eq!(cpu[name], json!(value), "{name}");
    }
    assert_eq!(cpu["iff2"], json!(saved[19] & 0b100 != 0));
    assert_eq!(session.inspect(&handle, "ula.border"), json!(saved[26]));
    let mut memory = memory_from(&mut session, &handle, 0x4000);
    let mut expected = saved[27..].to_vec();
    for at in [pushed - 27, pushed - 26] {
        memory[at] = 0;
        expected[at] = 0;
    }
    assert_eq!(memory, expected);
}

// MASK-INT, the 48K ROM's handler of the frame's interrupt, counts the frames
// with LD ($5C78),HL at &003E, whose instruction ends at &0041: "The lower
// two bytes of the frame counter (FRAMES) are incremented every 20 ms" (The
// Complete Spectrum ROM Disassembly, as SkoolKit gives it,
// https://skoolkid.github.io/rom/asm/0038.html).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn the_spectrums_frame_count_is_written_by_its_interrupt_handler() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "frames": 150 }));
    let frames = json!([{ "write": "&5C78", "to": "&5C79" }]);
    let gathered = session.run(&handle, json!({ "frames": 2, "gather": frames }));
    let writes: Vec<_> = seen(&gathered["gathered"])
        .into_iter()
        .map(|(kind, address, _, pc)| (kind, address, pc))
        .collect();
    let handler = Some(0x003E);
    assert_eq!(
        writes,
        [
            ("write", 0x5C78, handler),
            ("write", 0x5C79, handler),
            ("write", 0x5C78, handler),
            ("write", 0x5C79, handler),
        ],
        "{gathered}"
    );
    let stopped = session.run(&handle, json!({ "frames": 2, "until": frames }));
    assert_eq!(
        (&stopped["stopped"], &stopped["pc"]),
        (&json!("matched"), &ampersand(0x0041)),
        "{stopped}"
    );
}

// The frame's interrupt is answered at &0038 once a frame, and MASK-INT
// writes FRAMES from &003E once each time; FRAMES is "incremented every 20
// ms" (The Complete Spectrum ROM Disassembly, 0038). Counted over fifty
// frames, both are fifty, and FRAMES is fifty on.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn the_spectrums_interrupt_handler_is_counted_once_a_frame() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "frames": 150 }));
    let frames = |session: &mut Session| {
        u16::from_str_radix(&session.peek(&handle, json!("&5C78"), 2), 16)
            .map(u16::swap_bytes)
            .expect("FRAMES")
    };
    let before = frames(&mut session);
    let ran = session.run(
        &handle,
        json!({ "frames": 50, "count": [{ "execute": "&0038" }, { "execute": "&003E" }] }),
    );
    assert_eq!(
        ran["counted"],
        json!([{ "execute": "&0038", "count": 50 }, { "execute": "&003E", "count": 50 }]),
        "{ran}"
    );
    assert_eq!(frames(&mut session), before.wrapping_add(50));
}

// The Gate Array raises an interrupt each time R52 has counted 52 HSYNCs
// ("The Amstrad CPC CRTC Compendium" v1.11, ch. 27.3.1,
// https://shaker.logonsystem.eu/ACCC1.11-EN.pdf), and the firmware's standard
// frame is 312 lines (ch. 4.1), so it holds six; in interrupt mode 1 the
// processor takes each at &0038 (UM0080).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_cpc_takes_six_interrupts_a_frame() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "frames": 100 }));
    let gathered = session.run(
        &handle,
        json!({ "frames": 10, "gather": [{ "execute": "&0038" }] }),
    );
    assert_eq!(gathered["stopped"], "frames", "{gathered}");
    assert_eq!(seen(&gathered["gathered"]).len(), 60, "{gathered}");
}

// The 6845's registers are selected through &BCxx and written through &BDxx:
// the chip answers A14 low, its RS is A8 and its R/W A9, and "register index
// is 5bit wide" ("The CRTC", Grim, I/O ports,
// https://web.archive.org/web/20240924035112/https://www.grimware.org/doku.php/documentations/devices/crtc).
// What the firmware writes there first, as it starts, is its own table for a
// 50Hz screen at &5C5 (Compendium ch. 4.1), all sixteen registers of it.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn the_firmware_writes_the_6845_its_own_table() {
    let roms = firmware();
    let table =
        fs::read(roms.join("cpc6128.rom")).expect("the firmware")[0x05C5..0x05C5 + 16].to_vec();
    let mut session = Session::reading(&roms);
    let handle = session.open("cpc6128");
    let crtc = json!([
        { "out": "&BC00", "mask": "&4300" },
        { "out": "&BD00", "mask": "&4300" },
    ]);
    let mut written = [None; 16];
    let mut selected = 0;
    loop {
        let ran = session.run(&handle, json!({ "frames": 100, "gather": crtc }));
        for (_, port, value, _) in seen(&ran["gathered"]) {
            let value = value.expect("a byte");
            if port & 0x0100 == 0 {
                selected = usize::from(value as u8 & 0x1F);
            } else if selected < 16 {
                written[selected].get_or_insert(value);
            }
        }
        if ran["stopped"] != "full" {
            break;
        }
    }
    let written: Vec<Option<u64>> = written[..16].to_vec();
    let expected: Vec<Option<u64>> = table.iter().map(|&value| Some(u64::from(value))).collect();
    assert_eq!(written, expected);
}

// PIXEL-ADD, at &22AA in the 48K ROM, "is entered with the co-ordinates of a
// pixel in the BC register pair and returns with HL holding the address of
// the display file byte which contains that pixel and A pointing to the
// position of the pixel within the byte": B is the y, C the x, and A "C mod
// 8" (The Complete Spectrum ROM Disassembly, as SkoolKit gives it,
// https://skoolkid.github.io/rom/asm/22AA.html). Its first instructions
// take B from 175, so y 175 is the picture's top line, whose first byte is
// "the first byte of the screen (16384)" (comp.sys.sinclair FAQ, 16K/48K
// reference); from &22BC to &22C6 it makes L from that line and C divided
// by 8, so x 13 lies in the line's second byte.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_rom_routine_called_returns_what_its_disassembly_says() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "frames": 150 }));
    let before = session.inspect(&handle, "cpu");
    let called = session.answer(
        "call",
        json!({
            "handle": handle,
            "address": "&22AA",
            "values": { "cpu.b": 175, "cpu.c": 13 },
        }),
    );
    let pc = u16::try_from(before["pc"].as_u64().unwrap()).unwrap();
    assert_eq!(
        (&called["stopped"], &called["pc"]),
        (&json!("returned"), &ampersand(pc)),
        "{called}"
    );
    let cpu = session.inspect(&handle, "cpu");
    assert_eq!(
        (&cpu["h"], &cpu["l"], &cpu["a"]),
        (&json!(0x40), &json!(0x01), &json!(5))
    );
    assert_eq!(cpu["sp"], before["sp"]);
}

// The emulator's command line boots a machine for as many of its frames, its
// standard frames' ticks, and writes its picture, which the png crate reads
// back as colours.
fn command_line_screenshot(machine: &str, frames: u64, path: &Path) -> (usize, usize, Vec<u32>) {
    let command_line = emulator().join("build/emulator");
    let output = Command::new(command_line)
        .arg("boot")
        .args(["--machine", machine, "--frames", &frames.to_string()])
        .arg("--roms")
        .arg(firmware())
        .arg("--screenshot")
        .arg(path)
        .output()
        .expect("the command line runs");
    assert!(output.status.success(), "{output:?}");
    let file = fs::File::open(path).expect("the screenshot");
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("a PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("a size")];
    let frame = reader.next_frame(&mut pixels).expect("its pixels");
    assert_eq!(
        (frame.color_type, frame.bit_depth),
        (png::ColorType::Rgb, png::BitDepth::Eight)
    );
    let colours = pixels
        .chunks(3)
        .map(|rgb| u32::from_be_bytes([0, rgb[0], rgb[1], rgb[2]]))
        .collect();
    (frame.width as usize, frame.height as usize, colours)
}

// Each machine booted on its own firmware shows on its monitor what the
// command line's screenshot shows after as many frames, pixel for pixel. The
// server runs on to the end of the instruction standing there, a few ticks
// past the command line, which paints nothing new where the picture stands
// still.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn each_machines_monitor_shows_what_the_command_lines_screenshot_does() {
    let scratch = Roms::new(&[]);
    let mut session = Session::reading(&firmware());
    for machine in MACHINES {
        let name = machine.name;
        let path = scratch.0.join(format!("{name}.png"));
        let (width, height, colours) = command_line_screenshot(name, 100, &path);
        let handle = session.open(name);
        let frame = if name.starts_with("cpc") {
            CPC_FRAME_TICKS
        } else {
            SPECTRUM_FRAME_TICKS
        };
        session.run(&handle, json!({ "ticks": 100 * frame }));
        let (_, monitor) = session.pictured("monitor", json!({ "handle": handle }));
        assert_eq!((monitor.width, monitor.height), (width, height), "{name}");
        let shown: Vec<u32> = monitor
            .indices
            .iter()
            .map(|index| monitor.palette[usize::from(*index)])
            .collect();
        let differing = shown
            .iter()
            .zip(&colours)
            .filter(|(shown, written)| shown != written)
            .count();
        assert_eq!(differing, 0, "{name}");
    }
}

// The firmware's screen, read by screen with nothing asked, is the monitor's
// picture where the display falls: the start the firmware writes into R12
// and R13, its mode 1 and its inks.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn the_firmwares_screen_is_the_6845s_reading_of_its_memory() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "frames": 100 }));
    let (_, monitor) = session.pictured("monitor", json!({ "handle": handle }));
    let (drawn, screen) = session.pictured("screen", json!({ "handle": handle }));
    assert_eq!(
        (
            &drawn["base"],
            &drawn["mode"],
            &drawn["width"],
            &drawn["height"]
        ),
        (&ampersand(0xC000), &json!(1), &json!(80), &json!(200))
    );
    let inks: Vec<u8> = drawn["palette"]
        .as_array()
        .expect("a palette")
        .iter()
        .map(|code| code.as_u64().unwrap() as u8)
        .collect();
    let display = (272 - 208, (70 - 34) * 2);
    let mut lit = 0;
    for y in 0..screen.height {
        let shown = &monitor.indices[(display.1 + y) * monitor.width + display.0..][..640];
        let drawn: Vec<u8> = screen.indices[y * 640..][..640]
            .iter()
            .map(|pen| inks[usize::from(*pen)])
            .collect();
        assert_eq!(shown, drawn.as_slice(), "line {y}");
        lit += screen.indices[y * 640..][..640]
            .iter()
            .filter(|pen| **pen != 0)
            .count();
    }
    assert!(lit > 1000, "{lit} pixels of the firmware's text");
}

// A CPC's character matrices, as its firmware hands them out: TXT GET
// MATRIX, at &BBA5, takes "the character whose matrix is to be found" in A
// and returns with "HL contains the address of the matrix", and "Carry
// false" if "the matrix is in the lower ROM", which is the first 16K of the
// firmware's file; a matrix is "an 8 byte bit significant vector", whose
// "first byte describes the top line of the character" and in which "bit 7
// of a byte refers to the leftmost pixel" (SOFT 968, The Amstrad 6128
// Firmware Manual, https://archive.org/details/SOFT968TheAmstrad6128FirmwareManual).
fn cpc_font(session: &mut Session, handle: &str, machine: &str) -> BTreeMap<[u8; 8], char> {
    let rom = fs::read(firmware().join(format!("{machine}.rom"))).expect("the firmware");
    (b' '..=b'~')
        .map(|code| {
            session.answer(
                "call",
                json!({ "handle": handle, "address": "&BBA5", "values": { "cpu.a": code } }),
            );
            let cpu = session.inspect(handle, "cpu");
            assert_eq!(cpu["f"].as_u64().unwrap() & 1, 0, "{code} is in the ROM");
            let at = usize::try_from(cpu["h"].as_u64().unwrap() * 256 + cpu["l"].as_u64().unwrap())
                .unwrap();
            (rom[at..at + 8].try_into().unwrap(), char::from(code))
        })
        .collect()
}

// The text a CPC shows in mode 1, read from its RAM as a snapshot saved from
// it holds it, each character's matrix found in the font. "The screen memory
// is divided into 8 blocks, each 2K bytes long", "each line of pixels on the
// screen uses 80 consecutive bytes from a block", "the top line of the
// screen comes from block 0, the second line from block 1 and so on", and
// "a block wraps around from its last byte to its first byte"; in mode 1 the
// leftmost of a byte's four pixels is "Bits 3,7" and the rightmost "Bits
// 0,4", "the first bit given is most significant bit of the pixel" (SOFT
// 968, section 6). The 6845's start address, R12 and R13, places the first
// byte: its MA13 and MA12 reach A15 and A14, and MA9 to MA0 reach A10 to A1
// ("Screen memory addressess", Kevin Thacker's cpctech,
// https://cpctech.cpcwiki.de/docs/scraddr.html). A pixel of any pen but the
// paper's, 0, is drawn; a cell no matrix matches reads as '?'.
fn cpc_text(
    session: &mut Session,
    handle: &str,
    scratch: &Roms,
    font: &BTreeMap<[u8; 8], char>,
) -> Vec<String> {
    static TAKEN: AtomicU32 = AtomicU32::new(0);
    let path = scratch.0.join(format!(
        "screen-{}.sna",
        TAKEN.fetch_add(1, Ordering::Relaxed)
    ));
    session.answer("snapshot_save", json!({ "handle": handle, "path": path }));
    let ram = fs::read(&path).expect("the snapshot")[0x100..].to_vec();
    assert_eq!(session.inspect(handle, "gate_array.mode"), 1);
    let registers = session.inspect(handle, "crtc.registers");
    let register = |number: usize| usize::try_from(registers[number].as_u64().unwrap()).unwrap();
    let start = (register(12) << 8 | register(13)) & 0x3FFF;
    let (base, offset) = ((start & 0x3000) << 2, (start & 0x3FF) << 1);
    let drawn = |byte: u8| -> u8 {
        (0..4).fold(0, |pixels, pixel| {
            let pen = (byte >> (3 - pixel) & 1) << 1 | (byte >> (7 - pixel) & 1);
            pixels << 1 | u8::from(pen != 0)
        })
    };
    (0..25)
        .map(|row| {
            (0..40)
                .map(|column| {
                    let matrix: [u8; 8] = std::array::from_fn(|line| {
                        let at = |byte: usize| {
                            ram[base
                                + line * 0x800
                                + ((offset + row * 80 + column * 2 + byte) & 0x7FF)]
                        };
                        drawn(at(0)) << 4 | drawn(at(1))
                    });
                    font.get(&matrix).copied().unwrap_or('?')
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

// PRINT 2+2 and RETURN typed at each CPC's BASIC, and p2+2 and ENTER at a
// Spectrum's, whose K cursor makes the p PRINT, once the command line would
// type them, and ten frames run after: the screen's memory is what the
// command line's typing leaves there, byte for byte, as a snapshot of each
// keeps it, a CPC's 16K from &C000 and a Spectrum's 6912 bytes of pixels and
// attributes from 16384 (16K/48K reference). The rest of the machine is a
// few ticks on from the command line's, whose typing began as its frames
// ran out, where the server's began at the end of the instruction standing
// there. The firmware answers 4, which a CPC prints on the line after the
// sum, read in its own font, before Ready.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn text_typed_leaves_the_screen_the_command_lines_typing_leaves() {
    let scratch = Roms::new(&[]);
    let mut session = Session::reading(&firmware());
    for (machine, frames, text, frame, screen) in [
        (
            "cpc6128",
            78,
            "PRINT 2+2\n",
            CPC_FRAME_TICKS,
            0x100 + 0xC000..0x100 + 0x10000,
        ),
        (
            "cpc664",
            78,
            "PRINT 2+2\n",
            CPC_FRAME_TICKS,
            0x100 + 0xC000..0x100 + 0x10000,
        ),
        (
            "cpc464",
            78,
            "PRINT 2+2\n",
            CPC_FRAME_TICKS,
            0x100 + 0xC000..0x100 + 0x10000,
        ),
        (
            "spectrum48",
            128,
            "p2+2\n",
            SPECTRUM_FRAME_TICKS,
            27..27 + 6912,
        ),
    ] {
        let theirs = scratch.0.join(format!("{machine}-there.sna"));
        let typed = text.replace('\n', "\\n");
        command_line([
            "boot",
            "--machine",
            machine,
            "--frames",
            &frames.to_string(),
            "--type",
            &typed,
            "--wait",
            "10",
            "--save",
            theirs.to_str().unwrap(),
        ]);
        let handle = session.open(machine);
        session.run(&handle, json!({ "ticks": frames * frame }));
        session.answer("type", json!({ "handle": handle, "text": text }));
        session.run(&handle, json!({ "ticks": 10 * frame }));
        let ours = scratch.0.join(format!("{machine}-here.sna"));
        session.answer("snapshot_save", json!({ "handle": handle, "path": ours }));
        let (ours, theirs) = (fs::read(&ours).unwrap(), fs::read(&theirs).unwrap());
        assert!(ours[screen.clone()] == theirs[screen.clone()], "{machine}");
        if machine.starts_with("cpc") {
            let font = cpc_font(&mut session, &handle, machine);
            let text = cpc_text(&mut session, &handle, &scratch, &font);
            let sum = text
                .iter()
                .position(|line| line == "PRINT 2+2")
                .expect("the sum");
            assert_eq!(
                text[sum + 1..sum + 3],
                [" 4", "Ready"],
                "{machine}: {text:#?}"
            );
        }
    }
}

// The Spectrum's answer, at the top left of its picture, in the ROM's own
// font: CHARS, at 23606, is "256 less than address of character set (which
// starts with space and carries on to the copyright symbol)" (ZX Spectrum
// BASIC programming, ch. 25), and PIXEL-ADD, at &22AA, finds the bytes of
// the top line's pixels from y 175 down
// (a_spectrum_rom_routine_called_returns_what_its_disassembly_says).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_typed_at_answers_in_its_own_font() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 128 * SPECTRUM_FRAME_TICKS }));
    session.answer("type", json!({ "handle": handle, "text": "p2+2\n" }));
    session.run(&handle, json!({ "frames": 10 }));
    let chars = bytes_of(&session.peek(&handle, json!(23606), 2));
    let font = u16::from_le_bytes([chars[0], chars[1]]) + 256;
    let four = bytes_of(&session.peek(&handle, json!(font + 8 * u16::from(b'4' - b' ')), 8));
    let shown: Vec<u8> = (0..8)
        .map(|line| {
            session.answer(
                "call",
                json!({
                    "handle": handle,
                    "address": "&22AA",
                    "values": { "cpu.b": 175 - line, "cpu.c": 0 },
                }),
            );
            let cpu = session.inspect(&handle, "cpu");
            let at = cpu["h"].as_u64().unwrap() * 256 + cpu["l"].as_u64().unwrap();
            bytes_of(&session.peek(&handle, json!(at), 1))[0]
        })
        .collect();
    assert_eq!(shown, four);
}

// INKEY(47) asked again and again while keys are held: "the returned value
// of -1" when the key is up, 0 when it is down with SHIFT and CONTROL up,
// and 32 when it is down with SHIFT (CPC6128 User Instructions, ch. 3, INKEY,
// https://archive.org/details/amstrad-cpc-6128-user-manual), 47 being SPACE
// as the firmware numbers it. Each line printed while the last is printing
// is whole once the next has begun.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn keys_held_through_a_run_are_the_keys_basic_reads() {
    let scratch = Roms::new(&[]);
    let mut session = Session::reading(&firmware());
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "ticks": 78 * CPC_FRAME_TICKS }));
    session.answer(
        "type",
        json!({ "handle": handle, "text": "10 PRINT INKEY(47):GOTO 10\nRUN\n" }),
    );
    let font = {
        let reading = session.open("cpc6128");
        session.run(&reading, json!({ "ticks": 78 * CPC_FRAME_TICKS }));
        cpc_font(&mut session, &reading, "cpc6128")
    };
    for (keys, read) in [
        (json!(["SPACE"]), "0"),
        (json!(["SPACE", "SHIFT"]), "32"),
        (json!([]), "-1"),
        (json!([47]), "0"),
    ] {
        session.run(&handle, json!({ "frames": 10, "keys": keys }));
        let text = cpc_text(&mut session, &handle, &scratch, &font);
        let printed: Vec<&String> = text.iter().filter(|line| !line.trim().is_empty()).collect();
        assert_eq!(printed[printed.len() - 2].trim(), read, "{keys}: {text:#?}");
    }
}

// CAPS SHIFT with SPACE "acts as a BREAK" (ZX Spectrum BASIC programming,
// ch. 1), tested "after every statement", and "Report L - BREAK into program"
// is RST &08 followed by &14 (The Complete Spectrum ROM Disassembly,
// https://skoolkid.github.io/rom/asm/1B76.html), which ERR NR, at 23610,
// holds as "1 less than the report code" (ch. 25).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_held_at_break_breaks_into_its_program() {
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 128 * SPECTRUM_FRAME_TICKS }));
    // 10, then GO TO in the K cursor's g, 10 and ENTER; then RUN, its r.
    session.answer("type", json!({ "handle": handle, "text": "10g10\nr\n" }));
    session.run(&handle, json!({ "frames": 10 }));
    assert_eq!(session.peek(&handle, json!(23610), 1), "FF");
    session.run(&handle, json!({ "frames": 10, "keys": ["CAPS", "SPACE"] }));
    assert_eq!(session.peek(&handle, json!(23610), 1), "14");
}

// A blank DATA disc in a drive, a one-line program SAVEd to it and the disc
// catalogued: AMSDOS shows what the CPC6128 User Instructions show, "Drive
// A: user 0", "EXAMPLE.BAS 1K", "177K free" (Foundation Course, ch. 1, p.
// 44), read here without its spaces, whose count the manual's scan does not keep.
// The 6128 reads drive A, and the 664 drive B, as |B makes it: "Set default
// drive to Drive B" (ch. 5), whose heading names B where the manual's names
// A. What was written is the machine's: an image the emulator cannot read is
// refused and the disc stays in the drive, and the file given, put in again,
// is the blank disc it was.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn amsdos_catalogues_what_is_saved_to_a_disc_and_the_file_is_never_written() {
    let scratch = Roms::new(&[]);
    let blank = blank_data_disc();
    let path = scratch.write("blank.dsk", &blank);
    let not_a_disc = scratch.write("not.dsk", &[0; 512]);
    let mut session = Session::reading(&firmware());
    for (machine, drive) in [("cpc6128", "A"), ("cpc664", "B")] {
        let handle = session.open(machine);
        session.run(&handle, json!({ "ticks": 78 * CPC_FRAME_TICKS }));
        let font = cpc_font(&mut session, &handle, machine);
        let given = |path: &Path| json!({ "handle": handle, "path": path, "drive": drive });
        session.answer("disc_insert", given(&path));
        if drive == "B" {
            session.answer("type", json!({ "handle": handle, "text": "|B\n" }));
            session.run(&handle, json!({ "frames": 50 }));
        }
        let catalogue = |session: &mut Session| {
            session.answer("type", json!({ "handle": handle, "text": "CAT\n" }));
            session.run(&handle, json!({ "frames": 250 }));
            let text = cpc_text(session, &handle, &scratch, &font);
            let typed = text.iter().rposition(|line| line == "CAT").expect("CAT");
            let mut shown: Vec<String> = text[typed + 1..]
                .iter()
                .map(|line| line.replace(' ', ""))
                .filter(|line| !line.is_empty())
                .collect();
            // The cursor, which no matrix draws, waits on the line after.
            shown.truncate(
                shown
                    .iter()
                    .position(|line| line == "Ready")
                    .expect("Ready")
                    + 1,
            );
            shown
        };
        session.answer(
            "type",
            json!({ "handle": handle, "text": "10 PRINT 2+2\nSAVE\"EXAMPLE\"\n" }),
        );
        session.run(&handle, json!({ "frames": 250 }));
        let heading = format!("Drive{drive}:user0");
        let saved = [heading.as_str(), "EXAMPLE.BAS1K", "177Kfree", "Ready"];
        assert_eq!(catalogue(&mut session), saved, "{machine}");
        session.refusal("disc_insert", given(&not_a_disc));
        assert_eq!(catalogue(&mut session), saved, "{machine}");
        session.answer("disc_insert", given(&path));
        assert_eq!(
            catalogue(&mut session),
            [heading.as_str(), "178Kfree", "Ready"],
            "{machine}"
        );
        assert_eq!(fs::read(&path).unwrap(), blank);
    }
}

// LOAD "", typed as the K cursor's j and two of SYM SHFT's P, reads a tape
// the test writes. LD-BYTES, at &0556, is called for the header, "LOAD
// seventeen bytes" (https://skoolkid.github.io/rom/asm/0605.html, LD_LOOK_H),
// as ENTER goes down, inside the typing, and once more, for the program,
// in the run after; the program stands where PROG, at 23635, the "Address
// of BASIC program" (ZX Spectrum BASIC programming, ch. 25), points, and has
// run from its autostart line.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn load_reads_a_program_from_a_tape_and_runs_it() {
    let scratch = Roms::new(&[]);
    let program = poking_program();
    let path = scratch.write("program.tap", &program_tape(b"TEST      ", 10, &program));
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 128 * SPECTRUM_FRAME_TICKS }));
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    session.answer("type", json!({ "handle": handle, "text": "j\"\"\n" }));
    let ran = session.run(
        &handle,
        json!({ "frames": 600, "gather": [{ "execute": "&0556" }] }),
    );
    assert_eq!(ran["gathered"].as_array().unwrap().len(), 1, "{ran}");
    let prog = bytes_of(&session.peek(&handle, json!(23635), 2));
    let prog = u16::from_le_bytes([prog[0], prog[1]]);
    assert_eq!(
        bytes_of(&session.peek(&handle, json!(prog), u16::try_from(program.len()).unwrap())),
        program
    );
    assert_eq!(session.peek(&handle, json!(30000), 1), "2A");
}

// A snapshot the server saves is the one the command line saves of the
// same machine, and one the command line runs on from stands where the
// server, opening it, runs on to.
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_snapshot_saved_is_the_command_lines_and_runs_on_there() {
    let scratch = Roms::new(&[]);
    let mut session = Session::reading(&firmware());
    let mut cpc = cpc_snapshot(128, 1);
    cpc[0x23..0x25].copy_from_slice(&0x4000u16.to_le_bytes());
    let program = [&[LD_HL, 0x00, 0x80][..], &COUNTING_LOOP].concat();
    cpc[0x100 + 0x4000..0x100 + 0x4000 + program.len()].copy_from_slice(&program);
    let mut spectrum = spectrum_snapshot();
    spectrum[25] = 1;
    let program = [&[LD_HL, 0x00, 0xC0][..], &COUNTING_LOOP].concat();
    spectrum[27 + 0x4000..27 + 0x4000 + program.len()].copy_from_slice(&program);
    for (machine, snapshot, frame) in [
        ("cpc6128", cpc, CPC_FRAME_TICKS),
        ("spectrum48", spectrum, SPECTRUM_FRAME_TICKS),
    ] {
        let from = scratch.write(&format!("{machine}.sna"), &snapshot);
        let mut previous = from;
        for (round, frames) in [10, 5].into_iter().enumerate() {
            let theirs = scratch.0.join(format!("{machine}-{round}-there.sna"));
            command_line_run(machine, &previous, frames, &theirs);
            let handle = session.open_from(machine, &previous);
            session.run(&handle, json!({ "ticks": u64::from(frames) * frame }));
            let ours = scratch.0.join(format!("{machine}-{round}-here.sna"));
            session.answer("snapshot_save", json!({ "handle": handle, "path": ours }));
            assert_eq!(
                fs::read(&ours).unwrap(),
                fs::read(&theirs).unwrap(),
                "{machine}, round {round}"
            );
            previous = ours;
        }
    }
}

// A CPC kept while AMSDOS saves to its disc, its disc controller inside a
// command, runs on from a restore to the machine it ran on to, and catalogues
// the disc the same; a disc swapped in since, and the writes since, are undone by
// restoring a state kept before them, and the catalogue shows the disc as it
// was then (amsdos_catalogues_what_is_saved_to_a_disc_and_the_file_is_never_written).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_cpc_kept_while_it_writes_its_disc_runs_on_from_a_restore_alike() {
    let scratch = Roms::new(&[]);
    let path = scratch.write("blank.dsk", &blank_data_disc());
    let mut session = Session::reading(&firmware());
    let handle = session.open("cpc6128");
    session.run(&handle, json!({ "ticks": 78 * CPC_FRAME_TICKS }));
    let font = cpc_font(&mut session, &handle, "cpc6128");
    session.answer("disc_insert", json!({ "handle": handle, "path": path }));
    let blank = keep(&mut session, &handle);
    session.answer(
        "type",
        json!({ "handle": handle, "text": "10 PRINT 2+2\nSAVE\"EXAMPLE\"\n" }),
    );
    session.run(&handle, json!({ "frames": 100 }));
    assert_eq!(
        session.inspect(&handle, "fdc.phase"),
        "UPD765_PHASE_EXECUTION"
    );
    let writing = keep(&mut session, &handle);
    let kept = everything(&mut session, &handle);
    session.run(&handle, json!({ "frames": 250 }));
    let saved = everything(&mut session, &handle);
    restore(&mut session, &handle, &writing);
    assert!(everything(&mut session, &handle) == kept);
    session.run(&handle, json!({ "frames": 250 }));
    assert!(everything(&mut session, &handle) == saved);
    let catalogue = |session: &mut Session| {
        session.answer("type", json!({ "handle": handle, "text": "CAT\n" }));
        session.run(&handle, json!({ "frames": 250 }));
        let text = cpc_text(session, &handle, &scratch, &font);
        let typed = text.iter().rposition(|line| line == "CAT").expect("CAT");
        text[typed + 1..]
            .iter()
            .map(|line| line.replace(' ', ""))
            .find(|line| line.ends_with("free"))
            .expect("a catalogue")
    };
    assert_eq!(catalogue(&mut session), "177Kfree");
    let saved = keep(&mut session, &handle);
    session.answer("disc_insert", json!({ "handle": handle, "path": path }));
    assert_eq!(catalogue(&mut session), "178Kfree");
    restore(&mut session, &handle, &saved);
    assert_eq!(catalogue(&mut session), "177Kfree");
    restore(&mut session, &handle, &blank);
    assert_eq!(catalogue(&mut session), "178Kfree");
}

// A Spectrum kept partway through LOAD "", inside the ROM's LD-BYTES, from
// &0556 to &0604 (The Complete Spectrum ROM Disassembly), its tape playing,
// runs on from a restore to the machine it ran on to, the program loaded and
// run (load_reads_a_program_from_a_tape_and_runs_it).
#[test]
#[ignore = "needs the emulator's fetched firmware and its command line: make test-firmware"]
fn a_spectrum_kept_while_it_loads_runs_on_from_a_restore_alike() {
    let scratch = Roms::new(&[]);
    let path = scratch.write(
        "program.tap",
        &program_tape(b"TEST      ", 10, &poking_program()),
    );
    let mut session = Session::reading(&firmware());
    let handle = session.open("spectrum48");
    session.run(&handle, json!({ "ticks": 128 * SPECTRUM_FRAME_TICKS }));
    session.answer("tape_insert", json!({ "handle": handle, "path": path }));
    session.answer("type", json!({ "handle": handle, "text": "j\"\"\n" }));
    let ran = session.run(&handle, json!({ "frames": 200 }));
    assert!(
        (0x0556..0x0605).contains(&after_ampersand(&ran["pc"], 4)),
        "{ran}"
    );
    let loading = keep(&mut session, &handle);
    let kept = everything(&mut session, &handle);
    session.run(&handle, json!({ "frames": 400 }));
    let loaded = everything(&mut session, &handle);
    assert_eq!(session.peek(&handle, json!(30000), 1), "2A");
    restore(&mut session, &handle, &loading);
    assert!(everything(&mut session, &handle) == kept);
    session.run(&handle, json!({ "frames": 400 }));
    assert!(everything(&mut session, &handle) == loaded);
}
