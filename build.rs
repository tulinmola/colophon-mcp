use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
    bindgen::Builder::default()
        .header(bridge.join("mcp.h").display().to_string())
        .clang_arg(format!("-I{}", machine.display()))
        .clang_arg("-std=c99")
        .rust_edition(bindgen::RustEdition::Edition2024)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // A copy of a machine would share its buffers, and a CPC's controller,
        // copied alone or with its CPC, would point at the drives of the one
        // it was copied from.
        .no_copy("(cpc_t|upd765_t|spectrum_t)")
        .allowlist_function("(cpc|spectrum|mcp)_.*")
        .allowlist_type("(cpc|spectrum|mcp)_.*")
        .allowlist_var("(CPC|SPECTRUM)_.*")
        .generate()
        .expect("bindgen reads the bridge's header")
        .write_to_file(out.join("emulator.rs"))
        .expect("the bindings are written");
}
