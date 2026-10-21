// The decomposition of "Decoding Z80 Opcodes" (Cristian Dinu),
// http://z80.info/decoding.htm: x, y and z cut from the opcode, p and q from
// y, and these tables read with them. The emulator's z80.c decodes by the same
// cuts, and the names are those colophon-player's z80_disassemble.js gives.
const R: [&str; 8] = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
const RP: [&str; 4] = ["BC", "DE", "HL", "SP"];
const RP2: [&str; 4] = ["BC", "DE", "HL", "AF"];
const CC: [&str; 8] = ["NZ", "Z", "NC", "C", "PO", "PE", "P", "M"];
const ALU: [&str; 8] = [
    "ADD A,", "ADC A,", "SUB ", "SBC A,", "AND ", "XOR ", "OR ", "CP ",
];
const ROT: [&str; 8] = ["RLC", "RRC", "RL", "RR", "SLA", "SRA", "SLL", "SRL"];
const IM: [&str; 8] = ["0", "0/1", "1", "2", "0", "0/1", "1", "2"];
const ACCUMULATOR: [&str; 8] = ["RLCA", "RRCA", "RLA", "RRA", "DAA", "CPL", "SCF", "CCF"];
const INTERRUPT: [&str; 8] = [
    "LD I,A", "LD R,A", "LD A,I", "LD A,R", "RRD", "RLD", "NOP", "NOP",
];
const BLI: [[&str; 4]; 4] = [
    ["LDI", "CPI", "INI", "OUTI"],
    ["LDD", "CPD", "IND", "OUTD"],
    ["LDIR", "CPIR", "INIR", "OTIR"],
    ["LDDR", "CPDR", "INDR", "OTDR"],
];

pub struct Instruction {
    pub length: usize,
    pub text: String,
}

struct Cuts {
    x: u8,
    y: u8,
    z: u8,
    p: u8,
    q: u8,
}

fn cuts(opcode: u8) -> Cuts {
    let y = (opcode >> 3) & 7;
    Cuts {
        x: opcode >> 6,
        y,
        z: opcode & 7,
        p: y >> 1,
        q: y & 1,
    }
}

fn byte(value: u8) -> String {
    format!("&{value:02X}")
}

fn word(value: u16) -> String {
    format!("&{value:04X}")
}

struct Decoding<'a> {
    read: &'a dyn Fn(u16) -> u8,
    address: u16,
    length: usize,
    index: Option<&'static str>,
    displacement: Option<i8>,
}

impl Decoding<'_> {
    fn next(&mut self) -> u8 {
        let value = (self.read)(self.address.wrapping_add(self.length as u16));
        self.length += 1;
        value
    }

    fn word(&mut self) -> u16 {
        u16::from_le_bytes([self.next(), self.next()])
    }

    // At most one of these per instruction, and under DD or FD followed by CB
    // it is read before the opcode that uses it.
    fn offset(&mut self) -> String {
        let displacement = match self.displacement {
            Some(displacement) => displacement,
            None => {
                let displacement = self.next().cast_signed();
                self.displacement = Some(displacement);
                displacement
            }
        };
        let sign = if displacement < 0 { '-' } else { '+' };
        let index = self.index.unwrap_or("HL");
        format!("({index}{sign}{})", byte(displacement.unsigned_abs()))
    }

    // The target is counted from the end of the instruction, where the
    // processor's PC then stands.
    fn relative(&mut self) -> String {
        let step = i16::from(self.next().cast_signed());
        let end = self.address.wrapping_add(self.length as u16);
        word(end.wrapping_add_signed(step))
    }

    fn operand(&mut self, number: u8) -> String {
        match (self.index, number) {
            (Some(index), 4 | 5) => format!("{index}{}", R[usize::from(number)]),
            (Some(_), 6) => self.offset(),
            _ => R[usize::from(number)].to_owned(),
        }
    }

    fn pair(&self, table: &[&'static str; 4], number: u8) -> &'static str {
        match (table[usize::from(number)], self.index) {
            ("HL", Some(index)) => index,
            (name, _) => name,
        }
    }

    fn hl(&self) -> &'static str {
        self.index.unwrap_or("HL")
    }

    fn main(&mut self, opcode: u8) -> String {
        let Cuts { x, y, z, p, q } = cuts(opcode);
        match x {
            0 => self.x0(y, z, p, q),
            1 => self.load(y, z),
            2 => format!("{}{}", ALU[usize::from(y)], self.operand(z)),
            _ => self.x3(y, z, p, q),
        }
    }

    fn x0(&mut self, y: u8, z: u8, p: u8, q: u8) -> String {
        match z {
            0 => match y {
                0 => "NOP".to_owned(),
                1 => "EX AF,AF'".to_owned(),
                2 => format!("DJNZ {}", self.relative()),
                3 => format!("JR {}", self.relative()),
                _ => format!("JR {},{}", CC[usize::from(y - 4)], self.relative()),
            },
            1 if q == 0 => format!("LD {},{}", self.pair(&RP, p), word(self.word())),
            1 => format!("ADD {},{}", self.hl(), self.pair(&RP, p)),
            2 => self.indirect_load(p, q),
            3 => format!(
                "{} {}",
                if q == 0 { "INC" } else { "DEC" },
                self.pair(&RP, p)
            ),
            4 => format!("INC {}", self.operand(y)),
            5 => format!("DEC {}", self.operand(y)),
            6 => {
                let target = self.operand(y);
                format!("LD {target},{}", byte(self.next()))
            }
            _ => ACCUMULATOR[usize::from(y)].to_owned(),
        }
    }

    fn indirect_load(&mut self, p: u8, q: u8) -> String {
        match (p, q) {
            (0, 0) => "LD (BC),A".to_owned(),
            (0, _) => "LD A,(BC)".to_owned(),
            (1, 0) => "LD (DE),A".to_owned(),
            (1, _) => "LD A,(DE)".to_owned(),
            (2, 0) => format!("LD ({}),{}", word(self.word()), self.hl()),
            (2, _) => format!("LD {},({})", self.hl(), word(self.word())),
            (_, 0) => format!("LD ({}),A", word(self.word())),
            _ => format!("LD A,({})", word(self.word())),
        }
    }

    fn load(&mut self, y: u8, z: u8) -> String {
        if y == 6 && z == 6 {
            return "HALT".to_owned();
        }
        // The register beside an (IX+d) stays itself: only one of the two can
        // be indexed, so the other never becomes IXH or IXL.
        if self.index.is_some() && (y == 6 || z == 6) {
            let target = if y == 6 {
                self.offset()
            } else {
                R[usize::from(y)].to_owned()
            };
            let source = if z == 6 {
                self.offset()
            } else {
                R[usize::from(z)].to_owned()
            };
            return format!("LD {target},{source}");
        }
        format!("LD {},{}", self.operand(y), self.operand(z))
    }

    fn x3(&mut self, y: u8, z: u8, p: u8, q: u8) -> String {
        match z {
            0 => format!("RET {}", CC[usize::from(y)]),
            1 if q == 0 => format!("POP {}", self.pair(&RP2, p)),
            1 => match p {
                0 => "RET".to_owned(),
                1 => "EXX".to_owned(),
                2 => format!("JP ({})", self.hl()),
                _ => format!("LD SP,{}", self.hl()),
            },
            2 => format!("JP {},{}", CC[usize::from(y)], word(self.word())),
            3 => match y {
                0 => format!("JP {}", word(self.word())),
                2 => format!("OUT ({}),A", byte(self.next())),
                3 => format!("IN A,({})", byte(self.next())),
                4 => format!("EX (SP),{}", self.hl()),
                5 => "EX DE,HL".to_owned(),
                6 => "DI".to_owned(),
                _ => "EI".to_owned(),
            },
            4 => format!("CALL {},{}", CC[usize::from(y)], word(self.word())),
            5 if q == 0 => format!("PUSH {}", self.pair(&RP2, p)),
            5 => format!("CALL {}", word(self.word())),
            6 => format!("{}{}", ALU[usize::from(y)], byte(self.next())),
            _ => format!("RST {}", byte(y * 8)),
        }
    }

    fn extended(&mut self, opcode: u8) -> String {
        // http://www.winape.net/help/debug.html — WinAPE's name for the pair.
        if opcode == 0xFF {
            return "BRK".to_owned();
        }
        let Cuts { x, y, z, p, q } = cuts(opcode);
        if x == 2 && z <= 3 && y >= 4 {
            return BLI[usize::from(y - 4)][usize::from(z)].to_owned();
        }
        if x != 1 {
            return "NOP".to_owned();
        }
        let r = R[usize::from(y)];
        let rp = RP[usize::from(p)];
        match z {
            0 if y == 6 => "IN (C)".to_owned(),
            0 => format!("IN {r},(C)"),
            1 if y == 6 => "OUT (C),0".to_owned(),
            1 => format!("OUT (C),{r}"),
            2 => format!("{} HL,{rp}", if q == 0 { "SBC" } else { "ADC" }),
            3 if q == 0 => format!("LD ({}),{rp}", word(self.word())),
            3 => format!("LD {rp},({})", word(self.word())),
            4 => "NEG".to_owned(),
            5 if y == 1 => "RETI".to_owned(),
            5 => "RETN".to_owned(),
            6 => format!("IM {}", IM[usize::from(y)]),
            _ => INTERRUPT[usize::from(y)].to_owned(),
        }
    }

    // Under an index prefix the register field names a copy the
    // documentation does not carry: "RLC (IX+d),B" and its like.
    fn rotated(&mut self, opcode: u8) -> String {
        let Cuts { x, y, z, .. } = cuts(opcode);
        let target = if self.index.is_some() {
            self.offset()
        } else {
            R[usize::from(z)].to_owned()
        };
        let copy = if self.index.is_some() && z != 6 {
            format!(",{}", R[usize::from(z)])
        } else {
            String::new()
        };
        match x {
            0 => format!("{} {target}{copy}", ROT[usize::from(y)]),
            1 => format!("BIT {y},{target}"),
            2 => format!("RES {y},{target}{copy}"),
            _ => format!("SET {y},{target}{copy}"),
        }
    }
}

// The instruction at an address, read through `read`, which is the
// processor's view and wraps past &FFFF as its program counter does. A chain
// of DD and FD prefixes is one instruction, ruled by its last prefix, because
// the processor runs it as one: a prefix has not finished until the
// instruction it modifies has run (the emulator's z80.h). None if the chain
// runs to `most` bytes without ending.
pub fn instruction(read: &dyn Fn(u16) -> u8, address: u16, most: usize) -> Option<Instruction> {
    let mut decoding = Decoding {
        read,
        address,
        length: 0,
        index: None,
        displacement: None,
    };
    let mut opcode = decoding.next();
    while opcode == 0xDD || opcode == 0xFD {
        if decoding.length >= most {
            return None;
        }
        decoding.index = Some(if opcode == 0xDD { "IX" } else { "IY" });
        opcode = decoding.next();
    }
    let text = match opcode {
        0xCB => {
            if decoding.index.is_some() {
                decoding.offset();
            }
            let opcode = decoding.next();
            decoding.rotated(opcode)
        }
        0xED => {
            decoding.index = None;
            let opcode = decoding.next();
            decoding.extended(opcode)
        }
        _ => decoding.main(opcode),
    };
    Some(Instruction {
        length: decoding.length,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE};
    use crate::machine::Spectrum;

    const PREFIXES: [u8; 4] = [0xCB, 0xDD, 0xED, 0xFD];

    // Every opcode with and without each prefix, its displacement where it
    // takes one, and chains of index prefixes, ED after one among them.
    fn forms() -> Vec<Vec<u8>> {
        let mut forms = Vec::new();
        for opcode in 0..=u8::MAX {
            if !PREFIXES.contains(&opcode) {
                forms.push(vec![opcode]);
            }
            forms.push(vec![0xCB, opcode]);
            forms.push(vec![0xED, opcode]);
            for prefix in [0xDD, 0xFD] {
                if !PREFIXES.contains(&opcode) {
                    forms.push(vec![prefix, opcode]);
                }
                forms.push(vec![prefix, 0xCB, 0x05, opcode]);
            }
        }
        forms.extend([
            vec![0xDD, 0xDD, 0x21],
            vec![0xDD, 0xFD, 0x36],
            vec![0xFD, 0xDD, 0xFD, 0xE5],
            vec![0xFD, 0xDD, 0xCB, 0x05, 0x46],
            vec![0xDD, 0xED, 0x6B],
        ]);
        forms
    }

    // What moves the program counter by what it reads from the stack or a
    // register pair, or back to an instruction's own start.
    const UNLISTED_TARGETS: [&str; 12] = [
        "HALT", "RET", "RETI", "RETN", "LDIR", "LDDR", "CPIR", "CPDR", "INIR", "INDR", "OTIR",
        "OTDR",
    ];

    // What moves it to the address its last operand names, if it is taken.
    const JUMPS: [&str; 5] = ["CALL", "DJNZ", "JP", "JR", "RST"];

    // The conditions met with every flag clear: S, Z, P/V and C (UM0080).
    const MET: [&str; 4] = ["NZ", "NC", "PO", "P"];

    // Every instruction is as long as what step runs, and every jump goes
    // where the listing says, the emulator's processor being the measure of
    // both: with every flag clear and B zero, which DJNZ counts down past, a
    // jump is taken unless its condition wants a flag set. Each runs from
    // RAM, with interrupts disabled, its operands after it, and the
    // registers pointing where nothing it writes reaches it.
    #[test]
    fn an_instruction_is_as_long_as_what_step_runs_and_jumps_where_it_says() {
        let rom = Box::new([0xF3; SPECTRUM_ROM_SIZE as usize]);
        let ram = vec![0; SPECTRUM_RAM_48K as usize].into_boxed_slice();
        let mut spectrum = Spectrum::new(ram, rom);
        let (mut stepped, mut jumped) = (0, 0);
        for form in forms() {
            let bytes: Vec<u8> = form
                .iter()
                .chain(&[0x05, 0x34, 0x12, 0x00])
                .copied()
                .collect();
            for (offset, byte) in (0..).zip(&bytes) {
                spectrum.poke(0x8000 + offset, *byte);
            }
            let read = |address: u16| bytes[usize::from(address - 0x8000)];
            let listed = instruction(&read, 0x8000, 4096).expect("an instruction");
            let (mnemonic, operands) = listed.text.split_once(' ').unwrap_or((&listed.text, ""));
            if UNLISTED_TARGETS.contains(&mnemonic) || operands.starts_with('(') && mnemonic == "JP"
            {
                continue;
            }
            let cpu = &mut spectrum.machine_mut().cpu;
            (cpu.pc, cpu.sp, cpu.iff1, cpu.iff2, cpu.f) = (0x8000, 0xFF00, false, false, 0);
            (cpu.b, cpu.c, cpu.h, cpu.l) = (0x00, 0x01, 0xC0, 0x00);
            (cpu.ixh, cpu.ixl, cpu.iyh, cpu.iyl) = (0xC0, 0x00, 0xC0, 0x00);
            let step = spectrum.step(1);
            assert!(step.settled, "{form:02X?}");
            let next = 0x8000 + listed.length as u16;
            let expected = if JUMPS.contains(&mnemonic) {
                jumped += 1;
                let target = operands.rsplit('&').next().unwrap_or_default();
                let target = u16::from_str_radix(target, 16).expect("a target");
                match operands.split_once(',') {
                    Some((condition, _)) if !MET.contains(&condition) => next,
                    _ => target,
                }
            } else {
                next
            };
            assert_eq!(
                spectrum.machine().cpu.pc,
                expected,
                "{form:02X?}: {}",
                listed.text
            );
            stepped += 1;
        }
        assert!(
            stepped > 1500 && jumped > 50,
            "{stepped} forms stepped, {jumped} of them jumps"
        );
    }

    fn listed(bytes: &[u8]) -> (usize, String) {
        let read = |address: u16| bytes.get(usize::from(address)).copied().unwrap_or(0);
        let instruction = instruction(&read, 0, 4096).expect("an instruction");
        (instruction.length, instruction.text)
    }

    // A chain of prefixes is one instruction ruled by its last prefix, and
    // ED takes no index from one before it (the emulator's z80.c).
    #[test]
    fn a_chain_of_prefixes_is_one_instruction_ruled_by_its_last() {
        assert_eq!(
            listed(&[0xDD, 0xDD, 0x21, 0x34, 0x12]),
            (5, "LD IX,&1234".to_owned())
        );
        assert_eq!(
            listed(&[0xDD, 0xFD, 0x21, 0x34, 0x12]),
            (5, "LD IY,&1234".to_owned())
        );
        assert_eq!(listed(&[0xDD, 0x18, 0x05]), (3, "JR &0008".to_owned()));
        assert_eq!(
            listed(&[0xFD, 0xED, 0x6B, 0x00, 0x40]),
            (5, "LD HL,(&4000)".to_owned())
        );
        assert_eq!(listed(&[0xDD, 0x00]), (2, "NOP".to_owned()));
    }

    #[test]
    fn a_chain_that_does_not_end_is_no_instruction() {
        let read = |_| 0xFD;
        assert!(instruction(&read, 0, 64).is_none());
    }

    // A relative jump counts from the instruction's end, and wraps with the
    // program counter.
    #[test]
    fn a_relative_target_is_counted_from_the_end_and_wraps() {
        let bytes = [0x18, 0xFE];
        let read = |address: u16| bytes[usize::from(address.wrapping_sub(0xFFFF))];
        let jump = instruction(&read, 0xFFFF, 4096).expect("an instruction");
        assert_eq!((jump.length, jump.text.as_str()), (2, "JR &FFFF"));
    }
}
