// The disassembly tier: the listing held to two disassemblies no test here
// wrote, The Complete Spectrum ROM Disassembly and SkoolKit's disassembler,
// and its names for a CPC's firmware entries to SOFT 968, which `make
// test-disassembly` fetches and checks by their hashes.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use colophon_mcp::disassembly::instruction;
use colophon_mcp::firmware::{self, VERSIONS, Version};

fn evidence() -> PathBuf {
    PathBuf::from(
        std::env::var_os("COLOPHON_MCP_DISASSEMBLY_EVIDENCE")
            .expect("the evidence `make test-disassembly` fetches, named by where it put it"),
    )
}

fn listed(memory: &[u8], address: u16) -> (usize, String) {
    let read = |address: u16| memory.get(usize::from(address)).copied().unwrap_or(0);
    let listed = instruction(&read, address, 4096).expect("an instruction");
    (listed.length, listed.text)
}

// Where SkoolKit writes a hexadecimal number after $ this listing writes it
// after &, and it writes the halves of IX and IY in capitals, as
// colophon-player's disassembly names them.
fn as_listed(skoolkit: &str) -> String {
    skoolkit.replace('$', "&").to_uppercase()
}

// A skool file may write a byte as the character it stands for, "A" or "\"",
// or in binary after %, and this listing writes every byte in hexadecimal.
fn in_hexadecimal(text: &str) -> String {
    let mut written = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(['"', '%']) {
        written.push_str(&rest[..at]);
        let operand = &rest[at..];
        let (value, length) = if let Some(binary) = operand.strip_prefix('%') {
            let digits = binary
                .chars()
                .take_while(|digit| "01".contains(*digit))
                .count();
            let value = u8::from_str_radix(&binary[..digits], 2).expect("a binary byte");
            (value, 1 + digits)
        } else if operand.starts_with("\"\\") {
            (operand.as_bytes()[2], 4)
        } else {
            (operand.as_bytes()[1], 3)
        };
        written.push_str(&format!("${value:02X}"));
        rest = &operand[length..];
    }
    written + rest
}

// What comes before a line's comment, which begins at the first ; outside a
// character.
fn before_the_comment(text: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (at, character) in text.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return &text[..at],
            _ => {}
        }
    }
    text
}

// A skool file's instruction line begins with the kind of the block it
// opens, or with a space or a * for one within the block, then its address;
// c is code ("Skool files", SkoolKit's reference,
// https://skoolkit.ca/docs/skoolkit/skool-files.html). Every code line that
// is an instruction, rather than a DEFB or its like among the code, reads as
// the disassembly writes it, and is as long as the distance to the next line.
#[test]
#[ignore = "needs SkoolKit and The Complete Spectrum ROM Disassembly: make test-disassembly"]
fn every_instruction_of_the_48k_rom_reads_as_its_disassembly_writes_it() {
    let roms = PathBuf::from(env!("COLOPHON_MCP_EMULATOR_DIR")).join("roms");
    let rom = fs::read(roms.join("spectrum48.rom")).expect("the 48K's firmware");
    let skool = fs::read_to_string(evidence().join("rom.skool")).expect("the disassembly");
    let mut code = false;
    let mut lines: Vec<(bool, u16, String)> = Vec::new();
    for line in skool.lines() {
        let Some(kind) = line.chars().next() else {
            continue;
        };
        let Some(rest) = line.get(1..).and_then(|rest| rest.strip_prefix('$')) else {
            continue;
        };
        let Some(address) = rest
            .get(..4)
            .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        else {
            continue;
        };
        match kind {
            ' ' | '*' => {}
            'b' | 'c' | 'g' | 'i' | 's' | 't' | 'u' | 'w' => code = kind == 'c',
            _ => continue,
        }
        let text = before_the_comment(&rest[4..]).trim();
        let instruction = code && !text.starts_with("DEF");
        let text = if instruction {
            in_hexadecimal(text)
        } else {
            text.to_owned()
        };
        lines.push((instruction, address, text));
    }
    let mut checked = 0;
    let mut differences = Vec::new();
    for (index, (instruction, address, text)) in lines.iter().enumerate() {
        if !instruction {
            continue;
        }
        let (length, listed) = listed(&rom, *address);
        let next = lines
            .get(index + 1)
            .map_or(0x4000, |(_, next, _)| usize::from(*next));
        if listed != as_listed(text) || usize::from(*address) + length != next {
            differences.push(format!(
                "&{address:04X}: {text} as {listed}, {length} bytes"
            ));
        }
        checked += 1;
    }
    assert!(
        differences.is_empty(),
        "{} of {checked}:\n{}",
        differences.len(),
        differences.join("\n")
    );
    assert_eq!(checked, 6775, "the instructions of the disassembly pinned");
}

// SkoolKit's disassembler, given each form at its address in a 64K of zeros,
// with every undocumented instruction it knows turned on ("opcodes", its
// reference's Disassembler component), answering its length and what it says.
const SKOOLKIT: &str = r#"
import sys
from types import SimpleNamespace
sys.path.insert(0, sys.argv[1])
from skoolkit.disassembler import Disassembler

config = SimpleNamespace(
    asm_hex=True, asm_lower=False, defb_size=8, defm_size=66, defw_size=1, handle_rst=False,
    imaker=lambda address, operation, data: SimpleNamespace(operation=operation, data=data),
    opcodes='ALL', wrap=False)
for line in sys.stdin:
    address, form = line.split()
    address, form = int(address, 16), bytes.fromhex(form)
    memory = [0] * 65536
    memory[address:address + len(form)] = form
    instruction = Disassembler(memory, config).disassemble(address, address + 1, 'n')[0]
    print(f'{len(instruction.data)}\t{instruction.operation}')
"#;

const PREFIXES: [u8; 4] = [0xCB, 0xDD, 0xED, 0xFD];

// Every opcode alone and after each prefix, with its displacement where it
// takes one, and the bytes after it read as operands, once small and once
// large enough to be a negative displacement.
fn forms() -> Vec<Vec<u8>> {
    let mut forms = Vec::new();
    for operands in [[0x05, 0x34, 0x12], [0xFB, 0xCD, 0xAB]] {
        for opcode in 0..=u8::MAX {
            let mut prefixed = |prefix: &[u8]| {
                forms.push([prefix, &[opcode], &operands].concat());
            };
            if !PREFIXES.contains(&opcode) {
                prefixed(&[]);
                prefixed(&[0xDD]);
                prefixed(&[0xFD]);
            }
            prefixed(&[0xCB]);
            prefixed(&[0xED]);
            prefixed(&[0xDD, 0xCB, operands[0]]);
            prefixed(&[0xFD, 0xCB, operands[0]]);
        }
    }
    forms
}

fn skoolkit(forms: &[(u16, &[u8])]) -> Vec<(usize, String)> {
    let mut python = Command::new("python3")
        .args(["-I", "-c", SKOOLKIT])
        .arg(evidence().join("skoolkit"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    let mut input = python.stdin.take().expect("its input");
    let lines: String = forms
        .iter()
        .map(|(address, form)| {
            let form: String = form.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("{address:04x} {form}\n")
        })
        .collect();
    let writing = std::thread::spawn(move || input.write_all(lines.as_bytes()));
    let output = python.wait_with_output().expect("SkoolKit answers");
    writing
        .join()
        .expect("the forms are written")
        .expect("SkoolKit reads them");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .expect("text")
        .lines()
        .map(|line| {
            let (length, operation) = line.split_once('\t').expect("a length and an operation");
            (length.parse().expect("a length"), operation.to_owned())
        })
        .collect()
}

// Every form reads as SkoolKit reads it, save where colophon-player's
// disassembly names what the documentation does not: IN (C) where SkoolKit
// writes IN F,(C), IM 0/1 for ED 4E and ED 6E, which it reads as IM 0, NOP for
// ED followed by a byte no instruction begins with, and BRK for ED FF, which
// SkoolKit leaves as bytes, after WinAPE (colophon-player's docs/debugger/
// disassembly.en.md, "The instruction with no datasheet"). And an index prefix
// before an instruction that has no HL to replace, which SkoolKit leaves as a
// byte of its own, is listed with the instruction, as the one step runs it:
// as SkoolKit reads that instruction where it stands, one byte on, and one
// byte longer.
#[test]
#[ignore = "needs SkoolKit and The Complete Spectrum ROM Disassembly: make test-disassembly"]
fn every_opcode_form_reads_as_skoolkit_disassembles_it() {
    let forms = forms();
    let at_8000: Vec<(u16, &[u8])> = forms.iter().map(|form| (0x8000, form.as_slice())).collect();
    let after_a_prefix: Vec<(u16, &[u8])> = forms.iter().map(|form| (0x8001, &form[1..])).collect();
    let read = skoolkit(&at_8000);
    let read_after_a_prefix = skoolkit(&after_a_prefix);
    let mut differences = Vec::new();
    for (index, (form, (length, operation))) in forms.iter().zip(&read).enumerate() {
        let mut memory = vec![0; 0x10000];
        memory[0x8000..0x8000 + form.len()].copy_from_slice(form);
        let expected = match (form.as_slice(), operation.as_str()) {
            (&[0xDD | 0xFD, ..], "DEFB $DD" | "DEFB $FD") => {
                let (length, operation) = &read_after_a_prefix[index];
                (length + 1, as_listed(operation))
            }
            (&[0xED, 0xFF, ..], _) => (2, "BRK".to_owned()),
            (_, defb) if defb.starts_with("DEFB $ED,") => (2, "NOP".to_owned()),
            (_, "IN F,(C)") => (2, "IN (C)".to_owned()),
            (&[0xED, 0x4E | 0x6E, ..], "IM 0") => (2, "IM 0/1".to_owned()),
            _ => (*length, as_listed(operation)),
        };
        let listed = listed(&memory, 0x8000);
        if listed != expected {
            differences.push(format!("{form:02X?}: {operation} as {listed:?}"));
        }
    }
    assert!(
        differences.is_empty(),
        "{} of {}:\n{}",
        differences.len(),
        forms.len(),
        differences.join("\n")
    );
}

// SOFT 968, The Amstrad 6128 Firmware Manual, as the text archive.org read
// from its scan, each run of spaces and line breaks made one space. The
// reading puts spaces where the print has none and leaves them out where it
// has them, so an entry is found by its letters with the spaces taken out.
fn soft_968() -> String {
    let text = fs::read_to_string(evidence().join("soft968.txt")).expect("SOFT 968");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// "Entry address = Start of jumpblock + 3 * Entry number" (SOFT 968, 15).
fn number(address: u16) -> u16 {
    let first = [0xBDCD, 0xBB00, 0xB900]
        .into_iter()
        .find(|first| address >= *first)
        .expect("a jumpblock");
    (address - first) / 3
}

// A name in section 14's lists ends where the sentence saying what its entry
// does begins: at a capital before a small letter, or at what is neither a
// letter nor a digit, such as the quotation mark KL EVENT's begins with.
fn ends_a_name(rest: &str) -> bool {
    let mut characters = rest.chars();
    match characters.next() {
        Some(first) if first.is_ascii_uppercase() => {
            characters.next().is_some_and(char::is_lowercase)
        }
        Some(first) => !first.is_alphanumeric(),
        None => true,
    }
}

// Every name a listing gives a CPC's firmware entry stands after the entry's
// number and address in section 14's lists, and after the * they put on an
// entry AMSDOS takes over. Where the reading of the scan garbled that line,
// as it did two of the main jumpblock's, the name heads the entry's
// description in section 15 instead.
#[test]
#[ignore = "needs SOFT 968: make test-disassembly"]
fn every_firmware_entry_is_named_as_soft_968_names_it() {
    let text = soft_968().replace(' ', "");
    let entries: BTreeSet<(u16, &str)> = VERSIONS.into_iter().flat_map(firmware::entries).collect();
    let mut missing = Vec::new();
    for &(address, name) in &entries {
        let (number, name) = (number(address), name.replace(' ', ""));
        let listing = format!("{number}#{address:04X}");
        let named = if text.contains(&listing) {
            [format!("{listing}{name}"), format!("{listing}*{name}")]
                .iter()
                .any(|listed| {
                    text.match_indices(listed.as_str())
                        .any(|(at, _)| ends_a_name(&text[at + listed.len()..]))
                })
        } else {
            [
                format!("{number}:{name}#{address:04X}"),
                format!("{number}{name}#{address:04X}"),
            ]
            .iter()
            .any(|head| text.contains(head.as_str()))
        };
        if !named {
            missing.push(format!("{number} &{address:04X} {name}"));
        }
    }
    assert!(missing.is_empty(), "not in SOFT 968: {missing:?}");
    assert_eq!(entries.len(), 229, "the entries of the three jumpblocks");
}

// How many times a name stands in the text as a whole, with no letter or
// digit before or after it.
fn times(text: &str, name: &str) -> usize {
    text.match_indices(name)
        .filter(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
        .count()
}

// Every name is spaced as SOFT 968 spaces it most: the reading of the scan
// drops a space here and adds one there, but never as often as the print has
// it, so each name stands in the text more often than any spelling a space
// away from it, one more or one fewer.
#[test]
#[ignore = "needs SOFT 968: make test-disassembly"]
fn every_firmware_entry_is_spaced_as_soft_968_spaces_it_most() {
    let text = soft_968();
    let names: BTreeSet<&str> = VERSIONS
        .into_iter()
        .flat_map(firmware::entries)
        .map(|(_, name)| name)
        .collect();
    let mut misspelled = Vec::new();
    for name in names {
        let characters: Vec<char> = name.chars().collect();
        let neighbours = (1..characters.len()).filter_map(|at| {
            let (before, after) = (characters[at - 1], characters[at]);
            let (head, tail) = name.split_at(at);
            match (before, after) {
                (_, ' ') => Some(format!("{head}{}", &tail[1..])),
                (' ', _) => None,
                _ => Some(format!("{head} {tail}")),
            }
        });
        let most = neighbours.map(|neighbour| times(&text, &neighbour)).max();
        if times(&text, name) <= most.unwrap_or(0) {
            misspelled.push(name);
        }
    }
    assert!(
        misspelled.is_empty(),
        "not as SOFT 968 spaces them: {misspelled:?}"
    );
}

// Section 15 says of a main jumpblock entry V1.0 lacks "This routine is not
// available on V1.0 firmware", and of one V1.0 and V1.1 lack "This routine
// is only available on the CPC6128 (ie. V1.2 firmware)"; an entry it says
// neither of is in every version. A note is taken to be the entry's whose
// heading comes last before it, so a heading the scan's reading garbled
// hands its notes to the entry before, and the versions disagree with SOFT
// 968 rather than agree by chance.
#[test]
#[ignore = "needs SOFT 968: make test-disassembly"]
fn each_version_has_the_main_jumpblock_entries_soft_968_says_it_has() {
    let text = soft_968().replace(' ', "");
    let start = text
        .rfind("15TheMainFirmwareJumpblock.")
        .expect("section 15");
    let end = text
        .rfind("16TheFirmwareIndirections.")
        .expect("section 16");
    let section = &text[start..end];
    let main: Vec<(u16, &str)> = firmware::entries(Version::V1_2)
        .filter(|(address, _)| (0xBB00..0xBDCD).contains(address))
        .collect();
    let headings: Vec<(usize, u16)> = main
        .iter()
        .filter_map(|&(address, name)| {
            let (number, name) = (number(address), name.replace(' ', ""));
            [
                format!("{number}:{name}#{address:04X}"),
                format!("{number}{name}#{address:04X}"),
            ]
            .iter()
            .find_map(|head| section.find(head.as_str()))
            .map(|at| (at, number))
        })
        .collect();
    let lower = section.to_ascii_lowercase();
    let noted = |note: &str| -> BTreeSet<u16> {
        lower
            .match_indices(note)
            .map(|(at, _)| {
                headings
                    .iter()
                    .filter(|(heading, _)| *heading < at)
                    .max()
                    .expect("a heading before the note")
                    .1
            })
            .collect()
    };
    let not_in_v1_0 = noted("thisroutineisnotavailableonv1.0firmware");
    let only_in_v1_2 = noted("thisroutineisonlyavailableonthecpc6128");
    for (address, _) in main {
        let number = number(address);
        let in_v1_1 = !only_in_v1_2.contains(&number);
        let in_v1_0 = in_v1_1 && !not_in_v1_0.contains(&number);
        assert_eq!(
            (
                firmware::entry(Version::V1_0, address).is_some(),
                firmware::entry(Version::V1_1, address).is_some()
            ),
            (in_v1_0, in_v1_1),
            "entry {number} at &{address:04X}"
        );
    }
}
