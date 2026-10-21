use std::os::raw::c_char;

use serde_json::Value;

use crate::emulator::{
    CPC_TICKS_PER_STANDARD_FRAME, SPECTRUM_TICKS_PER_FRAME, cpc_key_for_character,
    spectrum_key_for_character, spectrum_shift, spectrum_shift_SPECTRUM_NO_SHIFT as NO_SHIFT,
    spectrum_shift_SPECTRUM_WITH_CAPS_SHIFT as WITH_CAPS_SHIFT,
};

// A key, as the core numbers it: its line of the matrix times eight, and its
// bit. On a CPC that is the firmware's own number for it: "the key that is
// associated with bit 5 in row 4 has key number 37(4*8+5)" (SOFT 968, The
// Amstrad 6128 Firmware Manual, Appendix XII, section H, at
// https://archive.org/details/SOFT968TheAmstrad6128FirmwareManual).
pub use crate::emulator::keyboard_key as Key;

// keyboard.h's KEYBOARD_NO_KEY, which bindgen cannot read through its cast.
const NO_KEY: Key = 0xFF;

// A CPC's keys by their numbers, named as colophon-player names them, after
// Appendix I of SOFT 968: src/js/emulator/cpc_inscriptions.js at
// https://github.com/tulinmola/colophon-player. Joystick 0 is the keys in
// brackets; joystick 1 is wired to keys that carry letters and digits.
pub const CPC_KEYS: [&str; 80] = [
    "↑", "→", "↓", "f9", "f6", "f3", "ENTER", "f.", //
    "←", "COPY", "f7", "f8", "f5", "f1", "f2", "f0", //
    "CLR", "[", "RETURN", "]", "f4", "SHIFT", "\\", "CTRL", //
    "^", "-", "@", "P", ";", ":", "/", ".", //
    "0", "9", "O", "I", "L", "K", "M", ",", //
    "8", "7", "U", "Y", "H", "J", "N", "SPACE", //
    "6", "5", "R", "T", "G", "F", "B", "V", //
    "4", "3", "E", "W", "S", "D", "C", "X", //
    "1", "2", "ESC", "Q", "TAB", "A", "CAPS", "Z", //
    "(UP)", "(DOWN)", "(LEFT)", "(RIGHT)", "(FIRE 2)", "(FIRE 1)", "(SPARE)", "DEL",
];

// A Spectrum's keys by half-row and bit, half-row 0 selected by A8, as
// spectrum.h numbers them, named as colophon-player names them:
// src/js/emulator/spectrum_inscriptions.js.
pub const SPECTRUM_KEYS: [[&str; 5]; 8] = [
    ["CAPS", "Z", "X", "C", "V"],
    ["A", "S", "D", "F", "G"],
    ["Q", "W", "E", "R", "T"],
    ["1", "2", "3", "4", "5"],
    ["0", "9", "8", "7", "6"],
    ["P", "O", "I", "U", "Y"],
    ["ENTER", "L", "K", "J", "H"],
    ["SPACE", "SYM", "M", "N", "B"],
];

// How many frames a key is held and then let go as text is typed, as the
// emulator's command line types it: FRAMES_KEY_HELD_CPC and the rest in its
// cli/main.c, measured there (its docs/command-line.en.md, "Why those frame
// counts"). A CPC's firmware scans its keyboard "every fiftieth of a second",
// and a key "must be released for two consecutive scans before it is marked
// as released" (SOFT 968, section 3.1). A Spectrum's takes the same key again
// only once a set of its KSTATE variables is free, and while the key is up
// it will "decrease its '5 call counter' and when it reaches zero signal the
// set as free", a call each frame (The Complete Spectrum ROM Disassembly,
// https://skoolkid.github.io/rom/asm/02BF.html).
pub const FRAMES_KEY_HELD_CPC: u64 = 3;
pub const FRAMES_KEY_RELEASED_CPC: u64 = 3;
pub const FRAMES_KEY_HELD_SPECTRUM: u64 = 3;
pub const FRAMES_KEY_RELEASED_SPECTRUM: u64 = 6;

// A character typed: its key, and the shift held with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Keystroke {
    pub key: Key,
    pub shift: Option<Key>,
}

// The ticks a key is held and let go on each machine.
pub fn cpc_typing() -> (u64, u64) {
    let frame = u64::from(CPC_TICKS_PER_STANDARD_FRAME);
    (FRAMES_KEY_HELD_CPC * frame, FRAMES_KEY_RELEASED_CPC * frame)
}

pub fn spectrum_typing() -> (u64, u64) {
    let frame = u64::from(SPECTRUM_TICKS_PER_FRAME);
    (
        FRAMES_KEY_HELD_SPECTRUM * frame,
        FRAMES_KEY_RELEASED_SPECTRUM * frame,
    )
}

fn cpc_named(name: &str) -> Key {
    let index = CPC_KEYS
        .iter()
        .position(|key| *key == name)
        .expect("a CPC key of that name");
    index as Key
}

fn spectrum_named(name: &str) -> Key {
    SPECTRUM_KEYS
        .iter()
        .enumerate()
        .find_map(|(half_row, keys)| {
            let bit = keys.iter().position(|key| *key == name)?;
            Some((half_row * 8 + bit) as Key)
        })
        .expect("a Spectrum key of that name")
}

fn listed(names: impl Iterator<Item = String>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

// A CPC's key, by its name or its number.
pub fn cpc_key(machine: &str, given: &Value) -> Result<Key, String> {
    let found = match given {
        Value::String(name) => CPC_KEYS.iter().position(|key| key == name),
        Value::Number(number) => number
            .as_u64()
            .filter(|number| *number < CPC_KEYS.len() as u64)
            .map(|number| number as usize),
        _ => None,
    };
    found.map(|index| index as Key).ok_or_else(|| {
        format!(
            "no key of a {machine} is {given}; a key is given by its name, or by its number from \
             0 to 79 as the firmware numbers it, its line of the matrix times eight and its bit: {}",
            listed(
                CPC_KEYS
                    .iter()
                    .enumerate()
                    .map(|(number, name)| format!("{number} {name}"))
            )
        )
    })
}

// A Spectrum's key, by its name.
pub fn spectrum_key(machine: &str, given: &Value) -> Result<Key, String> {
    let found = given.as_str().and_then(|name| {
        SPECTRUM_KEYS
            .iter()
            .flatten()
            .any(|key| *key == name)
            .then(|| spectrum_named(name))
    });
    found.ok_or_else(|| {
        format!(
            "no key of a {machine} is {given}; a key is given by its name: {}",
            listed(
                SPECTRUM_KEYS
                    .iter()
                    .flatten()
                    .map(|name| (*name).to_owned())
            )
        )
    })
}

fn unkeyed(machine: &str, character: char) -> String {
    if character.is_ascii() {
        format!(
            "no key of a {machine} types {character:?}, alone or with a shift; any key is held \
             through run's keys"
        )
    } else {
        format!(
            "{character:?} is no ASCII character, and text is typed from ASCII; a key it has is \
             held through run's keys"
        )
    }
}

// A CPC types a character as cpc_key_for_character finds it, and RETURN, TAB,
// ESC and DEL for the four control characters the command line types by its
// escapes \n, \t, \e and \b.
pub fn cpc_keystroke(machine: &str, character: char) -> Result<Keystroke, String> {
    let control = match character {
        '\n' => Some("RETURN"),
        '\t' => Some("TAB"),
        '\u{1b}' => Some("ESC"),
        '\u{8}' => Some("DEL"),
        _ => None,
    };
    if let Some(name) = control {
        return Ok(Keystroke {
            key: cpc_named(name),
            shift: None,
        });
    }
    let ascii = u8::try_from(character)
        .ok()
        .filter(u8::is_ascii)
        .ok_or_else(|| unkeyed(machine, character))?;
    let mut shifted = false;
    // SAFETY: the core only reads its own table and writes the bool.
    let key = unsafe { cpc_key_for_character(ascii as c_char, &mut shifted) };
    if key == NO_KEY {
        return Err(unkeyed(machine, character));
    }
    Ok(Keystroke {
        key,
        shift: shifted.then(|| cpc_named("SHIFT")),
    })
}

// A Spectrum types a character as spectrum_key_for_character finds it, and
// ENTER for a newline, the one escape the command line takes on a Spectrum
// that names a key.
pub fn spectrum_keystroke(machine: &str, character: char) -> Result<Keystroke, String> {
    if character == '\n' {
        return Ok(Keystroke {
            key: spectrum_named("ENTER"),
            shift: None,
        });
    }
    let ascii = u8::try_from(character)
        .ok()
        .filter(u8::is_ascii)
        .ok_or_else(|| unkeyed(machine, character))?;
    let mut shift: spectrum_shift = NO_SHIFT;
    // SAFETY: the core only reads its own table and writes the shift.
    let key = unsafe { spectrum_key_for_character(ascii as c_char, &mut shift) };
    if key == NO_KEY {
        return Err(unkeyed(machine, character));
    }
    let shift = match shift {
        NO_SHIFT => None,
        WITH_CAPS_SHIFT => Some(spectrum_named("CAPS")),
        _ => Some(spectrum_named("SYM")),
    };
    Ok(Keystroke { key, shift })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn emulator_source(file: &str) -> String {
        let path = Path::new(env!("COLOPHON_MCP_EMULATOR_DIR")).join(file);
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    // The value a header's `#define NAME KIND_KEY(line, bit)` gives, read from
    // its source, as line times eight and bit.
    fn defined_key(source: &str, name: &str) -> Key {
        let line = source
            .lines()
            .find(|line| line.starts_with(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("no #define {name}"));
        let (_, arguments) = line.split_once('(').expect("a key macro's arguments");
        let (arguments, _) = arguments.split_once(')').expect("its closing bracket");
        let numbers: Vec<Key> = arguments
            .split(',')
            .map(|number| number.trim().parse().expect("a number"))
            .collect();
        numbers[0] * 8 + numbers[1]
    }

    #[test]
    fn every_key_has_a_name_of_its_own() {
        let mut names: Vec<&str> = CPC_KEYS.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 80);
        let mut names: Vec<&str> = SPECTRUM_KEYS.iter().flatten().copied().collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 40);
    }

    // The keys cpc.h and spectrum.h name are where the tables name them.
    #[test]
    fn the_keys_the_headers_name_are_named_so_here() {
        let cpc = emulator_source("src/cpc.h");
        for (macro_name, name) in [
            ("CPC_RETURN", "RETURN"),
            ("CPC_SHIFT", "SHIFT"),
            ("CPC_SPACE", "SPACE"),
            ("CPC_TAB", "TAB"),
            ("CPC_ESCAPE", "ESC"),
            ("CPC_DELETE", "DEL"),
        ] {
            assert_eq!(
                defined_key(&cpc, macro_name),
                cpc_named(name),
                "{macro_name}"
            );
        }
        let spectrum = emulator_source("src/spectrum.h");
        for (macro_name, name) in [
            ("SPECTRUM_CAPS_SHIFT", "CAPS"),
            ("SPECTRUM_SYMBOL_SHIFT", "SYM"),
            ("SPECTRUM_ENTER", "ENTER"),
            ("SPECTRUM_SPACE", "SPACE"),
        ] {
            assert_eq!(
                defined_key(&spectrum, macro_name),
                spectrum_named(name),
                "{macro_name}"
            );
        }
    }

    // The four control characters a CPC types are the keys cpc.h names.
    #[test]
    fn a_cpcs_control_characters_are_the_keys_its_header_names() {
        let cpc = emulator_source("src/cpc.h");
        for (character, macro_name) in [
            ('\n', "CPC_RETURN"),
            ('\t', "CPC_TAB"),
            ('\u{1b}', "CPC_ESCAPE"),
            ('\u{8}', "CPC_DELETE"),
        ] {
            assert_eq!(
                cpc_keystroke("cpc6128", character),
                Ok(Keystroke {
                    key: defined_key(&cpc, macro_name),
                    shift: None
                }),
                "{macro_name}"
            );
        }
        assert_eq!(
            spectrum_keystroke("spectrum48", '\n').unwrap().key,
            defined_key(&emulator_source("src/spectrum.h"), "SPECTRUM_ENTER")
        );
    }

    // A key named for the character it types is the key the core types it
    // with, unshifted.
    #[test]
    fn a_key_named_for_a_character_types_it() {
        for (number, name) in CPC_KEYS.iter().enumerate() {
            let mut characters = name.chars();
            let (Some(character), None) = (characters.next(), characters.next()) else {
                continue;
            };
            if !character.is_ascii() {
                continue;
            }
            let typed = cpc_keystroke("cpc6128", character.to_ascii_lowercase())
                .unwrap_or_else(|problem| panic!("{name}: {problem}"));
            assert_eq!(typed.key as usize, number, "{name}");
            assert_eq!(typed.shift, None, "{name}");
        }
        for name in SPECTRUM_KEYS.iter().flatten() {
            let mut characters = name.chars();
            let (Some(character), None) = (characters.next(), characters.next()) else {
                continue;
            };
            let typed = spectrum_keystroke("spectrum48", character.to_ascii_lowercase())
                .unwrap_or_else(|problem| panic!("{name}: {problem}"));
            assert_eq!(typed.key, spectrum_named(name), "{name}");
            assert_eq!(typed.shift, None, "{name}");
        }
        assert_eq!(
            cpc_keystroke("cpc6128", ' ').unwrap().key,
            cpc_named("SPACE")
        );
        assert_eq!(
            spectrum_keystroke("spectrum48", ' ').unwrap().key,
            spectrum_named("SPACE")
        );
    }

    // The cadence is the command line's, read from its source.
    #[test]
    fn keys_are_held_and_let_go_as_the_command_line_types() {
        let source = emulator_source("cli/main.c");
        let defined = |name: &str| -> u64 {
            let line = source
                .lines()
                .find(|line| line.starts_with(&format!("#define {name} ")))
                .unwrap_or_else(|| panic!("no #define {name}"));
            line.rsplit(' ').next().unwrap().parse().expect("a count")
        };
        assert_eq!(
            [
                FRAMES_KEY_HELD_CPC,
                FRAMES_KEY_RELEASED_CPC,
                FRAMES_KEY_HELD_SPECTRUM,
                FRAMES_KEY_RELEASED_SPECTRUM,
            ],
            [
                defined("FRAMES_KEY_HELD_CPC"),
                defined("FRAMES_KEY_RELEASED_CPC"),
                defined("FRAMES_KEY_HELD_SPECTRUM"),
                defined("FRAMES_KEY_RELEASED_SPECTRUM"),
            ]
        );
    }

    #[test]
    fn a_key_is_found_by_its_name_or_on_a_cpc_its_number() {
        assert_eq!(cpc_key("cpc6128", &Value::from("SPACE")), Ok(47));
        assert_eq!(cpc_key("cpc6128", &Value::from(47)), Ok(47));
        assert_eq!(cpc_key("cpc6128", &Value::from("(FIRE 2)")), Ok(76));
        assert_eq!(spectrum_key("spectrum48", &Value::from("SPACE")), Ok(56));
        let refused = cpc_key("cpc6128", &Value::from(80)).unwrap_err();
        assert!(
            refused.starts_with(
                "no key of a cpc6128 is 80; a key is given by its name, or by its number"
            ),
            "{refused}"
        );
        assert!(refused.contains("47 SPACE"), "{refused}");
        let refused = spectrum_key("spectrum48", &Value::from(56)).unwrap_err();
        assert!(
            refused
                .starts_with("no key of a spectrum48 is 56; a key is given by its name: CAPS, Z"),
            "{refused}"
        );
    }

    #[test]
    fn a_character_no_key_types_is_refused() {
        assert_eq!(
            cpc_keystroke("cpc6128", '£').unwrap_err(),
            "'£' is no ASCII character, and text is typed from ASCII; a key it has is held \
             through run's keys"
        );
        assert_eq!(
            spectrum_keystroke("spectrum48", '\t').unwrap_err(),
            "no key of a spectrum48 types '\\t', alone or with a shift; any key is held through \
             run's keys"
        );
    }
}
