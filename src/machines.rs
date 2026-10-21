use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::disassembly;
use crate::emulator::{
    CPC_FRAMEBUFFER_WIDTH, CPC_TICKS_PER_STANDARD_FRAME, SPECTRUM_FRAMEBUFFER_WIDTH,
    SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE, SPECTRUM_SNAPSHOT_SIZE, SPECTRUM_TICKS_PER_FRAME,
    keyboard_t, z80_t,
};
use crate::fields::{self, Fields, Segment};
use crate::keys::{self, Key, Keystroke};
use crate::machine::{
    COUNT, COUNTS_PER_KIND, Condition, Cpc, CpcState, KINDS, Kind, Match, Run, Spectrum,
    SpectrumState, Watch, gate_array_rgb, ula_rgb,
};
use crate::picture::Picture;
use crate::screen::{self, Asked, Screen};

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

// The window the emulator's command line crops a machine's screenshots to,
// and whether it draws each line twice: CPC_CROP_* and SPECTRUM_CROP_* in its
// cli/main.c, and double_lines in its machines table.
struct Window {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    doubled: bool,
}

const CPC_WINDOW: Window = Window {
    left: 208,
    top: 34,
    width: 768,
    height: 272,
    doubled: true,
};

const SPECTRUM_WINDOW: Window = Window {
    left: 96,
    top: 20,
    width: 352,
    height: 264,
    doubled: false,
};

// gate_array_rgb reads a CPC's colour code by its lowest five bits, and
// ula_rgb a Spectrum's sample by its lowest four.
const CPC_COLOUR_CODES: u8 = 32;
const SPECTRUM_SAMPLES: u8 = 16;

pub fn named(name: &str) -> Option<&'static Machine> {
    MACHINES.iter().find(|machine| machine.name == name)
}

// A file read for a machine or written from one, named by what it holds.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FileRead {
    /// Where the file was read, an absolute path.
    pub path: PathBuf,
    /// The SHA-256 of what it held, in hexadecimal.
    pub sha256: String,
}

impl FileRead {
    pub fn of(path: &Path, contents: &[u8]) -> FileRead {
        let digest = Sha256::digest(contents);
        FileRead {
            path: path.to_owned(),
            sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }
}

pub const DISCS: &str = "a disc is a DSK image, of the original layout or the extended one";
pub const TAPES: &str = "a tape is a .tap, of bare blocks at the ROM's own timings, or a .tzx";

// Where a relative path is taken from, said beside a file that could not be
// reached by it.
fn working_directory_clause() -> String {
    match std::env::current_dir() {
        Ok(directory) => format!(
            "a relative path is taken from the server's working directory, {}",
            directory.display()
        ),
        Err(error) => format!(
            "the server's working directory cannot be read ({error}), so a path is given absolute"
        ),
    }
}

// A path as the file system the server runs on takes it, from the server's
// working directory if relative, so that an answer or a journal names the
// same file wherever it is read.
pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    // Quoted, since the path refused may be empty.
    std::path::absolute(path).map_err(|error| {
        format!(
            "cannot make {path:?} absolute: {error}; a file is named by its path on the file \
             system the server runs on, and {}",
            working_directory_clause()
        )
    })
}

// A file read whole, refused by its size before it is read, so that neither
// a pipe nor a device can hold the server up; `what` names it and `takes`
// says what would be read.
fn read_whole(path: &Path, largest: u64, what: &str, takes: &str) -> Result<Vec<u8>, String> {
    let unreadable = |error: io::Error| {
        format!(
            "cannot read {}: {error}; {what} is named by its path on the file system the server \
             runs on, and {}",
            path.display(),
            working_directory_clause()
        )
    };
    let metadata = fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() || metadata.len() > largest {
        return Err(format!(
            "{} is not a file of at most {largest} bytes; {takes}",
            path.display()
        ));
    }
    let mut contents = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(largest + 1).read_to_end(&mut contents))
        .map_err(unreadable)?;
    if contents.len() as u64 > largest {
        return Err(format!(
            "{} grew past {largest} bytes while it was read; {takes}",
            path.display()
        ));
    }
    Ok(contents)
}

// Writes a file only where none is, so that none is ever written over, and
// takes away what it began if it cannot finish.
pub fn write_new(path: &Path, contents: &[u8]) -> Result<FileRead, String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "{} is taken, and a file is written only where none is: give a path that \
                     names nothing yet",
                    path.display()
                )
            } else {
                format!(
                    "cannot write {}: {error}; a file is written at a path on the file system \
                     the server runs on, in a directory that is there, and {}",
                    path.display(),
                    working_directory_clause()
                )
            }
        })?;
    if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let begun = match fs::remove_file(path) {
            Ok(()) => "what was begun there is taken away again",
            Err(_) => "what was begun there is left, unfinished, and holds that path",
        };
        return Err(format!(
            "cannot write {}: {error}; {begun}, and the file is written again when asked, at a \
             path the file system can take it at",
            path.display()
        ));
    }
    Ok(FileRead::of(path, contents))
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

enum HardwareState {
    Cpc(CpcState),
    Spectrum(SpectrumState),
}

// A request that stood a machine up or changed it.
#[derive(Clone, Serialize, JsonSchema)]
pub struct Entry {
    /// The machine's tick as the request began.
    pub tick: u64,
    /// The tool asked.
    pub tool: &'static str,
    /// The arguments it was given, save the handle.
    #[schemars(schema_with = "crate::server::values_schema")]
    pub arguments: Value,
    /// The snapshot, the disc or the tape it read, named by what it held.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::server::absent_not_null)]
    pub file: Option<FileRead>,
}

// What has been done to a machine, in the order it was done: the stretches
// that states were kept at the end of, which they share, and the entries
// since the last.
#[derive(Clone)]
struct Journal {
    kept: Vec<Rc<[Entry]>>,
    since: Vec<Entry>,
}

impl Journal {
    fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.kept
            .iter()
            .flat_map(|kept| kept.iter())
            .chain(&self.since)
    }

    fn len(&self) -> usize {
        self.kept.iter().map(|kept| kept.len()).sum::<usize>() + self.since.len()
    }

    // The journal as a state keeps it, sharing what earlier states keep.
    fn keep(&mut self) -> Journal {
        if !self.since.is_empty() {
            self.kept.push(std::mem::take(&mut self.since).into());
        }
        self.clone()
    }
}

// A machine kept as it stood, with the ticks it had run and its journal then.
struct State {
    hardware: HardwareState,
    tick: u64,
    journal: Journal,
}

// A machine as the server holds it: the hardware, the ticks it has run since
// it was stood up, which the core leaves to whoever runs it, the files it was
// built from, what has been done to it since on the branch it stands on, and
// the states kept of it, by name.
pub struct Built {
    hardware: Hardware,
    tick: u64,
    files: Vec<FileRead>,
    journal: Journal,
    states: BTreeMap<String, State>,
}

fn read(
    roms: &Path,
    file: &str,
    size: usize,
    files: &mut Vec<FileRead>,
) -> Result<Vec<u8>, String> {
    let path = absolute(&roms.join(file))?;
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
             roms directory; --roms names the directory it is read from, and {}",
            path.display(),
            working_directory_clause()
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
    // before it is read. A Spectrum's is one length; a CPC's of version 3 may
    // carry chunks of its writer's own after its memory, as many as it kept
    // ("Snapshot (.SNA) file format", cpctech), so its bound stands far past
    // them.
    fn snapshot(&self, path: &Path) -> Result<Vec<u8>, String> {
        let largest = match self.board {
            Board::Cpc { .. } => 8 * 1024 * 1024,
            Board::Spectrum { .. } => u64::from(SPECTRUM_SNAPSHOT_SIZE),
        };
        read_whole(path, largest, "a snapshot", self.snapshots())
    }

    // A disc image, of either layout, describes at most 102 cylinders of two
    // sides (FLOPPY_MAX_CYLINDERS and FLOPPY_MAX_SIDES in floppy.h), and a
    // track of at most 65535 bytes, after a header of 256, which comes to
    // under 14MB.
    pub fn disc(&self, path: &Path) -> Result<Vec<u8>, String> {
        self.takes_discs()?;
        read_whole(path, 16 * 1024 * 1024, "a disc", DISCS)
    }

    // A tape is held whole for as long as it is in the deck. 90 minutes of
    // tape at the ROM's own timings, where "a reset bit is encoded as two
    // pulses of 855 tstates each, a set bit as two pulses of 1710 tstates
    // each" (comp.sys.sinclair FAQ, 16K/48K reference), hold under 1.4MB at
    // 3.5MHz, and the bound stands far past them.
    pub fn tape(&self, path: &Path) -> Result<Vec<u8>, String> {
        self.takes_tapes()?;
        read_whole(path, 16 * 1024 * 1024, "a tape", TAPES)
    }

    // How many characters one request may type: as many as are held and let
    // go in the frames a run may take.
    pub fn most_typed(&self, most_frames: u32) -> usize {
        let (held, released) = self.typing();
        (u64::from(most_frames) * self.standard_frame() / (held + released)) as usize
    }

    // The ticks a key is held and then let go as text is typed.
    pub fn typing(&self) -> (u64, u64) {
        match self.board {
            Board::Cpc { .. } => keys::cpc_typing(),
            Board::Spectrum { .. } => keys::spectrum_typing(),
        }
    }

    // A key of this machine's, by its name, or on a CPC by its number.
    pub fn key(&self, given: &Value) -> Result<Key, String> {
        match self.board {
            Board::Cpc { .. } => keys::cpc_key(self.name, given),
            Board::Spectrum { .. } => keys::spectrum_key(self.name, given),
        }
    }

    // The keys that type the text, or what no key types.
    pub fn keystrokes(&self, text: &str) -> Result<Vec<Keystroke>, String> {
        text.chars()
            .map(|character| match self.board {
                Board::Cpc { .. } => keys::cpc_keystroke(self.name, character),
                Board::Spectrum { .. } => keys::spectrum_keystroke(self.name, character),
            })
            .collect()
    }

    // Whether the machine takes a disc, and what does if it does not.
    pub fn takes_discs(&self) -> Result<(), String> {
        match self.board {
            Board::Cpc {
                disc_interface: true,
                ..
            } => Ok(()),
            Board::Cpc { .. } => Err(format!(
                "a {} is built here as the emulator's command line builds one given no disc, \
                 without a disc interface, and nothing here fits one afterwards; a cpc664 or a \
                 cpc6128 takes a disc",
                self.name
            )),
            Board::Spectrum { .. } => Err(format!(
                "a {} has no disc drive; a cpc664 or a cpc6128 takes a disc, and a Spectrum a \
                 tape",
                self.name
            )),
        }
    }

    // Whether the machine takes a tape, and what does if it does not.
    pub fn takes_tapes(&self) -> Result<(), String> {
        match self.board {
            Board::Spectrum { .. } => Ok(()),
            Board::Cpc { .. } => Err(format!(
                "a {} is given no tape here: the emulator has never loaded a tape through a CPC's \
                 firmware, so nothing here could hold one to what the machine did; a spectrum48 \
                 takes a tape, and a cpc664 or a cpc6128 a disc",
                self.name
            )),
        }
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
        let mut opening = Entry {
            tick: 0,
            tool: "machine_open",
            arguments: json!({ "machine": machine.name }),
            file: None,
        };
        if let Some((path, bytes)) = self.snapshot {
            let loaded = match &mut hardware {
                Hardware::Cpc(cpc) => cpc.snapshot_load(&bytes),
                Hardware::Spectrum(spectrum) => spectrum.snapshot_load(&bytes),
            };
            loaded.map_err(|problem| {
                format!("{} {problem}; {}", path.display(), machine.snapshots())
            })?;
            opening.arguments["snapshot"] = json!(path);
            opening.file = Some(FileRead::of(&path, &bytes));
        }
        Ok(Built {
            hardware,
            tick: 0,
            files: self.files,
            journal: Journal {
                kept: Vec::new(),
                since: vec![opening],
            },
            states: BTreeMap::new(),
        })
    }
}

// The monitor's picture, and where its beam stands, in samples across and
// lines down the raster.
pub struct Shown {
    pub picture: Picture,
    pub beam_x: u16,
    pub beam_y: u16,
    pub whole: bool,
}

// What a run watches for, and how many matches it gathers and places it
// counts before it stops.
pub struct Watching<'a> {
    pub conditions: &'a [Condition],
    pub most_gathered: u32,
    pub most_counted: u32,
}

// What a run saw, when it watched: every match, in the order it happened,
// its tick counted from the machine's start, and every place counted, by kind
// and then by address; and whether the processor halting for good stopped
// it, which a run watched or not may.
pub struct Watched {
    pub matches: Vec<Match>,
    pub counts: Vec<(Kind, u16, u32)>,
    pub matched: bool,
    pub full: bool,
    pub returned: bool,
    pub halted: bool,
}

// Past the match that fills a gathering, a run sees out the instruction under
// way, which makes four accesses at most (EX (SP),IX), then the response to
// an interrupt taken at its end, four more (in mode 2), and the instruction it
// stops before.
const ROOM_PAST_FULL: u32 = 9;

// A state named when it was asked for and dropped before the asking reached
// the machine.
fn gone(name: &str) -> String {
    format!(
        "{name} was dropped as it was asked for; machine_list names the states kept from each \
         machine"
    )
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

    fn keyboard_mut(&mut self) -> &mut keyboard_t {
        match &mut self.hardware {
            Hardware::Cpc(cpc) => &mut cpc.machine_mut().keyboard,
            Hardware::Spectrum(spectrum) => &mut spectrum.machine_mut().keyboard,
        }
    }

    // A run watches for what it is given, and a call's for its return as
    // well. The keys are held down through it, on top of what the keyboard
    // holds, which it is left holding.
    pub fn run(
        &mut self,
        frames: u32,
        limit: u64,
        watching: &Watching,
        keys: &[Key],
    ) -> (Run, Watched) {
        let kept = *self.keyboard_mut();
        for key in keys {
            self.keyboard_mut().press(*key);
        }
        let ran = self.watched(frames, limit, watching, None);
        *self.keyboard_mut() = kept;
        ran
    }

    // Types as the emulator's command line types: each keystroke's keys held
    // down for `held` ticks, on top of what the keyboard holds, then the
    // keyboard as it stood for `released`. Each stretch ends between
    // instructions, as a run does, and is measured from where the typing
    // began, so that a key goes down or comes up a few ticks from where the
    // command line puts it, and the ticks one stretch runs past its end are
    // not added to the next. A stretch that ends inside a chain of prefixes
    // ends the typing there, as it ends a run, with the keyboard as it stood.
    pub fn r#type(&mut self, keystrokes: &[Keystroke], held: u64, released: u64) -> Run {
        let kept = *self.keyboard_mut();
        let mut typed = Run {
            ticks: 0,
            frames: 0,
            settled: true,
        };
        let mut until = 0;
        for keystroke in keystrokes {
            for key in keystroke.shift.into_iter().chain([keystroke.key]) {
                self.keyboard_mut().press(key);
            }
            until += held;
            self.run_until(until, &mut typed);
            *self.keyboard_mut() = kept;
            until += released;
            if typed.settled {
                self.run_until(until, &mut typed);
            }
            if !typed.settled {
                break;
            }
        }
        typed
    }

    // A stretch stopped short where the processor halts for good runs on
    // from there, as a run begun there does, so that typing takes its ticks
    // whatever the program does.
    fn run_until(&mut self, until: u64, typed: &mut Run) {
        let nothing = Watching {
            conditions: &[],
            most_gathered: 0,
            most_counted: 0,
        };
        while typed.settled && typed.ticks < until {
            let (run, _) = self.watched(u32::MAX, until - typed.ticks, &nothing, None);
            typed.ticks += run.ticks;
            typed.frames += run.frames;
            typed.settled = run.settled;
        }
    }

    // Machine::disc and Machine::tape read a file only for a machine that
    // takes it.
    pub fn insert_disc(&mut self, drive: u8, contents: &[u8]) -> Result<(), String> {
        match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.insert_disc(drive, contents),
            Hardware::Spectrum(_) => unreachable!("a Spectrum is given no disc"),
        }
    }

    pub fn insert_tape(&mut self, contents: &[u8]) -> Result<(), String> {
        match &mut self.hardware {
            Hardware::Spectrum(spectrum) => spectrum.insert_tape(contents),
            Hardware::Cpc(_) => unreachable!("a CPC is given no tape"),
        }
    }

    // The machine as the emulator's command line saves it, which a machine
    // inside an instruction cannot be.
    pub fn snapshot_save(&self) -> Result<Vec<u8>, String> {
        let complete = match &self.hardware {
            Hardware::Cpc(cpc) => cpc.instruction_complete(),
            Hardware::Spectrum(spectrum) => spectrum.instruction_complete(),
        };
        if !complete {
            return Err(
                "stands inside an instruction, as a run or a step leaves it in a chain of \
                 prefixes, and a snapshot keeps no instruction half run; step it on to the \
                 instruction's end"
                    .to_owned(),
            );
        }
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.snapshot_save(),
            Hardware::Spectrum(spectrum) => spectrum.snapshot_save().map_err(|problem| {
                format!(
                    "{problem}; a Spectrum's snapshot keeps the program counter in the two bytes \
                     below the stack pointer, so cpu.sp stands from &4002 to &FFFF"
                )
            }),
        }
    }

    fn watched(
        &mut self,
        frames: u32,
        limit: u64,
        watching: &Watching,
        returning: Option<(u16, u16)>,
    ) -> (Run, Watched) {
        let conditions = watching.conditions;
        // A gathering that may hold nothing fills at its first match.
        let room = watching.most_gathered.max(1).saturating_add(ROOM_PAST_FULL);
        // SAFETY: bindgen renders a match as integers and bools, and all-zero
        // bytes are a value of each.
        let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; room as usize];
        let counting = conditions.iter().any(|condition| condition.list == COUNT);
        let size = if counting {
            KINDS.len() * COUNTS_PER_KIND
        } else {
            0
        };
        let mut counts = vec![0; size];
        let mut watch = Watch::new(
            conditions,
            &mut matches,
            &mut counts,
            watching.most_gathered,
            watching.most_counted,
        );
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
        let counts = counts
            .chunks(COUNTS_PER_KIND)
            .zip(KINDS)
            .flat_map(|(counts, kind)| {
                (0..=u16::MAX)
                    .zip(counts)
                    .filter(|(_, count)| **count > 0)
                    .map(move |(address, count)| (kind, address, *count))
            })
            .collect();
        let watched = Watched {
            matches,
            counts,
            matched: watch.matched,
            full: watch.full,
            returned: watch.returned,
            // Short of its frames and its limit, a settled run was stopped
            // by its watch or by a halt for good, which may coincide with a
            // gathering or a count filling.
            halted: run.settled
                && run.frames < frames
                && run.ticks < limit
                && self.cpu().halted_for_good(),
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
        watching: &Watching,
    ) -> Result<(Run, Watched), String> {
        let kept = (!values.is_empty()).then(|| self.keep_hardware());
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
                self.restore_hardware(&kept);
            }
            return Err(format!("cannot call &{address:04X}: {problem}"));
        }
        self.cpu_mut().halted = false;
        let (run, watched) = self.watched(frames, limit, watching, Some((pc, sp)));
        if watched.returned {
            self.cpu_mut().halted = halted;
        }
        Ok((run, watched))
    }

    fn keep_hardware(&self) -> HardwareState {
        match &self.hardware {
            Hardware::Cpc(cpc) => HardwareState::Cpc(cpc.state()),
            Hardware::Spectrum(spectrum) => HardwareState::Spectrum(spectrum.state()),
        }
    }

    fn restore_hardware(&mut self, state: &HardwareState) {
        match (&mut self.hardware, state) {
            (Hardware::Cpc(cpc), HardwareState::Cpc(state)) => cpc.restore(state),
            (Hardware::Spectrum(spectrum), HardwareState::Spectrum(state)) => {
                spectrum.restore(state)
            }
            _ => unreachable!("a state is restored into the machine it was kept from"),
        }
    }

    // What changed the machine, at the tick it began, on the branch it
    // stands on.
    pub fn record(
        &mut self,
        tick: u64,
        tool: &'static str,
        arguments: Value,
        file: Option<FileRead>,
    ) {
        self.journal.since.push(Entry {
            tick,
            tool,
            arguments,
            file,
        });
    }

    pub fn journal(&self) -> impl Iterator<Item = &Entry> {
        self.journal.entries()
    }

    pub fn journal_length(&self) -> usize {
        self.journal.len()
    }

    pub fn files(&self) -> &[FileRead] {
        &self.files
    }

    // Keeps the machine as it stands under a name, with its tick and its
    // journal.
    pub fn state_keep(&mut self, name: String) {
        let state = State {
            hardware: self.keep_hardware(),
            tick: self.tick,
            journal: self.journal.keep(),
        };
        self.states.insert(name, state);
    }

    // Puts the machine back as the state of that name kept it; the journal
    // is the state's, and what is done from here on follows it.
    pub fn state_restore(&mut self, name: &str) -> Result<(), String> {
        let (name, state) = self.states.remove_entry(name).ok_or_else(|| gone(name))?;
        self.restore_hardware(&state.hardware);
        self.tick = state.tick;
        self.journal = state.journal.clone();
        self.states.insert(name, state);
        Ok(())
    }

    pub fn state_drop(&mut self, name: &str) -> Result<(), String> {
        self.states.remove(name).map(drop).ok_or_else(|| gone(name))
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

    // A CPC's RAM by its own addresses, as screen's base counts them, past the
    // banking and the ROMs; refused on a Spectrum, whose RAM the processor
    // reaches whole.
    pub fn peek_ram(&self, ram: u32, length: u16) -> Result<Vec<u8>, String> {
        let Hardware::Cpc(cpc) = &self.hardware else {
            return Err(
                "ram reads a CPC's RAM by its own addresses, past its banking and its ROMs; a \
                 Spectrum's RAM stands at &4000 to &FFFF, where address reads it"
                    .to_owned(),
            );
        };
        let memory = cpc.ram();
        let start = ram as usize;
        let last = memory.len() - 1;
        memory
            .get(start..start + usize::from(length))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                if start > last {
                    format!("&{ram:04X} is past &{last:04X}, where the machine's RAM ends")
                } else {
                    format!(
                        "{length} bytes from &{ram:04X} run past &{last:04X}, where the \
                         machine's RAM ends; {} fit from there",
                        memory.len() - start
                    )
                }
            })
    }

    // The picture the monitor holds, cut to the window and drawn as the
    // emulator's command line draws its screenshots, each pixel's index the
    // colour the machine put on the cable.
    pub fn monitor(&self) -> Shown {
        let (window, framebuffer, samples_per_line, colours, rgb, monitor) = match &self.hardware {
            Hardware::Cpc(cpc) => (
                &CPC_WINDOW,
                cpc.framebuffer(),
                CPC_FRAMEBUFFER_WIDTH as usize,
                CPC_COLOUR_CODES,
                gate_array_rgb as fn(u8) -> u32,
                &cpc.machine().monitor,
            ),
            Hardware::Spectrum(spectrum) => (
                &SPECTRUM_WINDOW,
                spectrum.framebuffer(),
                SPECTRUM_FRAMEBUFFER_WIDTH as usize,
                SPECTRUM_SAMPLES,
                ula_rgb as fn(u8) -> u32,
                &spectrum.machine().monitor,
            ),
        };
        let repeats = if window.doubled { 2 } else { 1 };
        let mut indices = Vec::with_capacity(window.width * window.height * repeats);
        for line in 0..window.height {
            let start = (window.top + line) * samples_per_line + window.left;
            let row: Vec<u8> = framebuffer[start..start + window.width]
                .iter()
                .map(|sample| sample & (colours - 1))
                .collect();
            for _ in 0..repeats {
                indices.extend(&row);
            }
        }
        // The beam stands on the sample it paints next (monitor.c's
        // monitor_receive), so no pass is partway through the window where
        // the beam stands at or before its first sample, or past its last.
        let beam = (usize::from(monitor.beam_y), usize::from(monitor.beam_x));
        let first = (window.top, window.left);
        let last = (
            window.top + window.height - 1,
            window.left + window.width - 1,
        );
        Shown {
            picture: Picture {
                width: window.width as u32,
                height: (window.height * repeats) as u32,
                indices,
                palette: (0..colours).map(rgb).collect(),
            },
            beam_x: monitor.beam_x,
            beam_y: monitor.beam_y,
            whole: beam <= first || beam > last,
        }
    }

    // A CPC's memory read as a screen, with what was not asked for read from
    // the machine; refused on a Spectrum.
    pub fn screen(&self, asked: &Asked) -> Result<(Screen, Picture), String> {
        let Hardware::Cpc(cpc) = &self.hardware else {
            return Err(
                "screen reads a CPC's memory as its video hardware does, and a Spectrum's \
                 picture is read by monitor"
                    .to_owned(),
            );
        };
        let screen = screen::settle(asked, cpc.machine())?;
        let picture = screen::draw(&screen, cpc.ram());
        Ok((screen, picture))
    }

    fn read(&self, address: u16) -> u8 {
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.peek(address),
            Hardware::Spectrum(spectrum) => spectrum.peek(address),
        }
    }

    // Instructions read as the processor would read them, from an address
    // on, each with its address and its bytes: as many as were asked, or as
    // fit in most_bytes.
    pub fn disassemble(
        &self,
        address: u16,
        instructions: u16,
        most_bytes: usize,
    ) -> Result<Vec<(u16, Vec<u8>, String)>, String> {
        let read = |address| self.read(address);
        let mut listed = Vec::new();
        let (mut at, mut used) = (address, 0);
        for _ in 0..instructions {
            let Some(instruction) = disassembly::instruction(&read, at, most_bytes - used) else {
                break;
            };
            if used + instruction.length > most_bytes {
                break;
            }
            let bytes = (0..instruction.length as u16)
                .map(|offset| read(at.wrapping_add(offset)))
                .collect();
            listed.push((at, bytes, instruction.text));
            used += instruction.length;
            at = at.wrapping_add(instruction.length as u16);
        }
        if listed.is_empty() {
            let prefixed = (0..=u16::MAX)
                .map(|offset| address.wrapping_add(offset))
                .find(|at| !matches!(read(*at), 0xDD | 0xFD));
            return Err(match prefixed {
                Some(at) => {
                    let last = at.wrapping_sub(1);
                    format!(
                        "&{address:04X} begins a chain of DD and FD prefixes that runs on to \
                         &{last:04X}, longer than the {most_bytes} bytes a listing reads; the \
                         last of them rules the instruction they prefix, so disassemble from \
                         &{last:04X}"
                    )
                }
                None => "memory holds nothing but DD and FD prefixes, which the processor runs as \
                         one instruction without end; no listing reads one"
                    .to_owned(),
            });
        }
        Ok(listed)
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
