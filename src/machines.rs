use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::emulator::{
    CPC_TICKS_PER_STANDARD_FRAME, SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE, SPECTRUM_SNAPSHOT_SIZE,
    SPECTRUM_TICKS_PER_FRAME, cpc_t, spectrum_t, z80_t,
};
use crate::fields::{self, Fields, Segment};
use crate::machine::{Condition, Cpc, Match, Run, Spectrum, Watch};

// Named and wired as the emulator's command line names and wires them, in
// its order: its machines table, and the sockets run_cpc fills, in cli/main.c
// at https://github.com/tulinmola/colophon-emulator/blob/0acb96206163686b735a6e17fc6adbcf4cf9a13e/cli/main.c#L107-L136
// and #L847-L855. The command line takes the first bytes of a file longer
// than the firmware it reads; here the file must be the firmware's size, as
// a file of another size is not the firmware it is named for.
pub struct Machine {
    pub name: &'static str,
    board: Board,
}

enum Board {
    // The firmware holds the operating system for the lower ROM, then BASIC
    // for upper ROM 0. The disc interface brings AMSDOS, as upper ROM 7.
    Cpc {
        firmware: &'static str,
        ram_size: usize,
        disc_interface: bool,
    },
    Spectrum {
        firmware: &'static str,
        ram_size: usize,
    },
}

pub const MACHINES: &[Machine] = &[
    Machine {
        name: "cpc6128",
        board: Board::Cpc {
            firmware: "cpc6128.rom",
            ram_size: 0x20000,
            disc_interface: true,
        },
    },
    Machine {
        name: "cpc664",
        board: Board::Cpc {
            firmware: "cpc664.rom",
            ram_size: 0x10000,
            disc_interface: true,
        },
    },
    Machine {
        name: "cpc464",
        board: Board::Cpc {
            firmware: "cpc464.rom",
            ram_size: 0x10000,
            disc_interface: false,
        },
    },
    Machine {
        name: "spectrum48",
        board: Board::Spectrum {
            firmware: "spectrum48.rom",
            ram_size: SPECTRUM_RAM_48K as usize,
        },
    },
];

const AMSDOS: &str = "amsdos.rom";
const CPC_ROM_SIZE: usize = 0x4000;

pub fn named(name: &str) -> Option<&'static Machine> {
    MACHINES.iter().find(|machine| machine.name == name)
}

// A file a machine was built from, named by what it holds.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FileRead {
    /// Where the file was read.
    pub path: PathBuf,
    /// The SHA-256 of what it held, in hexadecimal.
    pub sha256: String,
}

impl FileRead {
    fn of(path: &Path, contents: &[u8]) -> FileRead {
        let digest = Sha256::digest(contents);
        FileRead {
            path: path.to_owned(),
            sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }
}

// What a machine is built from, read and checked, so that building it
// cannot fail but on its snapshot, which only the core can judge.
pub struct Parts {
    memories: Memories,
    snapshot: Option<(PathBuf, Vec<u8>)>,
    pub files: Vec<FileRead>,
}

enum Memories {
    Cpc {
        ram: Box<[u8]>,
        lower_rom: Box<[u8; CPC_ROM_SIZE]>,
        basic: Box<[u8; CPC_ROM_SIZE]>,
        amsdos: Option<Box<[u8; CPC_ROM_SIZE]>>,
    },
    Spectrum {
        ram: Box<[u8]>,
        rom: Box<[u8; SPECTRUM_ROM_SIZE as usize]>,
    },
}

enum Hardware {
    Cpc(Cpc),
    Spectrum(Spectrum),
}

// A machine as the server holds it: the hardware, and the ticks it has run
// since it was stood up, which the core leaves to whoever runs it.
pub struct Built {
    hardware: Hardware,
    tick: u64,
}

fn read(
    roms: &Path,
    file: &str,
    size: usize,
    files: &mut Vec<FileRead>,
) -> Result<Vec<u8>, String> {
    let path = roms.join(file);
    // A pipe or a device under the firmware's name could keep its reader
    // waiting for good.
    if path.exists() && !path.is_file() {
        return Err(format!(
            "{} is not a file; the emulator's `make roms` fetches the firmware into its roms \
             directory as files",
            path.display()
        ));
    }
    let contents = fs::read(&path).map_err(|error| {
        format!(
            "cannot read {}: {error}; the emulator's `make roms` fetches the firmware into its \
             roms directory, and --roms names the directory it is read from",
            path.display()
        )
    })?;
    if contents.len() != size {
        return Err(format!(
            "{} holds {} bytes, expected {size}; the emulator's `make roms` fetches the \
             firmware at its size",
            path.display(),
            contents.len()
        ));
    }
    files.push(FileRead::of(&path, &contents));
    Ok(contents)
}

fn rom<const SIZE: usize>(bytes: &[u8]) -> Box<[u8; SIZE]> {
    bytes
        .to_vec()
        .into_boxed_slice()
        .try_into()
        .expect("read at its size")
}

impl Machine {
    pub fn parts(&self, roms: &Path, snapshot: Option<&Path>) -> Result<Parts, String> {
        let mut files = Vec::new();
        let memories = match self.board {
            Board::Cpc {
                firmware,
                ram_size,
                disc_interface,
            } => {
                let firmware = read(roms, firmware, 2 * CPC_ROM_SIZE, &mut files)?;
                let amsdos = if disc_interface {
                    Some(rom(&read(roms, AMSDOS, CPC_ROM_SIZE, &mut files)?))
                } else {
                    None
                };
                let (lower_rom, basic) = firmware.split_at(CPC_ROM_SIZE);
                Memories::Cpc {
                    ram: vec![0; ram_size].into_boxed_slice(),
                    lower_rom: rom(lower_rom),
                    basic: rom(basic),
                    amsdos,
                }
            }
            Board::Spectrum { firmware, ram_size } => Memories::Spectrum {
                ram: vec![0; ram_size].into_boxed_slice(),
                rom: rom(&read(
                    roms,
                    firmware,
                    SPECTRUM_ROM_SIZE as usize,
                    &mut files,
                )?),
            },
        };
        let snapshot = match snapshot {
            None => None,
            Some(path) => {
                let contents = self.snapshot(path)?;
                files.push(FileRead::of(path, &contents));
                Some((path.to_owned(), contents))
            }
        };
        Ok(Parts {
            memories,
            snapshot,
            files,
        })
    }

    // A file that is no snapshot of this machine's is refused by its size
    // before it is read, so that neither a pipe nor a device can hold the
    // server up. A Spectrum's is one length; a CPC's of version 3 may carry
    // chunks of its writer's own after its memory, as many as it kept
    // ("Snapshot (.SNA) file format", cpctech), so its bound stands far past
    // them.
    fn snapshot(&self, path: &Path) -> Result<Vec<u8>, String> {
        let unreadable = |error: io::Error| {
            format!(
                "cannot read {}: {error}; a snapshot is named by its path on the file system the \
                 server runs on",
                path.display()
            )
        };
        let largest = match self.board {
            Board::Cpc { .. } => 8 * 1024 * 1024,
            Board::Spectrum { .. } => u64::from(SPECTRUM_SNAPSHOT_SIZE),
        };
        let metadata = fs::metadata(path).map_err(unreadable)?;
        if !metadata.is_file() || metadata.len() > largest {
            return Err(format!(
                "{} is not a file of at most {largest} bytes; {}",
                path.display(),
                self.snapshots()
            ));
        }
        let mut contents = Vec::new();
        fs::File::open(path)
            .and_then(|file| file.take(largest + 1).read_to_end(&mut contents))
            .map_err(unreadable)?;
        if contents.len() as u64 > largest {
            return Err(format!(
                "{} grew past {largest} bytes while it was read; {}",
                path.display(),
                self.snapshots()
            ));
        }
        Ok(contents)
    }

    // How long a frame of this machine is when nothing has reprogrammed it.
    pub fn standard_frame(&self) -> u64 {
        match self.board {
            Board::Cpc { .. } => u64::from(CPC_TICKS_PER_STANDARD_FRAME),
            Board::Spectrum { .. } => u64::from(SPECTRUM_TICKS_PER_FRAME),
        }
    }

    // What machine_open would take, said when a snapshot is refused.
    fn snapshots(&self) -> &'static str {
        match self.board {
            Board::Cpc { .. } => {
                "a CPC opens an SNA snapshot of version 1, 2 or 3 holding no more RAM than it \
                 has, in the dump that follows its header: 128K on a cpc6128, 64K on a cpc664 \
                 or a cpc464, which have no PAL and take only RAM configuration 0"
            }
            Board::Spectrum { .. } => "a spectrum48 opens a 48K .sna snapshot of 49179 bytes",
        }
    }
}

impl Parts {
    pub fn build(self, machine: &Machine) -> Result<Built, String> {
        let mut hardware = match self.memories {
            Memories::Cpc {
                ram,
                lower_rom,
                basic,
                amsdos,
            } => {
                let mut cpc = Cpc::new(ram, lower_rom);
                cpc.set_upper_rom(0, basic);
                if let Some(amsdos) = amsdos {
                    cpc.fit_disc_interface(true);
                    cpc.set_upper_rom(7, amsdos);
                }
                Hardware::Cpc(cpc)
            }
            Memories::Spectrum { ram, rom } => Hardware::Spectrum(Spectrum::new(ram, rom)),
        };
        if let Some((path, bytes)) = self.snapshot {
            let loaded = match &mut hardware {
                Hardware::Cpc(cpc) => cpc.snapshot_load(&bytes),
                Hardware::Spectrum(spectrum) => spectrum.snapshot_load(&bytes),
            };
            loaded.map_err(|problem| {
                format!("{} {problem}; {}", path.display(), machine.snapshots())
            })?;
        }
        Ok(Built { hardware, tick: 0 })
    }
}

// What a run saw, when it watched: every match, in the order it happened,
// its tick counted from the machine's start.
pub struct Watched {
    pub matches: Vec<Match>,
    pub matched: bool,
    pub full: bool,
    pub returned: bool,
}

// Past the match that fills a gathering, a run sees out the instruction under
// way, which makes four accesses at most (EX (SP),IX), then the response to
// an interrupt taken at its end, four more (in mode 2), and the instruction it
// stops before.
const ROOM_PAST_FULL: u32 = 9;

// The machine as it stood, put back when a call is refused after its values
// were written.
enum Kept {
    Cpc(Box<cpc_t>),
    Spectrum(Box<spectrum_t>),
}

// Every write is made to a copy, so that a value refused leaves the machine
// as it stood.
fn written<T: Fields>(
    machine: &T,
    values: &[(&String, Vec<Segment>, &Value)],
) -> Result<T, String> {
    // SAFETY: a machine's struct owns nothing, so a copy of its bytes is a
    // value of its own: it is dropped, or moved back over the machine, and
    // never run. set writes no pointer, so every pointer in the copy is one
    // the machine already holds.
    let mut copy = unsafe { std::ptr::read(machine) };
    for (path, segments, value) in values {
        copy.set(segments, value)
            .map_err(|problem| format!("{path}: {problem}"))?;
    }
    Ok(copy)
}

impl Built {
    pub fn tick(&self) -> u64 {
        self.tick
    }

    pub fn pc(&self) -> u16 {
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.machine().cpu.pc,
            Hardware::Spectrum(spectrum) => spectrum.machine().cpu.pc,
        }
    }

    fn cpu(&self) -> &z80_t {
        match &self.hardware {
            Hardware::Cpc(cpc) => &cpc.machine().cpu,
            Hardware::Spectrum(spectrum) => &spectrum.machine().cpu,
        }
    }

    fn cpu_mut(&mut self) -> &mut z80_t {
        match &mut self.hardware {
            Hardware::Cpc(cpc) => &mut cpc.machine_mut().cpu,
            Hardware::Spectrum(spectrum) => &mut spectrum.machine_mut().cpu,
        }
    }

    // A run watches for the conditions given, gathering up to most_gathered
    // matches, and a call's for its return as well.
    pub fn run(
        &mut self,
        frames: u32,
        limit: u64,
        conditions: &[Condition],
        most_gathered: u32,
    ) -> (Run, Watched) {
        self.watched(frames, limit, conditions, most_gathered, None)
    }

    fn watched(
        &mut self,
        frames: u32,
        limit: u64,
        conditions: &[Condition],
        most_gathered: u32,
        returning: Option<(u16, u16)>,
    ) -> (Run, Watched) {
        // A gathering that may hold nothing fills at its first match.
        let room = most_gathered.max(1).saturating_add(ROOM_PAST_FULL);
        // SAFETY: bindgen renders a match as integers and bools, and all-zero
        // bytes are a value of each.
        let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; room as usize];
        let mut watch = Watch::new(conditions, &mut matches, most_gathered);
        if let Some((pc, sp)) = returning {
            watch.returning_to(pc, sp);
        }
        let watching = !conditions.is_empty() || returning.is_some();
        let run = match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.run(frames, limit, watching.then_some(&mut watch)),
            Hardware::Spectrum(spectrum) => {
                spectrum.run(frames, limit, watching.then_some(&mut watch))
            }
        };
        let watch = *watch.seen();
        assert!(
            watch.match_count <= room,
            "a run saw {} matches, past the {room} there was room for",
            watch.match_count
        );
        matches.truncate(watch.match_count as usize);
        for seen in &mut matches {
            seen.tick += self.tick;
        }
        self.tick += run.ticks;
        let watched = Watched {
            matches,
            matched: watch.matched,
            full: watch.full,
            returned: watch.returned,
        };
        (run, watched)
    }

    // Writes the values, as set does, then calls the routine and runs until
    // it returns. A processor halted on HALT is woken for the routine, and
    // halted again once it has returned.
    pub fn call(
        &mut self,
        address: u16,
        values: &Map<String, Value>,
        frames: u32,
        limit: u64,
        conditions: &[Condition],
        most_gathered: u32,
    ) -> Result<(Run, Watched), String> {
        let kept = (!values.is_empty()).then(|| self.keep());
        if !values.is_empty() {
            self.set(values)?;
        }
        let (pc, sp, halted) = {
            let cpu = self.cpu();
            (cpu.pc, cpu.sp, cpu.halted)
        };
        let called = match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.call(address),
            Hardware::Spectrum(spectrum) => spectrum.call(address),
        };
        if let Err(problem) = called {
            if let Some(kept) = kept {
                self.restore(kept);
            }
            return Err(format!("cannot call &{address:04X}: {problem}"));
        }
        self.cpu_mut().halted = false;
        let (run, watched) = self.watched(frames, limit, conditions, most_gathered, Some((pc, sp)));
        if watched.returned {
            self.cpu_mut().halted = halted;
        }
        Ok((run, watched))
    }

    fn keep(&self) -> Kept {
        // SAFETY: as in written(): a copy of a machine's bytes owns nothing,
        // and is moved back over the machine or dropped.
        match &self.hardware {
            Hardware::Cpc(cpc) => Kept::Cpc(Box::new(unsafe { std::ptr::read(cpc.machine()) })),
            Hardware::Spectrum(spectrum) => {
                Kept::Spectrum(Box::new(unsafe { std::ptr::read(spectrum.machine()) }))
            }
        }
    }

    fn restore(&mut self, kept: Kept) {
        match (&mut self.hardware, kept) {
            (Hardware::Cpc(cpc), Kept::Cpc(kept)) => {
                *cpc.machine_mut() = *kept;
                cpc.remap();
            }
            (Hardware::Spectrum(spectrum), Kept::Spectrum(kept)) => {
                *spectrum.machine_mut() = *kept;
                spectrum.seek();
            }
            _ => unreachable!("a machine is kept as the machine it is"),
        }
    }

    pub fn step(&mut self, instructions: u32) -> Run {
        let run = match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.step(instructions),
            Hardware::Spectrum(spectrum) => spectrum.step(instructions),
        };
        self.tick += run.ticks;
        run
    }

    pub fn inspect(&self, path: &str) -> Result<Value, String> {
        let segments = fields::parse(path)?;
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.machine().get(&segments),
            Hardware::Spectrum(spectrum) => spectrum.machine().get(&segments),
        }
        .map_err(|problem| format!("{path}: {problem}"))
    }

    // Writes every value or none, and answers each path as it now reads.
    pub fn set(&mut self, values: &Map<String, Value>) -> Result<Map<String, Value>, String> {
        let values = values
            .iter()
            .map(|(path, value)| Ok((path, fields::parse(path)?, value)))
            .collect::<Result<Vec<_>, String>>()?;
        match &mut self.hardware {
            Hardware::Cpc(cpc) => {
                let written = written(cpc.machine(), &values)?;
                cpc.check(&written)?;
                *cpc.machine_mut() = written;
                cpc.remap();
            }
            Hardware::Spectrum(spectrum) => {
                let written = written(spectrum.machine(), &values)?;
                spectrum.check(&written)?;
                *spectrum.machine_mut() = written;
                spectrum.seek();
            }
        }
        values
            .iter()
            .map(|(path, _, _)| Ok(((*path).clone(), self.inspect(path)?)))
            .collect()
    }

    pub fn peek(&self, address: u16, length: u16) -> Vec<u8> {
        (0..length)
            .map(|offset| {
                let address = address.wrapping_add(offset);
                match &self.hardware {
                    Hardware::Cpc(cpc) => cpc.peek(address),
                    Hardware::Spectrum(spectrum) => spectrum.peek(address),
                }
            })
            .collect()
    }

    pub fn poke(&mut self, address: u16, bytes: &[u8]) {
        for (offset, value) in (0..).zip(bytes) {
            let address = address.wrapping_add(offset);
            match &mut self.hardware {
                Hardware::Cpc(cpc) => cpc.poke(address, *value),
                Hardware::Spectrum(spectrum) => spectrum.poke(address, *value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::CPC_MANUFACTURER_AMSTRAD;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Stand-ins for the firmware, each file filled with a byte of its own so
    // that one cannot pass for another.
    const STAND_INS: &[(&str, usize, u8)] = &[
        ("cpc6128.rom", 0x8000, 0x61),
        ("cpc664.rom", 0x8000, 0x66),
        ("cpc464.rom", 0x8000, 0x46),
        ("amsdos.rom", 0x4000, 0xAD),
        ("spectrum48.rom", 0x4000, 0x48),
    ];

    struct Roms(PathBuf);

    impl Roms {
        fn new(files: &[(&str, usize, u8)]) -> Roms {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "colophon-mcp-machines-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("a scratch directory");
            for &(file, size, fill) in files {
                // The second half of a CPC's firmware is BASIC's, and told
                // apart from the first.
                let mut contents = vec![fill; size];
                if size == 0x8000 {
                    contents[0x4000..].fill(!fill);
                }
                fs::write(path.join(file), contents).expect("a stand-in is written");
            }
            Roms(path)
        }

        fn contents(&self, file: &str) -> Vec<u8> {
            fs::read(self.0.join(file)).expect("a stand-in is read")
        }
    }

    impl Drop for Roms {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sixteen_k(rom: *const u8) -> Vec<u8> {
        assert!(!rom.is_null(), "no ROM is fitted");
        // SAFETY: every ROM a CPC is handed is 16K, and the machine holding
        // it stands while it is copied.
        unsafe { std::slice::from_raw_parts(rom, CPC_ROM_SIZE) }.to_vec()
    }

    fn build(name: &str, roms: &Roms) -> Built {
        let machine = named(name).expect("a machine of that name");
        machine
            .parts(&roms.0, None)
            .expect("its firmware is read")
            .build(machine)
            .expect("a machine without a snapshot is always built")
    }

    fn cpc(name: &str, roms: &Roms) -> Cpc {
        match build(name, roms).hardware {
            Hardware::Cpc(cpc) => cpc,
            Hardware::Spectrum(_) => panic!("{name} is built as a Spectrum"),
        }
    }

    // The command line's own table, read from its source, so that a machine
    // it gains or changes cannot go unnoticed here: each machine's name, its
    // firmware, its RAM and whether it has the disc interface.
    fn command_line_machines() -> Vec<(String, String, usize, bool)> {
        let path = Path::new(env!("COLOPHON_MCP_EMULATOR_DIR")).join("cli/main.c");
        let source = fs::read_to_string(&path).expect("the command line's source");
        let (_, table) = source
            .split_once("static const machine_t machines[] = {")
            .expect("its machines table");
        let (table, _) = table.split_once("\n};").expect("the table's end");
        table
            .split("{.kind =")
            .skip(1)
            .map(|entry| {
                let field = |name: &str| {
                    let (_, value) = entry
                        .split_once(&format!(".{name} = "))
                        .unwrap_or_else(|| panic!("no {name} in {entry}"));
                    value
                        .split(',')
                        .next()
                        .unwrap_or(value)
                        .trim()
                        .trim_matches('"')
                        .to_owned()
                };
                let ram_size = match field("ram_size").as_str() {
                    "SPECTRUM_RAM_48K" => SPECTRUM_RAM_48K as usize,
                    size => usize::from_str_radix(size.trim_start_matches("0x"), 16)
                        .unwrap_or_else(|_| panic!("a RAM size of {size}")),
                };
                (
                    field("name"),
                    field("rom_file"),
                    ram_size,
                    field("disc_interface") == "true",
                )
            })
            .collect()
    }

    #[test]
    fn the_machines_are_the_command_lines() {
        let table: Vec<(String, String, usize, bool)> = MACHINES
            .iter()
            .map(|machine| match machine.board {
                Board::Cpc {
                    firmware,
                    ram_size,
                    disc_interface,
                } => (
                    machine.name.to_owned(),
                    firmware.to_owned(),
                    ram_size,
                    disc_interface,
                ),
                Board::Spectrum { firmware, ram_size } => (
                    machine.name.to_owned(),
                    firmware.to_owned(),
                    ram_size,
                    false,
                ),
            })
            .collect();
        assert_eq!(table, command_line_machines());
    }

    // As run_cpc does: the firmware's first 16K is the lower ROM and the rest
    // upper ROM 0, AMSDOS is ROM 7 where the disc interface is fitted, and
    // the links are left 50Hz and Amstrad's.
    #[test]
    fn each_cpc_is_wired_as_the_command_line_wires_it() {
        let roms = Roms::new(STAND_INS);
        for machine in MACHINES {
            let Board::Cpc {
                firmware,
                ram_size,
                disc_interface,
            } = machine.board
            else {
                continue;
            };
            let name = machine.name;
            let cpc = cpc(name, &roms);
            let machine = cpc.machine();
            let firmware = roms.contents(firmware);
            assert_eq!(machine.ram_size as usize, ram_size, "{name}");
            assert!(machine.fifty_hz, "{name}");
            assert_eq!(
                u32::from(machine.manufacturer),
                CPC_MANUFACTURER_AMSTRAD,
                "{name}"
            );
            assert_eq!(sixteen_k(machine.lower_rom), firmware[..0x4000], "{name}");
            assert_eq!(
                sixteen_k(machine.upper_roms[0]),
                firmware[0x4000..],
                "{name}"
            );
            assert_eq!(machine.disc_interface, disc_interface, "{name}");
            if disc_interface {
                assert_eq!(
                    sixteen_k(machine.upper_roms[7]),
                    roms.contents("amsdos.rom"),
                    "{name}"
                );
            } else {
                assert!(machine.upper_roms[7].is_null(), "{name}");
            }
        }
    }

    #[test]
    fn the_spectrum_is_wired_as_the_command_line_wires_it() {
        let roms = Roms::new(STAND_INS);
        let Hardware::Spectrum(spectrum) = build("spectrum48", &roms).hardware else {
            panic!("spectrum48 is built as a CPC");
        };
        let machine = spectrum.machine();
        assert_eq!(machine.ram_size, SPECTRUM_RAM_48K);
        // SAFETY: as for a CPC's ROMs, at the Spectrum's size.
        let rom = unsafe { std::slice::from_raw_parts(machine.rom, SPECTRUM_ROM_SIZE as usize) };
        assert_eq!(rom, roms.contents("spectrum48.rom"));
    }

    // NIST's worked examples of SHA-256, its "One Block Message Sample" and
    // "Two Block Message Sample":
    // https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Standards-and-Guidelines/documents/examples/SHA256.pdf
    #[test]
    fn a_file_is_named_by_its_sha256() {
        assert_eq!(
            FileRead::of(Path::new("one block"), b"abc").sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            FileRead::of(
                Path::new("two blocks"),
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )
            .sha256,
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_cpc_without_a_disc_interface_reads_no_amsdos() {
        let roms = Roms::new(&[("cpc464.rom", 0x8000, 0x46)]);
        cpc("cpc464", &roms);
    }

    #[test]
    fn missing_firmware_says_where_it_was_looked_for_and_how_to_fetch_it() {
        let roms = Roms::new(&[("cpc6128.rom", 0x8000, 0x61)]);
        let Err(problem) = named("cpc6128").expect("the 6128").parts(&roms.0, None) else {
            panic!("the 6128 is built without AMSDOS");
        };
        assert!(
            problem.starts_with(&format!(
                "cannot read {}",
                roms.0.join("amsdos.rom").display()
            )),
            "{problem}"
        );
        assert!(problem.contains("`make roms`"), "{problem}");
        assert!(problem.contains("--roms"), "{problem}");
    }

    #[test]
    fn firmware_of_another_size_is_refused() {
        let roms = Roms::new(&[("spectrum48.rom", 0x8000, 0x48)]);
        let Err(problem) = named("spectrum48").expect("the 48").parts(&roms.0, None) else {
            panic!("a 32K Spectrum ROM is taken");
        };
        assert_eq!(
            problem,
            format!(
                "{} holds 32768 bytes, expected 16384; the emulator's `make roms` fetches the \
                 firmware at its size",
                roms.0.join("spectrum48.rom").display()
            )
        );
    }
}
