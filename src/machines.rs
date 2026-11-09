use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::disassembly;
use crate::emulator::{
    CPC_FRAMEBUFFER_WIDTH, CPC_TICKS_PER_STANDARD_FRAME, SPECTRUM_FRAMEBUFFER_WIDTH,
    SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE, SPECTRUM_SNAPSHOT_SIZE, SPECTRUM_TICKS_PER_FRAME,
    keyboard_t, z80_t,
};
use crate::fields::{self, Fields, Segment};
use crate::firmware::{self, Version};
use crate::keys::{self, Key, Keystroke};
use crate::machine::{
    Buffers, COUNT, CRTC, CRTC_TYPES, Capture, Condition, Cpc, CpcState, KINDS, MOST_MEDIUM_BYTES,
    Match, PSG, Place, Run, SPECTRUM_LOCATIONS, Span, Spectrum, SpectrumState, Start, Watch,
    gate_array_rgb, ula_rgb,
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
        version: Version,
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
            version: Version::V1_2,
            ram_size: 0x20000,
            disc_interface: true,
        },
    },
    Machine {
        name: "cpc664",
        board: Board::Cpc {
            firmware: "cpc664.rom",
            version: Version::V1_1,
            ram_size: 0x10000,
            disc_interface: true,
        },
    },
    Machine {
        name: "cpc464",
        board: Board::Cpc {
            firmware: "cpc464.rom",
            version: Version::V1_0,
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

// The 6845 a CPC is built with where machine_open is given none: type 0, the
// first CPCs' type. The firmware programs the frame sync's width in R3, which
// types 1 and 2 ignore, holding it at 16 lines, so that "If AMSTRAD engineers
// were aware of the existence of these differences, it can be assumed that
// the first CRTC's used were type 0s, since this function is used and
// programmed by the ROM", which a 464 prototype confirmed in 2023 (the
// Compendium, ch. 4.2). The emulator's command line builds every CPC so
// (crtc_type in its machines table, cli/main.c at 78c56f2), and MAME builds
// the 464, the 664 and the 6128 with an HD6845S (amstrad_base,
// https://github.com/mamedev/mame/blob/079ac58c453b09a3ffd8267d1c611b4d2c63795b/src/mame/amstrad/amstrad.cpp#L928);
// CPCEC has built a type 1 unless told otherwise since its release of
// 20191117, "More precise emulation of type 1 CRTC (by default instead of
// 0)" (CPCEC.TXT in cpcec-20261030.zip, http://cngsoft.no-ip.org/cpcec.htm).
pub const DEFAULT_CRTC_TYPE: u8 = 0;

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
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileRead {
    /// The file's absolute path.
    pub path: PathBuf,
    /// The SHA-256 of the file's contents when the server read or wrote it, in hexadecimal.
    pub sha256: String,
}

impl FileRead {
    pub fn of(path: &Path, contents: &[u8]) -> FileRead {
        FileRead::digested(path, &Sha256::digest(contents))
    }

    fn digested(path: &Path, digest: &[u8]) -> FileRead {
        FileRead {
            path: path.to_owned(),
            sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }
}

pub const DISCS: &str = "a disc is a DSK image, of the original or the extended layout";

// Where a relative path is taken from, said beside a file that could not be
// reached by it.
fn working_directory_clause() -> String {
    match std::env::current_dir() {
        Ok(directory) => format!(
            "a relative path starts from the server's working directory, {}",
            directory.display()
        ),
        Err(error) => format!(
            "the server's working directory cannot be read ({error}), so give an absolute path"
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
            "cannot make {path:?} absolute: {error}; give a path on the server's file system, and \
             {}",
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
            "cannot read {}: {error}; give {what} by its path on the server's file system, and \
             {}",
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

// A file written only where none is, so that none is ever written over, and
// named by what it holds once it is written; what was begun there is taken
// away again if it cannot be finished. What is hashed is what the file
// took, write by write, so that the hash is of what it holds.
pub struct NewFile {
    path: PathBuf,
    file: io::BufWriter<Hashing>,
}

struct Hashing {
    file: fs::File,
    sha256: Sha256,
}

impl Write for Hashing {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.file.write(bytes)?;
        self.sha256.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl NewFile {
    pub fn create(path: &Path) -> Result<NewFile, String> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    format!(
                        "{} already exists, and files are never overwritten; give a path that \
                         names nothing yet",
                        path.display()
                    )
                } else {
                    format!(
                        "cannot write {}: {error}; give a path on the server's file system, in a \
                         directory that exists, and {}",
                        path.display(),
                        working_directory_clause()
                    )
                }
            })?;
        Ok(NewFile {
            path: path.to_owned(),
            file: io::BufWriter::new(Hashing {
                file,
                sha256: Sha256::new(),
            }),
        })
    }

    // The file, once what was written to it is on the file system, or what
    // stopped the writing.
    pub fn finish(mut self, written: io::Result<()>) -> Result<FileRead, String> {
        let finished = written
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.get_ref().file.sync_all());
        if let Err(error) = finished {
            // Dropped whole, the buffer would try again to write what it holds.
            let (hashing, _) = self.file.into_parts();
            drop(hashing);
            let begun = match fs::remove_file(&self.path) {
                Ok(()) => "the partial file has been removed",
                Err(_) => "the partial file could not be removed and is still at that path",
            };
            return Err(format!(
                "cannot write {}: {error}; {begun}",
                self.path.display()
            ));
        }
        let digest = std::mem::take(&mut self.file.get_mut().sha256).finalize();
        Ok(FileRead::digested(&self.path, &digest))
    }

    // Takes away a file made for a request refused before anything was
    // written to it.
    pub fn discard(self) {
        drop(self.file);
        let _ = fs::remove_file(&self.path);
    }
}

impl Write for NewFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub fn write_new(path: &Path, contents: &[u8]) -> Result<FileRead, String> {
    let mut file = NewFile::create(path)?;
    let written = file.write_all(contents);
    file.finish(written)
        .map_err(|problem| format!("{problem}; ask again with a path the file system can take"))
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
        crtc_type: u8,
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
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The machine's tick when the request began.
    pub tick: u64,
    /// The tool called.
    pub tool: String,
    /// The arguments the tool was given, except the handle, as the journal writes them: `address`, `poke`'s `ram`, and the conditions' addresses, ports, masks and bytes, in hexadecimal after &, but for `when`'s bytes, pairs of capital hexadecimal digits with a space between, `??` for any byte; registers as numbers; paths absolute; `crtc_type` in a CPC's `machine_open`, whether given or not.
    #[schemars(schema_with = "crate::server::values_schema")]
    pub arguments: Value,
    /// The snapshot, disc or tape the request read, with the SHA-256 of what it held.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::server::absent_not_null)]
    pub file: Option<FileRead>,
    /// The file a run or call wrote its gathered or counted lists to, with the SHA-256 of what it held.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(transform = crate::server::absent_not_null)]
    pub written: Option<FileRead>,
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

// What a state file is, said as its first field.
const STATE_FORMAT: &str = "colophon-mcp state";

// A state file's first line, a line of JSON before its buffers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heading {
    format: String,
    build: String,
    machine: String,
    tick: u64,
    files: Vec<FileRead>,
    journal: Vec<Entry>,
    hardware: Map<String, Value>,
}

// What a state file is and the build that wrote it, read before the rest of
// its first line, which another build may write otherwise; serde_json passes
// over the rest unbuilt.
#[derive(Deserialize)]
struct Named {
    format: String,
    build: String,
}

// A state file as it was read.
pub struct StateFile {
    pub read: FileRead,
    heading: Heading,
    bytes: Vec<u8>,
    buffers_at: usize,
}

impl StateFile {
    // The type the state's CPC was built with, which set cannot change, so the
    // machine it opens into is built with it too; None for a Spectrum's.
    pub fn crtc_type(&self) -> Result<Option<u8>, String> {
        let Some(machine) = self.heading.hardware.get("cpc_t") else {
            return Ok(None);
        };
        machine
            .pointer("/crtc/type")
            .and_then(Value::as_u64)
            .and_then(|crtc_type| u8::try_from(crtc_type).ok())
            .filter(|crtc_type| CRTC_TYPES.contains(crtc_type))
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "{} holds no crtc.type from {} to {}; {STATE_FILES}",
                    self.read.path.display(),
                    CRTC_TYPES.start(),
                    CRTC_TYPES.end()
                )
            })
    }
}

// A state file's first line holds the fields of two discs, some 1.2MB a disc,
// and the journal, an entry for every request that changed the machine: some
// 60 bytes for a run, some 8K for the longest poke. These leave the journal
// some 29MB, half a million runs or 3700 of the longest pokes. Parsed, a line
// of JSON took up to some 35 times its length where this was written, a
// value being 72 bytes, so these come to some 1.1GB at the most.
const MOST_HEADING_BYTES: usize = 32 * 1024 * 1024;

// The first line, then the RAM and the picture, 440K on a 6128, and the
// images of two discs, each of at most MOST_MEDIUM_BYTES and the room past it
// to format every track again, some 17MB.
const MOST_STATE_BYTES: u64 = 128 * 1024 * 1024;

// A machine's tick past which the file is refused: some 36000 years of a CPC.
const MOST_TICKS: u64 = 1 << 62;

const STATE_FILES: &str = "give a file that state_save wrote";

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
            "{} is not a file; run the emulator's `make roms`, which fetches the firmware into its \
             roms directory as files",
            path.display()
        ));
    }
    let contents = fs::read(&path).map_err(|error| {
        format!(
            "cannot read {}: {error}; run the emulator's `make roms` to fetch the firmware into \
             its roms directory, and start the server with --roms naming that directory; {}",
            path.display(),
            working_directory_clause()
        )
    })?;
    if contents.len() != size {
        return Err(format!(
            "{} has {} bytes, not {size}; the emulator's `make roms` fetches the firmware at its \
             right size",
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
    pub fn parts(
        &self,
        roms: &Path,
        snapshot: Option<&Path>,
        crtc_type: Option<u8>,
    ) -> Result<Parts, String> {
        let mut files = Vec::new();
        let memories = match self.board {
            Board::Cpc {
                firmware,
                ram_size,
                disc_interface,
                ..
            } => {
                let firmware = read(roms, firmware, 2 * CPC_ROM_SIZE, &mut files)?;
                let amsdos = if disc_interface {
                    Some(rom(&read(roms, AMSDOS, CPC_ROM_SIZE, &mut files)?))
                } else {
                    None
                };
                let (lower_rom, basic) = firmware.split_at(CPC_ROM_SIZE);
                Memories::Cpc {
                    crtc_type: crtc_type.unwrap_or(DEFAULT_CRTC_TYPE),
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
        read_whole(path, MOST_MEDIUM_BYTES as u64, "a disc", DISCS)
    }

    // A tape is held whole for as long as it is in the deck. 90 minutes of
    // tape at the Spectrum ROM's own timings, where "a reset bit is encoded as
    // two pulses of 855 tstates each, a set bit as two pulses of 1710 tstates
    // each" (comp.sys.sinclair FAQ, 16K/48K reference), hold under 1.4MB at
    // 3.5MHz, and at a CPC firmware's fastest, whose "half zero length must
    // lie between 130 and 480 microseconds" (SOFT 968, CAS SET SPEED), under
    // 3MB; the bound stands far past them.
    pub fn tape(&self, path: &Path) -> Result<Vec<u8>, String> {
        read_whole(path, MOST_MEDIUM_BYTES as u64, "a tape", self.tapes())
    }

    // What the machine's deck plays, said beside a tape refused.
    pub fn tapes(&self) -> &'static str {
        match self.board {
            Board::Cpc { .. } => "a CPC's tape is a .cdt or a .tzx",
            Board::Spectrum { .. } => {
                "a Spectrum's tape is a .tap of bare blocks at the ROM's own timings, or a .tzx"
            }
        }
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

    pub fn firmware_entry(&self, address: u16) -> Option<&'static str> {
        match self.board {
            Board::Cpc { version, .. } => firmware::entry(version, address),
            Board::Spectrum { .. } => None,
        }
    }

    // Whether a CPC's RAM holds a byte at this address in it.
    pub fn holds_ram(&self, ram: u32) -> bool {
        matches!(self.board, Board::Cpc { ram_size, .. } if (ram as usize) < ram_size)
    }

    // Whether the machine takes a disc, and what does if it does not.
    pub fn takes_discs(&self) -> Result<(), String> {
        match self.board {
            Board::Cpc {
                disc_interface: true,
                ..
            } => Ok(()),
            Board::Cpc { .. } => Err(format!(
                "a {} is built without a disc interface, as the emulator's command line builds one \
                 given no disc; use a cpc664 or a cpc6128 for discs, or tape_insert for its \
                 tapes",
                self.name
            )),
            Board::Spectrum { .. } => Err(format!(
                "a {} has no disc drive; use a cpc664 or a cpc6128 for discs, and tape_insert for \
                 a Spectrum's tapes",
                self.name
            )),
        }
    }

    // Whether the machine takes a 6845 of the type given, and what does if it
    // does not.
    pub fn takes_crtc_type(&self, crtc_type: u8) -> Result<(), String> {
        match self.board {
            Board::Cpc { .. } if CRTC_TYPES.contains(&crtc_type) => Ok(()),
            Board::Cpc { .. } => Err(format!(
                "crtc_type is {crtc_type}, and a CPC's 6845 is of type {} to {}; give one of \
                 those, or leave crtc_type out for type {DEFAULT_CRTC_TYPE}",
                CRTC_TYPES.start(),
                CRTC_TYPES.end()
            )),
            Board::Spectrum { .. } => Err(format!(
                "a {} has no 6845, so it takes no crtc_type; leave crtc_type out, or open a \
                 cpc464, a cpc664 or a cpc6128",
                self.name
            )),
        }
    }

    // Whether the machine has what the conditions watch, and what does if it
    // has not. On a Spectrum's ULA port, "a one in bit 4 activates the EAR
    // output and the internal speaker", and "The lowest three bits specify the
    // border colour" ("16K/48K reference",
    // https://worldofspectrum.org/faq/reference/48kreference.htm).
    pub fn watchable(&self, conditions: &[Condition]) -> Result<(), String> {
        match self.board {
            Board::Spectrum { .. } if conditions.iter().any(|condition| condition.kind == PSG) => {
                Err(format!(
                    "a {} has no PSG for psg to watch; its speaker is bit 4 of its ULA's port, \
                     which {{\"out\": \"&FE\", \"mask\": \"&01\"}} watches; a cpc464, a cpc664 or \
                     a cpc6128 has a PSG",
                    self.name
                ))
            }
            Board::Spectrum { .. } if conditions.iter().any(|condition| condition.kind == CRTC) => {
                Err(format!(
                    "a {} has no 6845 for crtc to watch; its border's colour is the lowest 3 bits \
                     of its ULA's port, which {{\"out\": \"&FE\", \"mask\": \"&01\"}} watches; a \
                     cpc464, a cpc664 or a cpc6128 has a 6845",
                    self.name
                ))
            }
            _ => Ok(()),
        }
    }

    // How long a frame of this machine is when nothing has reprogrammed it.
    pub fn standard_frame(&self) -> u64 {
        match self.board {
            Board::Cpc { .. } => u64::from(CPC_TICKS_PER_STANDARD_FRAME),
            Board::Spectrum { .. } => u64::from(SPECTRUM_TICKS_PER_FRAME),
        }
    }

    // A state file of this machine's, written on this build, read and its
    // heading taken apart from its buffers.
    pub fn state(&self, path: &Path) -> Result<StateFile, String> {
        let bytes = read_whole(path, MOST_STATE_BYTES, "a state file", STATE_FILES)?;
        let unread = |problem: String| format!("{} {problem}; {STATE_FILES}", path.display());
        let line = bytes
            .iter()
            .take(MOST_HEADING_BYTES + 1)
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                unread(format!(
                    "has no line break, or a first line longer than the {MOST_HEADING_BYTES} \
                     bytes a state file's first line holds"
                ))
            })?;
        let unparsed = |error: serde_json::Error| {
            unread(format!(
                "does not begin with a state file's line of JSON: {error}"
            ))
        };
        let named: Named = serde_json::from_slice(&bytes[..line]).map_err(unparsed)?;
        if named.format != STATE_FORMAT {
            return Err(unread(format!(
                "has the format {:?}, not {STATE_FORMAT:?}",
                named.format
            )));
        }
        let build = env!("COLOPHON_MCP_BUILD");
        if named.build != build {
            return Err(format!(
                "{} was written by build {}, not by this server's build {build}; a state file \
                 opens only on the build that wrote it, so on this build make the requests in \
                 its journal again, from the files the journal names; the journal is in the \
                 file's first line, as JSON",
                path.display(),
                named.build
            ));
        }
        let heading: Heading = serde_json::from_slice(&bytes[..line]).map_err(unparsed)?;
        if heading.machine != self.name {
            return Err(format!(
                "{} holds a {}; open it with machine {}",
                path.display(),
                heading.machine,
                heading.machine
            ));
        }
        Ok(StateFile {
            read: FileRead::of(path, &bytes),
            heading,
            bytes,
            buffers_at: line + 1,
        })
    }

    // What machine_open would take, said when a snapshot is refused.
    fn snapshots(&self) -> &'static str {
        match self.board {
            Board::Cpc { .. } => {
                "a CPC opens an SNA snapshot of version 1, 2 or 3 whose memory dump, after its \
                 header, holds no more RAM than the machine has: 128K on a cpc6128, 64K on a \
                 cpc664 or a cpc464, which have no PAL and take only RAM configuration 0"
            }
            Board::Spectrum { .. } => "a spectrum48 opens a 48K .sna snapshot of 49179 bytes",
        }
    }
}

impl Parts {
    pub fn crtc_type(&self) -> Option<u8> {
        match self.memories {
            Memories::Cpc { crtc_type, .. } => Some(crtc_type),
            Memories::Spectrum { .. } => None,
        }
    }

    pub fn build(self, machine: &Machine) -> Result<Built, String> {
        let mut opening = Entry {
            tick: 0,
            tool: "machine_open".to_owned(),
            arguments: json!({ "machine": machine.name }),
            file: None,
            written: None,
        };
        let mut hardware = match self.memories {
            Memories::Cpc {
                crtc_type,
                ram,
                lower_rom,
                basic,
                amsdos,
            } => {
                opening.arguments["crtc_type"] = json!(crtc_type);
                let mut cpc = Cpc::new(ram, lower_rom, crtc_type);
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
// counts before it stops; where it gathers the last matches, how many of
// them it keeps instead. A count with no most_counted never fills, and
// keeps every place. Every capture takes the spans.
pub struct Watching<'a> {
    pub conditions: &'a [Condition],
    pub spans: &'a [Span],
    pub most_gathered: u32,
    pub gathers_last: bool,
    pub most_counted: Option<u32>,
    pub counts_by_pc: bool,
}

// What a run saw, when it watched: every match, in the order it happened,
// or the last it gathered, its tick counted from the machine's start, and,
// where a condition captures, the slot each was kept in, whose capture is its
// own; every place counted, by kind, then by address or pc, then by location,
// with how many times it matched; whether the machine pages ROMs and RAM, so
// that an address does not say which holds a byte; and whether the processor
// halting for good stopped it, which a run watched or not may.
pub struct Watched {
    pub matches: Vec<Match>,
    pub slots: Vec<u32>,
    pub captures: Vec<Capture>,
    pub counts: Vec<(Place, u32)>,
    pub paged: bool,
    pub matched: bool,
    pub full: bool,
    pub returned: bool,
    pub halted: bool,
}

// Past the match that fills a gathering or a count, a run sees out the
// instruction under way, which makes four matches at most (EX (SP),IX's
// accesses; OUTI to a port that reaches both the 6845 and the PPI makes four
// too, a read, a port written, and a 6845 register and a PSG register written
// through it), then the response to an interrupt taken at its end,
// four more (in mode 2), and the instruction it stops before. The last nine
// matches therefore hold every match of until, which stops the run at the
// first.
const ROOM_PAST_FULL: u32 = 9;

// A state named when it was asked for and dropped before the asking reached
// the machine.
fn gone(name: &str) -> String {
    format!(
        "{name} was dropped while this request waited; machine_list lists the states kept from \
         each machine"
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

    fn locations(&self) -> u32 {
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.locations(),
            Hardware::Spectrum(_) => SPECTRUM_LOCATIONS,
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
            spans: &[],
            most_gathered: 0,
            gathers_last: false,
            most_counted: Some(0),
            counts_by_pc: false,
        };
        while typed.settled && typed.ticks < until {
            let (run, _) = self.watched(u32::MAX, until - typed.ticks, &nothing, None);
            typed.ticks += run.ticks;
            typed.frames += run.frames;
            typed.settled = run.settled;
        }
    }

    // Machine::disc reads a file only for a machine that takes discs.
    pub fn insert_disc(
        &mut self,
        drive: u8,
        contents: &[u8],
        write_protected: bool,
    ) -> Result<(), String> {
        match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.insert_disc(drive, contents, write_protected),
            Hardware::Spectrum(_) => unreachable!("a Spectrum is given no disc"),
        }
    }

    // The server asks Machine::takes_discs before it saves a disc.
    pub fn disc_save(&self, drive: u8) -> Result<(Vec<u8>, bool), String> {
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.disc_save(drive),
            Hardware::Spectrum(_) => unreachable!("a Spectrum is given no disc"),
        }
    }

    pub fn insert_tape(&mut self, contents: &[u8]) -> Result<(), String> {
        match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.insert_tape(contents),
            Hardware::Spectrum(spectrum) => spectrum.insert_tape(contents),
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
                "is inside an instruction, where a run or a step can leave it in a chain of \
                 prefixes, and a snapshot cannot hold a half-run instruction; step to the \
                 instruction's end first"
                    .to_owned(),
            );
        }
        match &self.hardware {
            Hardware::Cpc(cpc) => cpc.snapshot_save(),
            Hardware::Spectrum(spectrum) => spectrum.snapshot_save().map_err(|problem| {
                format!(
                    "{problem}; a Spectrum snapshot stores the program counter in the 2 bytes \
                     below the stack pointer, so cpu.sp must be &4002 to &FFFF"
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
        // Room for the matches a gathering to a file keeps, some 56MB, is
        // asked for zeroed rather than filled, as a count's places are below.
        // SAFETY: bindgen renders a match and a place as integers, bools and
        // arrays of them, and all-zero bytes are a value of each.
        let mut matches =
            unsafe { Box::<[Match]>::new_zeroed_slice(room as usize).assume_init() }.into_vec();
        let counting = conditions.iter().any(|condition| condition.list == COUNT);
        let locations = self.locations() as usize;
        let every_place = KINDS.len() * locations;
        let (size, place_room, most_counted) = match (counting, watching.most_counted) {
            (false, _) => (0, 0, 0),
            (true, Some(most)) => (every_place, most.saturating_add(ROOM_PAST_FULL), most),
            (true, None) => (every_place, every_place as u32, u32::MAX),
        };
        let mut counts = vec![0; size];
        // Room for every place a count to a file or by range can reach,
        // 1949696 on a 6128, is asked for zeroed rather than filled, so that
        // room no place is kept in is never written.
        // SAFETY: as for a match.
        let mut places =
            unsafe { Box::<[Place]>::new_zeroed_slice(place_room as usize).assume_init() }
                .into_vec();
        // Room for a capture beside each match a gathering to a file keeps,
        // some 140MB, is asked for zeroed too, and stays where the run left
        // it: each match kept says by its slot which capture is its own.
        let capturing = conditions.iter().any(|condition| condition.captures);
        // SAFETY: bindgen renders a capture as integers, bools, and arrays and
        // structures of them, and all-zero bytes are a value of each.
        let mut captures = unsafe {
            Box::<[Capture]>::new_zeroed_slice(if capturing { room as usize } else { 0 })
                .assume_init()
        }
        .into_vec();
        let mut watch = Watch::new(
            conditions,
            &mut matches,
            &mut counts,
            &mut places,
            watching.most_gathered,
            most_counted,
        );
        if capturing {
            watch.capture(&mut captures, watching.spans);
        }
        if watching.gathers_last {
            watch.gather_last();
        }
        if watching.counts_by_pc {
            watch.count_by_pc();
        }
        if let Some((pc, sp)) = returning {
            watch.returning_to(pc, sp);
        }
        let watching_anything = !conditions.is_empty() || returning.is_some();
        let run = match &mut self.hardware {
            Hardware::Cpc(cpc) => cpc.run(frames, limit, watching_anything.then_some(&mut watch)),
            Hardware::Spectrum(spectrum) => {
                spectrum.run(frames, limit, watching_anything.then_some(&mut watch))
            }
        };
        let watch = *watch.seen();
        let mut slots: Vec<u32> = if capturing {
            (0..room).collect()
        } else {
            Vec::new()
        };
        if watching.gathers_last && watch.match_count > room {
            let first = (watch.match_count % room) as usize;
            matches.rotate_left(first);
            if capturing {
                slots.rotate_left(first);
            }
        } else {
            assert!(
                watch.match_count <= room,
                "a run saw {} matches, past the {room} there was room for",
                watch.match_count
            );
            matches.truncate(watch.match_count as usize);
            slots.truncate(watch.match_count as usize);
        }
        if watching.gathers_last {
            // Of the matches kept, those gathered before the last
            // most_gathered are dropped.
            let gathered = matches.iter().filter(|seen| seen.gathers).count();
            let mut surplus = gathered.saturating_sub(watching.most_gathered as usize);
            let kept: Vec<bool> = matches
                .iter_mut()
                .map(|seen| {
                    if seen.gathers && surplus > 0 {
                        surplus -= 1;
                        seen.gathers = false;
                    }
                    seen.stops || seen.gathers
                })
                .collect();
            let mut keeping = kept.iter();
            matches.retain(|_| keeping.next() == Some(&true));
            let mut keeping = kept.iter();
            slots.retain(|_| keeping.next() == Some(&true));
        }
        // A run ends unsettled only inside a chain of prefixes, before the end
        // its instruction's matches capture at; a chain makes no access, so
        // only an instruction set wrote into the processor's machinery has
        // made one by then.
        assert!(
            !run.settled || matches.iter().all(|seen| seen.captured == seen.captures),
            "a match that captures has captured by the end of its instruction"
        );
        for seen in &mut matches {
            seen.tick += self.tick;
        }
        assert!(
            watch.counted <= place_room,
            "a run counted at {} places, past the {place_room} there was room for",
            watch.counted
        );
        let place = |kept: &Place| {
            let first = &kept.first;
            if watching.counts_by_pc {
                (first.kind, first.pc, first.pc_location)
            } else {
                (first.kind, first.address, first.location)
            }
        };
        places.truncate(watch.counted as usize);
        places.sort_by_key(place);
        let counted = places
            .into_iter()
            .map(|mut kept| {
                let (kind, _, location) = place(&kept);
                assert!(
                    (location as usize) < locations,
                    "a place counted at location {location:#X}, past the {locations:#X} the \
                     machine has"
                );
                kept.first.tick += self.tick;
                (kept, counts[kind as usize * locations + location as usize])
            })
            .collect();
        self.tick += run.ticks;
        let watched = Watched {
            matches,
            slots,
            captures,
            counts: counted,
            paged: matches!(self.hardware, Hardware::Cpc(_)),
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
        written: Option<FileRead>,
    ) {
        self.journal.since.push(Entry {
            tick,
            tool: tool.to_owned(),
            arguments,
            file,
            written,
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

    // The state of that name as a file holds it, and its tick: a line of JSON,
    // then the buffers its hardware names.
    pub fn state_file(&self, name: &str, machine: &Machine) -> Result<(Vec<u8>, u64), String> {
        let state = self.states.get(name).ok_or_else(|| gone(name))?;
        let filed = match &state.hardware {
            HardwareState::Cpc(state) => state.filed(),
            HardwareState::Spectrum(state) => state.filed(),
        };
        let heading = Heading {
            format: STATE_FORMAT.to_owned(),
            build: env!("COLOPHON_MCP_BUILD").to_owned(),
            machine: machine.name.to_owned(),
            tick: state.tick,
            files: self.files.clone(),
            journal: state.journal.entries().cloned().collect(),
            hardware: filed.fields,
        };
        // serde_json's compact output holds no newline byte: one inside a
        // string is escaped.
        let mut bytes = serde_json::to_vec(&heading).map_err(|error| {
            format!("{name} cannot be written as JSON: {error}; give the server paths in UTF-8")
        })?;
        if bytes.len() > MOST_HEADING_BYTES {
            return Err(format!(
                "{name} makes a first line of {} bytes, larger than the {MOST_HEADING_BYTES} a \
                 state file's first line holds, most of it the journal of {} entries; the journal \
                 tool reads the journal, and its requests rebuild the machine",
                bytes.len(),
                heading.journal.len()
            ));
        }
        bytes.push(b'\n');
        for buffer in filed.buffers {
            bytes.extend_from_slice(buffer);
        }
        Ok((bytes, state.tick))
    }

    // Stands the machine, as built, as the state file holds it, with its
    // tick, the files it was built from and its journal; refused where the
    // firmware differs from the state's, by its hash.
    pub fn state_file_load(&mut self, state: StateFile) -> Result<(), String> {
        let heading = state.heading;
        if heading.tick > MOST_TICKS {
            return Err(format!(
                "holds a tick of {}, larger than the {MOST_TICKS} a state file holds; \
                 {STATE_FILES}",
                heading.tick
            ));
        }
        let snapshots = heading.files.len().checked_sub(self.files.len());
        if !matches!(snapshots, Some(0 | 1)) {
            return Err(format!(
                "names {} files the machine was built from, where a state of this machine names \
                 {} or {}: its firmware, and at most 1 snapshot; {STATE_FILES}",
                heading.files.len(),
                self.files.len(),
                self.files.len() + 1
            ));
        }
        let agrees = self
            .files
            .iter()
            .zip(&heading.files)
            .all(|(read, kept)| read.sha256 == kept.sha256);
        if !agrees {
            let named = |files: &[FileRead]| {
                files
                    .iter()
                    .map(|file| format!("{} ({})", file.path.display(), file.sha256))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(format!(
                "was kept on other firmware: the state's is {}, and this server's is {}; start \
                 the server with --roms naming a directory that holds the state's firmware",
                named(&heading.files[..self.files.len()]),
                named(&self.files)
            ));
        }
        let mut buffers = Buffers::new(&state.bytes[state.buffers_at..]);
        match &mut self.hardware {
            Hardware::Cpc(cpc) => {
                cpc.state_file_load(&heading.hardware, heading.tick, &mut buffers)
            }
            Hardware::Spectrum(spectrum) => {
                spectrum.state_file_load(&heading.hardware, heading.tick, &mut buffers)
            }
        }
        .map_err(|problem| {
            format!(
                "does not hold a machine this server can build: {problem}; make the machine again \
                 from the requests in the journal in its first line"
            )
        })?;
        if buffers.left() > 0 {
            return Err(format!(
                "holds {} bytes past the buffers its first line names; {STATE_FILES}",
                buffers.left()
            ));
        }
        self.tick = heading.tick;
        self.files = heading.files;
        self.journal = Journal {
            kept: Vec::new(),
            since: heading.journal,
        };
        Ok(())
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
    // reaches whole, with how to `verb` it there instead.
    fn cpc_ram(&self, verb: &str) -> Result<&[u8], String> {
        match &self.hardware {
            Hardware::Cpc(cpc) => Ok(cpc.ram()),
            Hardware::Spectrum(_) => Err(format!(
                "ram names a CPC's RAM by its own addresses; a Spectrum's RAM is at &4000 to \
                 &FFFF, so {verb} it with address"
            )),
        }
    }

    pub fn peek_ram(&self, ram: u32, length: u16) -> Result<Vec<u8>, String> {
        let memory = self.cpc_ram("read")?;
        Ok(memory[ram_span(memory, ram, usize::from(length))?].to_vec())
    }

    pub fn compared_ram(&self) -> Result<&[u8], String> {
        match &self.hardware {
            Hardware::Cpc(cpc) => Ok(cpc.ram()),
            Hardware::Spectrum(_) => Err(UNCOMPARED.to_owned()),
        }
    }

    pub fn state_ram(&self, name: &str) -> Result<&[u8], String> {
        match &self.states.get(name).ok_or_else(|| gone(name))?.hardware {
            HardwareState::Cpc(state) => Ok(state.ram()),
            HardwareState::Spectrum(_) => Err(UNCOMPARED.to_owned()),
        }
    }

    pub fn poke_ram(&mut self, ram: u32, bytes: &[u8]) -> Result<(), String> {
        let span = ram_span(self.cpc_ram("write")?, ram, bytes.len())?;
        if let Hardware::Cpc(cpc) = &mut self.hardware {
            cpc.ram_mut()[span].copy_from_slice(bytes);
        }
        Ok(())
    }

    // The location a CPC's processor reaches at an address as it stands. A
    // Spectrum's memory is never paged, so its address alone says where.
    pub fn locate(&self, address: u16, writing: bool) -> Option<u32> {
        match &self.hardware {
            Hardware::Cpc(cpc) => Some(cpc.locate(address, writing)),
            Hardware::Spectrum(_) => None,
        }
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
                "screen draws a CPC's memory only; use monitor for a Spectrum's picture".to_owned(),
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

    pub fn start(&self, start: Start, address: u16) -> u16 {
        self.cpu().start(start, address)
    }

    // Where each match of the pattern starts, lowest first, in memory read as
    // the processor reads it from first to last, no match running past last.
    pub fn find(&self, first: u16, last: u16, pattern: &[(u8, u8)]) -> Result<Vec<u16>, String> {
        in_order(first.into(), last.into(), "address")?;
        let memory: Vec<u8> = (first..=last).map(|address| self.read(address)).collect();
        Ok(matching(&memory, pattern)
            .map(|offset| first + offset as u16)
            .collect())
    }

    // The same in a CPC's RAM by its own addresses, to the RAM's end unless
    // `last` is given.
    pub fn find_ram(
        &self,
        first: u32,
        last: Option<u32>,
        pattern: &[(u8, u8)],
    ) -> Result<Vec<u32>, String> {
        let memory = self.cpc_ram("search")?;
        within(memory, first, "ram")?;
        let last = last.unwrap_or(memory.len() as u32 - 1);
        within(memory, last, "to")?;
        in_order(first, last, "ram")?;
        Ok(matching(&memory[first as usize..=last as usize], pattern)
            .map(|offset| first + offset as u32)
            .collect())
    }

    // Instructions as the processor reads them, from an address on.
    pub fn disassemble(
        &self,
        address: u16,
        instructions: u16,
        most_bytes: usize,
    ) -> Result<Vec<(u32, Vec<u8>, disassembly::Instruction)>, String> {
        let read = |address| self.read(address);
        let rom = self.lower_rom();
        listing(&read, rom, None, address, instructions, most_bytes)
    }

    // The same in a CPC's RAM by its own addresses: the 64K that holds `ram`,
    // listed as the processor's view is when that 64K is all it reads, the
    // ROMs disabled and the base 64K under RAM configuration 0 or a 6128's
    // second under 2, each instruction given its address in the RAM.
    pub fn disassemble_ram(
        &self,
        ram: u32,
        instructions: u16,
        most_bytes: usize,
    ) -> Result<Vec<(u32, Vec<u8>, disassembly::Instruction)>, String> {
        let memory = self.cpc_ram("disassemble")?;
        within(memory, ram, "ram")?;
        let base = ram & !0xFFFF;
        let window = &memory[base as usize..][..0x10000];
        let read = |address: u16| window[usize::from(address)];
        let rom = self.lower_rom();
        listing(&read, rom, Some(base), ram as u16, instructions, most_bytes)
    }

    fn lower_rom(&self) -> Option<&[u8; 0x4000]> {
        match &self.hardware {
            Hardware::Cpc(cpc) => Some(cpc.lower_rom()),
            Hardware::Spectrum(_) => None,
        }
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

// A refusal of an address in the RAM, given as `named`, past its end.
fn within(memory: &[u8], ram: u32, named: &str) -> Result<(), String> {
    let last = memory.len() - 1;
    if ram as usize > last {
        return Err(format!(
            "{named} &{ram:04X} is past &{last:04X}, the end of the RAM; give a {named} of \
             &{last:04X} or below"
        ));
    }
    Ok(())
}

// The range of `length` bytes from `ram` in the RAM, refused where it runs past
// the RAM's end.
fn ram_span(memory: &[u8], ram: u32, length: usize) -> Result<Range<usize>, String> {
    within(memory, ram, "ram")?;
    let start = ram as usize;
    if memory.len() - start < length {
        return Err(format!(
            "{length} bytes from &{ram:04X} run past &{:04X}, the end of the RAM; give at most {} \
             from there",
            memory.len() - 1,
            memory.len() - start
        ));
    }
    Ok(start..start + length)
}

const UNCOMPARED: &str = "compare reads a CPC's RAM only; a Spectrum's RAM is at &4000 to &FFFF, \
                          so peek it there in each machine and compare the bytes, and to compare \
                          with a state, keep the machine with state_keep, peek it, then \
                          state_restore the state, peek it again, and state_restore the kept \
                          state to go back";

// The runs of neighbouring addresses at which two RAMs differ, lowest first,
// from `first` to `last`, or to the last address both hold.
pub fn differences(
    ours: &[u8],
    theirs: &[u8],
    first: u32,
    last: Option<u32>,
) -> Result<Vec<Range<usize>>, String> {
    let held = ours.len().min(theirs.len()) as u32 - 1;
    let end = if ours.len() == theirs.len() {
        "the end of the RAM"
    } else {
        "the end of the smaller RAM"
    };
    for (named, address) in [("ram", Some(first)), ("to", last)] {
        if let Some(address) = address.filter(|address| *address > held) {
            return Err(format!(
                "{named} &{address:04X} is past &{held:04X}, {end}; give a {named} of &{held:04X} \
                 or below"
            ));
        }
    }
    let last = last.unwrap_or(held);
    in_order(first, last, "ram")?;
    let differing: Vec<usize> = (first as usize..=last as usize)
        .filter(|&address| ours[address] != theirs[address])
        .collect();
    Ok(differing
        .chunk_by(|address, next| *next == address + 1)
        .map(|run| run[0]..run[run.len() - 1] + 1)
        .collect())
}

// A refusal of a range whose last address, `to`, is below its first, given
// as `named`.
fn in_order(first: u32, last: u32, named: &str) -> Result<(), String> {
    if last < first {
        return Err(format!(
            "to &{last:04X} is below {named} &{first:04X}; give a to of &{first:04X} or above"
        ));
    }
    Ok(())
}

// Where each match of the pattern starts in memory, by its offset, lowest
// first, no match running past memory's end.
fn matching<'a>(memory: &'a [u8], pattern: &'a [(u8, u8)]) -> impl Iterator<Item = usize> + 'a {
    assert!(!pattern.is_empty(), "a pattern of a byte at least");
    memory
        .windows(pattern.len())
        .enumerate()
        .filter(move |(_, bytes)| {
            bytes
                .iter()
                .zip(pattern)
                .all(|(byte, (value, mask))| byte & mask == *value)
        })
        .map(|(offset, _)| offset)
}

// Instructions from `address` on, read through `read`, a 64K that wraps
// past &FFFF as the processor's view does, each with its address and its
// bytes: as many as were asked, or as fit in most_bytes. A CPC gives its
// `lower_rom`, and a firmware restart that takes an address is listed with
// the word after it, the two or neither, the word not counted among the
// instructions. A listing of a CPC's RAM gives `ram_base`, where its 64K
// starts in the RAM: its addresses count from there, and its refusals speak
// of the RAM.
fn listing(
    read: &dyn Fn(u16) -> u8,
    lower_rom: Option<&[u8; 0x4000]>,
    ram_base: Option<u32>,
    address: u16,
    instructions: u16,
    most_bytes: usize,
) -> Result<Vec<(u32, Vec<u8>, disassembly::Instruction)>, String> {
    let base = ram_base.unwrap_or(0);
    let mut listed = Vec::new();
    let (mut at, mut used) = (address, 0);
    for _ in 0..instructions {
        let Some(instruction) = disassembly::instruction(read, at, most_bytes - used) else {
            break;
        };
        let after = at.wrapping_add(instruction.length as u16);
        let word = lower_rom
            .filter(|rom| takes_an_address(read, rom, &instruction))
            .map(|_| disassembly::word_of_data(read, after));
        let length = instruction.length + word.as_ref().map_or(0, |word| word.length);
        if used + length > most_bytes {
            break;
        }
        for entry in std::iter::once(instruction).chain(word) {
            let bytes = (0..entry.length as u16)
                .map(|offset| read(at.wrapping_add(offset)))
                .collect();
            let next = at.wrapping_add(entry.length as u16);
            listed.push((base + u32::from(at), bytes, entry));
            at = next;
        }
        used += length;
    }
    if listed.is_empty() {
        let prefixed = (0..=u16::MAX)
            .map(|offset| address.wrapping_add(offset))
            .find(|at| !matches!(read(*at), 0xDD | 0xFD));
        let first = base + u32::from(address);
        let form = if ram_base.is_some() { "with ram " } else { "" };
        return Err(match prefixed {
            Some(at) => {
                let last = base + u32::from(at.wrapping_sub(1));
                format!(
                    "&{first:04X} starts a chain of DD and FD prefixes running to &{last:04X}, \
                     which with the instruction it prefixes is longer than the {most_bytes} bytes \
                     a listing reads; the last prefix decides the instruction, so disassemble \
                     {form}from &{last:04X}"
                )
            }
            None => {
                let held = match ram_base {
                    Some(base) => format!("&{base:04X} to &{:04X} of the RAM", base + 0xFFFF),
                    None => "memory".to_owned(),
                };
                format!(
                    "{held} holds nothing but DD and FD prefixes, which the processor runs as one \
                     endless instruction; there is nothing to list"
                )
            }
        });
    }
    Ok(listed)
}

// Whether an instruction is a restart that takes the two bytes after it as
// an address, as the CPC firmware's LOW JUMP, SIDE CALL, FAR CALL and FIRM
// JUMP do (SOFT 968, 2.3, https://archive.org/details/SOFT968TheAmstrad6128FirmwareManual):
// only while the first three bytes at the restart, read through `read`, are
// the lower ROM's JP, which the RAM beneath the ROM holds too, the area being
// "set up so that the restarts operate whatever the current ROM state is". A
// program that disables the ROM may put its own restart there, as Aliens
// makes RST &28 its key test.
fn takes_an_address(
    read: &dyn Fn(u16) -> u8,
    lower_rom: &[u8; 0x4000],
    instruction: &disassembly::Instruction,
) -> bool {
    let Some(restart @ (0x08 | 0x10 | 0x18 | 0x28)) = instruction.target else {
        return false;
    };
    instruction.text.starts_with("RST ")
        && (restart..restart + 3).all(|address| read(address) == lower_rom[usize::from(address)])
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
            .parts(&roms.0, None, None)
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
                    ..
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
                ..
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

    // A state whose journal takes its first line past MOST_HEADING_BYTES is
    // not written, since no build would open the file.
    #[test]
    fn a_state_too_long_to_open_is_not_written() {
        let roms = Roms::new(STAND_INS);
        let mut built = build("spectrum48", &roms);
        let arguments = json!({ "bytes": "00".repeat(MOST_HEADING_BYTES / 16) });
        for _ in 0..8 {
            built.record(0, "poke", arguments.clone(), None, None);
        }
        built.state_keep("long".to_owned());
        let refused = built.state_file("long", named("spectrum48").unwrap());
        let problem = refused.expect_err("a first line past the bound");
        assert!(
            problem.starts_with("long makes a first line of ")
                && problem.ends_with(
                    "bytes, larger than the 33554432 a state file's first line holds, most of it \
                     the journal of 9 entries; the journal tool reads the journal, and its \
                     requests rebuild the machine"
                ),
            "{problem}"
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
        let Err(problem) = named("cpc6128")
            .expect("the 6128")
            .parts(&roms.0, None, None)
        else {
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
        let Err(problem) = named("spectrum48")
            .expect("the 48")
            .parts(&roms.0, None, None)
        else {
            panic!("a 32K Spectrum ROM is taken");
        };
        assert_eq!(
            problem,
            format!(
                "{} has 32768 bytes, not 16384; the emulator's `make roms` fetches the firmware at \
                 its right size",
                roms.0.join("spectrum48.rom").display()
            )
        );
    }
}
