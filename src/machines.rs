use std::fs;
use std::path::Path;

use crate::emulator::{SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE};
use crate::machine::{Cpc, Spectrum};

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

// What a machine is built from, read and checked, so that building it
// cannot fail.
pub enum Parts {
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

pub enum Built {
    Cpc(Cpc),
    Spectrum(Spectrum),
}

fn read(roms: &Path, file: &str, size: usize) -> Result<Vec<u8>, String> {
    let path = roms.join(file);
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
    pub fn parts(&self, roms: &Path) -> Result<Parts, String> {
        match self.board {
            Board::Cpc {
                firmware,
                ram_size,
                disc_interface,
            } => {
                let firmware = read(roms, firmware, 2 * CPC_ROM_SIZE)?;
                let amsdos = if disc_interface {
                    Some(rom(&read(roms, AMSDOS, CPC_ROM_SIZE)?))
                } else {
                    None
                };
                let (lower_rom, basic) = firmware.split_at(CPC_ROM_SIZE);
                Ok(Parts::Cpc {
                    ram: vec![0; ram_size].into_boxed_slice(),
                    lower_rom: rom(lower_rom),
                    basic: rom(basic),
                    amsdos,
                })
            }
            Board::Spectrum { firmware, ram_size } => Ok(Parts::Spectrum {
                ram: vec![0; ram_size].into_boxed_slice(),
                rom: rom(&read(roms, firmware, SPECTRUM_ROM_SIZE as usize)?),
            }),
        }
    }
}

impl Parts {
    pub fn build(self) -> Built {
        match self {
            Parts::Cpc {
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
                Built::Cpc(cpc)
            }
            Parts::Spectrum { ram, rom } => Built::Spectrum(Spectrum::new(ram, rom)),
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
        named(name)
            .expect("a machine of that name")
            .parts(&roms.0)
            .expect("its firmware is read")
            .build()
    }

    fn cpc(name: &str, roms: &Roms) -> Cpc {
        match build(name, roms) {
            Built::Cpc(cpc) => cpc,
            Built::Spectrum(_) => panic!("{name} is built as a Spectrum"),
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
        let Built::Spectrum(spectrum) = build("spectrum48", &roms) else {
            panic!("spectrum48 is built as a CPC");
        };
        let machine = spectrum.machine();
        assert_eq!(machine.ram_size, SPECTRUM_RAM_48K);
        // SAFETY: as for a CPC's ROMs, at the Spectrum's size.
        let rom = unsafe { std::slice::from_raw_parts(machine.rom, SPECTRUM_ROM_SIZE as usize) };
        assert_eq!(rom, roms.contents("spectrum48.rom"));
    }

    #[test]
    fn a_cpc_without_a_disc_interface_reads_no_amsdos() {
        let roms = Roms::new(&[("cpc464.rom", 0x8000, 0x46)]);
        cpc("cpc464", &roms);
    }

    #[test]
    fn missing_firmware_says_where_it_was_looked_for_and_how_to_fetch_it() {
        let roms = Roms::new(&[("cpc6128.rom", 0x8000, 0x61)]);
        let Err(problem) = named("cpc6128").expect("the 6128").parts(&roms.0) else {
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
        let Err(problem) = named("spectrum48").expect("the 48").parts(&roms.0) else {
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
