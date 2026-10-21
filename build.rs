use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use bindgen::callbacks::{DeriveInfo, ParseCallbacks};

#[path = "src/build_name.rs"]
mod build_name;

// What a build is compiled from, in each repository, and so what marks it.
const SOURCES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "build.rs",
    "emulator",
    "rust-toolchain.toml",
    "src",
];
const EMULATOR_SOURCES: &[&str] = &["src"];

// The machines inspect reads and set writes, and everything they hold by
// value.
const MACHINES: &[&str] = &["cpc_t", "spectrum_t"];

// The types src/fields.rs encodes as a single value, by bindgen's names for
// them.
const SCALARS: &[&str] = &[
    "bool", "u8", "u16", "u32", "u64", "i8", "i32", "c_char", "c_int", "c_uint",
];

fn c_files_in(directory: &Path) -> Vec<PathBuf> {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "c"))
        // A name that begins with a dot is a file of the editor's or the
        // operating system's, which may end in .c without being C.
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| !name.to_string_lossy().starts_with('.'))
        })
        .collect();
    files.sort();
    files
}

// What a run did is answered as the bridge's header declares it.
#[derive(Debug)]
struct Answered;

impl ParseCallbacks for Answered {
    // A comment's lines are the header's, and a description is one sentence
    // run on.
    fn process_comment(&self, comment: &str) -> Option<String> {
        Some(comment.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn add_derives(&self, info: &DeriveInfo<'_>) -> Vec<String> {
        if info.name == "mcp_run_t" {
            vec!["serde::Serialize".into(), "schemars::JsonSchema".into()]
        } else {
            Vec::new()
        }
    }
}

enum Kind {
    Scalar,
    Enumeration(String),
    Structure(String),
    Array(Box<Kind>),
    Pointer,
}

struct Bindings {
    structures: BTreeMap<String, Vec<(String, syn::Type)>>,
    aliases: BTreeMap<String, syn::Type>,
    // Each C enum, as bindgen renders it: an alias of an integer, and a
    // constant for each enumerator, named for the enum and then for it.
    enumerators: BTreeMap<String, Vec<String>>,
}

fn last_name(path: &syn::TypePath) -> String {
    path.path
        .segments
        .last()
        .expect("a path names something")
        .ident
        .to_string()
}

impl Bindings {
    fn read(source: &str) -> Bindings {
        let file = syn::parse_file(source).expect("bindgen writes Rust");
        let mut bindings = Bindings {
            structures: BTreeMap::new(),
            aliases: BTreeMap::new(),
            enumerators: BTreeMap::new(),
        };
        let mut constants = Vec::new();
        for item in file.items {
            match item {
                syn::Item::Struct(structure) => {
                    let fields = structure
                        .fields
                        .iter()
                        .map(|field| {
                            let name = field.ident.as_ref().expect("a named field");
                            (name.to_string(), field.ty.clone())
                        })
                        .collect();
                    bindings
                        .structures
                        .insert(structure.ident.to_string(), fields);
                }
                syn::Item::Type(alias) => {
                    bindings.aliases.insert(alias.ident.to_string(), *alias.ty);
                }
                syn::Item::Const(constant) => {
                    if let syn::Type::Path(path) = &*constant.ty {
                        constants.push((last_name(path), constant.ident.to_string()));
                    }
                }
                _ => {}
            }
        }
        for (alias, constant) in constants {
            if bindings.aliases.contains_key(&alias) && constant.starts_with(&format!("{alias}_")) {
                bindings
                    .enumerators
                    .entry(alias)
                    .or_default()
                    .push(constant);
            }
        }
        bindings
    }

    fn kind(&self, field: &str, ty: &syn::Type) -> Kind {
        match ty {
            syn::Type::Ptr(_) => Kind::Pointer,
            syn::Type::Array(array) => match self.kind(field, &array.elem) {
                Kind::Pointer => Kind::Pointer,
                Kind::Enumeration(_) => {
                    panic!("{field} is an array of a C enum, which no machine has yet")
                }
                element => Kind::Array(Box::new(element)),
            },
            syn::Type::Path(path) => {
                let name = last_name(path);
                if self.enumerators.contains_key(&name) {
                    Kind::Enumeration(name)
                } else if self.structures.contains_key(&name) {
                    Kind::Structure(name)
                } else if let Some(aliased) = self.aliases.get(&name) {
                    self.kind(field, aliased)
                } else if name == "Option" {
                    // A function, as bindgen renders a pointer to one.
                    Kind::Pointer
                } else if SCALARS.contains(&name.as_str()) {
                    Kind::Scalar
                } else {
                    panic!("{field} is a {name}, which src/fields.rs has no encoding for yet")
                }
            }
            _ => panic!("{field} is of a kind src/fields.rs has no encoding for yet"),
        }
    }
}

fn enumerators_constant(enumeration: &str) -> String {
    format!("{}_ENUMERATORS", enumeration.to_uppercase())
}

// The calls that encode a field, get it and set it.
fn calls(kind: &Kind, field: &str) -> Option<[String; 3]> {
    match kind {
        Kind::Pointer => None,
        Kind::Enumeration(enumeration) => {
            let table = enumerators_constant(enumeration);
            Some([
                format!("enumerator(self.{field}, {table})"),
                format!("get_enumerator(self.{field}, {table}, rest)"),
                format!("set_enumerator(&mut self.{field}, {table}, rest, value)"),
            ])
        }
        Kind::Scalar | Kind::Structure(_) | Kind::Array(..) => Some([
            format!("self.{field}.encode()"),
            format!("self.{field}.get(rest)"),
            format!("self.{field}.set(rest, value)"),
        ]),
    }
}

fn generate(bindings: &Bindings) -> String {
    let mut code = String::new();
    let mut enumerations = BTreeSet::new();
    let mut pointers = Vec::new();
    let mut pending: Vec<String> = MACHINES.iter().map(|name| name.to_string()).collect();
    let mut generated = Vec::new();
    while let Some(structure) = pending.pop() {
        if generated.contains(&structure) {
            continue;
        }
        let fields = &bindings.structures[&structure];
        let (mut encode, mut get, mut set, mut names) =
            (String::new(), String::new(), String::new(), Vec::new());
        for (field, ty) in fields {
            assert!(
                !field.starts_with('_'),
                "{structure}.{field} begins as bindgen's own fields do, which C does not have"
            );
            let kind = bindings.kind(&format!("{structure}.{field}"), ty);
            let mut element = &kind;
            while let Kind::Array(inner) = element {
                element = inner;
            }
            match element {
                Kind::Structure(inner) => pending.push(inner.clone()),
                Kind::Enumeration(enumeration) => {
                    enumerations.insert(enumeration.clone());
                }
                _ => {}
            }
            let Some([encoding, getting, setting]) = calls(&kind, field) else {
                pointers.push(format!("\"{structure}.{field}\""));
                writeln!(get, "\"{field}\" => Err(pointer()),").expect("a String takes writes");
                writeln!(set, "\"{field}\" => Err(pointer()),").expect("a String takes writes");
                continue;
            };
            names.push(format!("\"{field}\""));
            writeln!(encode, "fields.insert(\"{field}\".to_owned(), {encoding});")
                .expect("a String takes writes");
            writeln!(get, "\"{field}\" => {getting},").expect("a String takes writes");
            writeln!(set, "\"{field}\" => {setting},").expect("a String takes writes");
        }
        let names = names.join(", ");
        writeln!(
            code,
            "impl Fields for {structure} {{
    fn encode(&self) -> Value {{
        let mut fields = Map::new();
        {encode}
        Value::Object(fields)
    }}

    fn get(&self, path: &[Segment]) -> Result<Value, String> {{
        match path.split_first() {{
            None => Ok(self.encode()),
            Some((Segment::Field(name), rest)) => match name.as_str() {{
                {get}
                _ => Err(no_field(\"{structure}\", name, &[{names}])),
            }},
            Some((Segment::Index(index), _)) => Err(not_an_array(\"{structure}\", *index, &[{names}])),
        }}
    }}

    fn set(&mut self, path: &[Segment], value: &Value) -> Result<(), String> {{
        match path.split_first() {{
            None => set_fields(self, \"{structure}\", value),
            Some((Segment::Field(name), rest)) => match name.as_str() {{
                {set}
                _ => Err(no_field(\"{structure}\", name, &[{names}])),
            }},
            Some((Segment::Index(index), _)) => Err(not_an_array(\"{structure}\", *index, &[{names}])),
        }}
    }}
}}"
        )
        .expect("a String takes writes");
        generated.push(structure);
    }
    let mut enumerators = Vec::new();
    for enumeration in enumerations {
        for constant in &bindings.enumerators[&enumeration] {
            enumerators.push(format!("\"{}\"", &constant[enumeration.len() + 1..]));
        }
        let entries: Vec<String> = bindings.enumerators[&enumeration]
            .iter()
            .map(|constant| {
                let name = &constant[enumeration.len() + 1..];
                format!("(\"{name}\", {constant})")
            })
            .collect();
        writeln!(
            code,
            "const {}: &[(&str, {enumeration})] = &[{}];",
            enumerators_constant(&enumeration),
            entries.join(", ")
        )
        .expect("a String takes writes");
    }
    pointers.sort();
    writeln!(
        code,
        "#[cfg(test)]\nconst POINTERS: &[&str] = &[{}];",
        pointers.join(", ")
    )
    .expect("a String takes writes");
    writeln!(
        code,
        "#[cfg(test)]\npub(crate) const ENUMERATORS: &[&str] = &[{}];",
        enumerators.join(", ")
    )
    .expect("a String takes writes");
    code
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo names the package"));
    let emulator = env::var_os("EMULATOR_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../colophon-emulator"));
    let machine = emulator.join("src");
    let bridge = manifest.join("emulator");
    if !machine.join("cpc.h").exists() {
        panic!(
            "no emulator at {} — set EMULATOR_DIR to your checkout",
            emulator.display()
        );
    }

    println!("cargo::rerun-if-env-changed=EMULATOR_DIR");
    for source in SOURCES {
        println!("cargo::rerun-if-changed={source}");
    }
    for source in EMULATOR_SOURCES {
        println!(
            "cargo::rerun-if-changed={}",
            emulator.join(source).display()
        );
    }
    for repository in [&manifest, &emulator] {
        for path in build_name::watched(repository) {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
    let name = build_name::name(
        &env::var("CARGO_PKG_VERSION").expect("cargo names the version"),
        build_name::commit(&manifest, SOURCES).as_ref(),
        build_name::commit(&emulator, EMULATOR_SOURCES).as_ref(),
    );
    println!("cargo::rustc-env=COLOPHON_MCP_BUILD={name}");
    println!(
        "cargo::rustc-env=COLOPHON_MCP_EMULATOR_DIR={}",
        emulator.display()
    );

    cc::Build::new()
        .std("c99")
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .opt_level(2)
        .include(&machine)
        .files(c_files_in(&machine))
        .files(c_files_in(&bridge))
        .compile("colophon_emulator");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo names the output directory"));
    let bindings = bindgen::Builder::default()
        .header(bridge.join("mcp.h").display().to_string())
        .header(machine.join("cpc_snapshot.h").display().to_string())
        .header(machine.join("spectrum_snapshot.h").display().to_string())
        .clang_arg(format!("-I{}", machine.display()))
        .clang_arg("-std=c99")
        // A field's comment is its description, wherever an answer carries
        // the field.
        .clang_arg("-fparse-all-comments")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(Answered))
        // A copy of a machine would share its buffers, and a CPC's controller,
        // copied alone or with its CPC, would point at the drives of the one
        // it was copied from.
        .no_copy("(cpc_t|upd765_t|spectrum_t)")
        .allowlist_function("(cpc|spectrum|mcp|floppy)_.*|gate_array_rgb|ula_rgb")
        .allowlist_type("(cpc|spectrum|mcp)_.*")
        .allowlist_var("(CPC|SPECTRUM|Z80|MCP)_.*")
        .generate()
        .expect("bindgen reads the bridge's header")
        .to_string();
    fs::write(out.join("emulator.rs"), &bindings).expect("the bindings are written");
    fs::write(out.join("fields.rs"), generate(&Bindings::read(&bindings)))
        .expect("the fields are written");
}
