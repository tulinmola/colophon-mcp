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
const TOOLS: [&str; 10] = [
    "call",
    "inspect",
    "machine_close",
    "machine_list",
    "machine_open",
    "peek",
    "poke",
    "run",
    "set",
    "step",
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
        let mut child = server()
            .arg("--roms")
            .arg(roms)
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

    // "If an output schema is provided: Servers MUST provide structured
    // results that conform to this schema." (2026-07-28, tools)
    fn answer(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
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
const DI: u8 = 0xF3;
const EI: u8 = 0xFB;
const EX_SP_IX: [u8; 2] = [0xDD, 0xE3];
const HALT: u8 = 0x76;
const IM_1: [u8; 2] = [0xED, 0x56];
const IN_A_N: u8 = 0xDB;
const INC_A: u8 = 0x3C;
const DEC_BC: u8 = 0x0B;
const INC_BC: u8 = 0x03;
const INC_HL: u8 = 0x23;
const INC_AT_HL: u8 = 0x34;
const INC_L: u8 = 0x2C;
const JR: u8 = 0x18;
const JR_NZ: u8 = 0x20;
const LD_A: u8 = 0x3E;
const LD_A_AT_HL: u8 = 0x7E;
const LD_A_B: u8 = 0x78;
const LD_A_FROM: u8 = 0x3A;
const LD_AT_HL: u8 = 0x36;
const LD_AT_HL_A: u8 = 0x77;
const LD_BC: u8 = 0x01;
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
// destructive update, and a machine stood up an additive one.
#[test]
fn every_tool_says_what_it_does_to_the_machines_and_that_nothing_else_is_reached() {
    let roms = Roms::all();
    let tools = tools(&mut Session::start(&roms));
    for name in TOOLS {
        let annotations = &listed(&tools, name)["annotations"];
        assert_eq!(annotations["openWorldHint"], false, "{name}: {annotations}");
        let reads = ["inspect", "machine_list", "peek"].contains(&name);
        assert_eq!(annotations["readOnlyHint"], reads, "{name}: {annotations}");
        if !reads {
            let destroys = name != "machine_open";
            // A handle put away names nothing again, so putting it away
            // again changes nothing.
            let repeats = ["set", "poke", "machine_close"].contains(&name);
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
    // The monitor locks on over the first two.
    let first = session.run(&handle, json!({ "frames": 2 }));
    assert_eq!(first["frames"], 2, "{first}");
    assert_eq!(first["settled"], true, "{first}");
    assert_eq!(first["stopped"], "frames", "{first}");
    assert_eq!(first["tick"], first["ticks"], "{first}");
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
        Some(first["ticks"].as_u64().unwrap() + SPECTRUM_FRAME_TICKS + ticks)
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
        (&json!(10), &json!(3), &json!("instructions")),
        "{one}"
    );
    let two = session.step(&handle, 2);
    assert_eq!(
        (&two["ticks"], &two["pc"]),
        (&json!(23 + 4), &json!(6)),
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
    let written = session.set(&handle, json!({ "cpu.pc": 3, "cpu.b": 7 }));
    assert_eq!(written, json!({ "values": { "cpu.pc": 3, "cpu.b": 7 } }));
    let step = session.step(&handle, 1);
    assert_eq!(
        (&step["ticks"], &step["pc"]),
        (&json!(23), &json!(5)),
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
    session.run(&run, json!({ "ticks": 23456 }));
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
        "cpu.pc: expects an integer from 0 to 65535, not 65536"
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
        (&json!(0), &step["ticks"]),
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
            let address = seen[kind].as_u64().expect("an address");
            (kind, address, seen["value"].as_u64(), seen["pc"].as_u64())
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
        assert_eq!(ran["stopped"], "ticks", "{ran}");
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
            (&json!("matched"), &json!(0x000D)),
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
            (&json!("matched"), &json!(4)),
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
    assert_eq!(ran["pc"], 8, "{ran}");
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
    assert_eq!(&session.inspect(&handle, "cpu.a"), read, "{ran}");
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
            seen(&ran["gathered"]),
            [("execute", 0, None, None), ("execute", 1, None, None)],
            "{machine}: {ran}"
        );
        let until = json!([{ "execute": 0, "to": "&FFFF" }]);
        let idled = session.run(&handle, json!({ "ticks": 1000, "until": until }));
        assert_eq!(idled["stopped"], "ticks", "{machine}: {idled}");
    }
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
        assert_eq!(
            (&called["stopped"], &called["pc"]),
            (&json!("returned"), &before["pc"]),
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
        assert_eq!(stepped["pc"], 6, "{machine}: {stepped}");
    }
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
        (&json!("returned"), &json!(512)),
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
        (&json!("returned"), &json!(6)),
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
    let pc = called["pc"].as_u64().unwrap();
    assert!(
        (0x0100..0x010C).contains(&pc) || (0x0038..0x0041).contains(&pc),
        "{called}"
    );
    let until = json!([{ "execute": 6 }]);
    let ran = session.run(&handle, json!({ "frames": 10, "until": until }));
    assert_eq!(
        (&ran["stopped"], &ran["pc"]),
        (&json!("matched"), &json!(6)),
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

// The emulator's command line runs a snapshot by its frames, which are its
// standard frames' ticks, and saves the machine as it then stands.
fn command_line_run(machine: &str, snapshot: &Path, frames: u32, saved: &Path) {
    let command_line = emulator().join("build/emulator");
    assert!(
        command_line.exists(),
        "no command line at {}: run `make` in the emulator",
        command_line.display()
    );
    let output = Command::new(command_line)
        .arg("run")
        .arg(snapshot)
        .args(["--machine", machine, "--frames", &frames.to_string()])
        .arg("--roms")
        .arg(firmware())
        .arg("--save")
        .arg(saved)
        .output()
        .expect("the command line runs");
    assert!(output.status.success(), "{output:?}");
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
        (&json!("matched"), &json!(0x0041)),
        "{stopped}"
    );
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
    assert_eq!(
        (&called["stopped"], &called["pc"]),
        (&json!("returned"), &before["pc"]),
        "{called}"
    );
    let cpu = session.inspect(&handle, "cpu");
    assert_eq!(
        (&cpu["h"], &cpu["l"], &cpu["a"]),
        (&json!(0x40), &json!(0x01), &json!(5))
    );
    assert_eq!(cpu["sp"], before["sp"]);
}
