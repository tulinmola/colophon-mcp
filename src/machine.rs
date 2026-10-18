use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::{PoisonError, RwLock};

use crate::emulator::{
    CPC_FRAMEBUFFER_HEIGHT, CPC_FRAMEBUFFER_WIDTH, SPECTRUM_FRAMEBUFFER_HEIGHT,
    SPECTRUM_FRAMEBUFFER_WIDTH, SPECTRUM_RAM_16K, SPECTRUM_RAM_48K, SPECTRUM_ROM_SIZE,
    cpc_connect_monitor, cpc_fit_disc_interface, cpc_init, cpc_set_upper_rom, cpc_t, mcp_cpc_run,
    mcp_spectrum_run, spectrum_connect_monitor, spectrum_init, spectrum_t,
};

pub use crate::emulator::mcp_run_t as Run;

// Every CPC reads one buffer of the core's own for an empty upper ROM socket,
// and each cpc_init fills it, so no CPC is built while another is built or
// runs: https://github.com/tulinmola/colophon-emulator/issues/4
static EMPTY_SOCKET: RwLock<()> = RwLock::new(());

// What is handed to the core is held by a raw pointer from then on, because
// the core keeps pointers into it, and moving or reborrowing a Box would
// invalidate them.
struct Handed<T: ?Sized>(NonNull<T>);

impl<T: ?Sized> Handed<T> {
    fn new(contents: Box<T>) -> Handed<T> {
        Handed(NonNull::from(Box::leak(contents)))
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T: ?Sized> Drop for Handed<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Box that this value alone holds.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

// A run counts the monitor's retraces, and an unplugged monitor has none.
fn framebuffer(width: u32, height: u32) -> Handed<[u8]> {
    Handed::new(vec![0; (width * height) as usize].into_boxed_slice())
}

pub struct Cpc {
    machine: Handed<cpc_t>,
    upper_roms: BTreeMap<u8, Handed<[u8; 0x4000]>>,
    _ram: Handed<[u8]>,
    _lower_rom: Handed<[u8; 0x4000]>,
    _framebuffer: Handed<[u8]>,
}

impl Cpc {
    pub fn new(ram: Box<[u8]>, lower_rom: Box<[u8; 0x4000]>) -> Cpc {
        assert!(
            matches!(ram.len(), 0x10000 | 0x20000),
            "a CPC takes 64K or 128K of RAM, not {} bytes",
            ram.len()
        );
        let ram_size = ram.len() as u32;
        // SAFETY: bindgen renders a machine as integers, bools and pointers,
        // in arrays and structs of them, and all-zero bytes are a value of
        // each.
        let machine = Handed::new(unsafe { Box::<cpc_t>::new_zeroed().assume_init() });
        let ram = Handed::new(ram);
        let lower_rom = Handed::new(lower_rom);
        let framebuffer = framebuffer(CPC_FRAMEBUFFER_WIDTH, CPC_FRAMEBUFFER_HEIGHT);
        let _building = EMPTY_SOCKET.write().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: what is handed over is held beside the machine and freed
        // after it, and the RAM is one of the two sizes cpc_init knows.
        unsafe {
            cpc_init(
                machine.as_ptr(),
                ram.as_ptr().cast(),
                ram_size,
                lower_rom.as_ptr().cast(),
            );
            cpc_connect_monitor(machine.as_ptr(), framebuffer.as_ptr().cast());
        }
        Cpc {
            machine,
            upper_roms: BTreeMap::new(),
            _ram: ram,
            _lower_rom: lower_rom,
            _framebuffer: framebuffer,
        }
    }

    pub fn set_upper_rom(&mut self, number: u8, rom: Box<[u8; 0x4000]>) {
        let rom = Handed::new(rom);
        // SAFETY: the ROM is held beside the machine; the one it replaces, if
        // any, is freed only once the machine no longer points at it.
        unsafe { cpc_set_upper_rom(self.machine.as_ptr(), number, rom.as_ptr().cast()) };
        self.upper_roms.insert(number, rom);
    }

    pub fn fit_disc_interface(&mut self, fitted: bool) {
        // SAFETY: the machine stands where it was built.
        unsafe { cpc_fit_disc_interface(self.machine.as_ptr(), fitted) };
    }

    pub fn run(&mut self, frames: u32, limit: u64) -> Run {
        let _running = EMPTY_SOCKET.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the machine and what it was handed stand where they were.
        unsafe { mcp_cpc_run(self.machine.as_ptr(), frames, limit) }
    }
}

pub struct Spectrum {
    machine: Handed<spectrum_t>,
    _ram: Handed<[u8]>,
    _rom: Handed<[u8; SPECTRUM_ROM_SIZE as usize]>,
    _framebuffer: Handed<[u8]>,
}

impl Spectrum {
    pub fn new(ram: Box<[u8]>, rom: Box<[u8; SPECTRUM_ROM_SIZE as usize]>) -> Spectrum {
        assert!(
            [SPECTRUM_RAM_16K as usize, SPECTRUM_RAM_48K as usize].contains(&ram.len()),
            "a Spectrum takes 16K or 48K of RAM, not {} bytes",
            ram.len()
        );
        let ram_size = ram.len() as u32;
        // SAFETY: as for a CPC, all-zero bytes are a value of every field.
        let machine = Handed::new(unsafe { Box::<spectrum_t>::new_zeroed().assume_init() });
        let ram = Handed::new(ram);
        let rom = Handed::new(rom);
        let framebuffer = framebuffer(SPECTRUM_FRAMEBUFFER_WIDTH, SPECTRUM_FRAMEBUFFER_HEIGHT);
        // SAFETY: as for a CPC, what is handed over outlives the machine, and
        // the RAM is one of the two sizes spectrum_init knows.
        unsafe {
            spectrum_init(
                machine.as_ptr(),
                ram.as_ptr().cast(),
                ram_size,
                rom.as_ptr().cast(),
            );
            spectrum_connect_monitor(machine.as_ptr(), framebuffer.as_ptr().cast());
        }
        Spectrum {
            machine,
            _ram: ram,
            _rom: rom,
            _framebuffer: framebuffer,
        }
    }

    pub fn run(&mut self, frames: u32, limit: u64) -> Run {
        // SAFETY: the machine and what it was handed stand where they were.
        unsafe { mcp_spectrum_run(self.machine.as_ptr(), frames, limit) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::spectrum_instruction_complete;

    const DI: u8 = 0xF3;
    const EX_SP_IX: [u8; 2] = [0xDD, 0xE3];
    const HALT: u8 = 0x76;
    const INC_BC: u8 = 0x03;
    const JR: u8 = 0x18;
    const LD_A: u8 = 0x3E;
    const LD_BC: u8 = 0x01;
    const LD_I_A: [u8; 2] = [0xED, 0x47];
    const OUT_C_C: [u8; 2] = [0xED, 0x49];
    const PREFIX_DD: u8 = 0xDD;

    // A standard frame is 312 lines of 64µs, "exactly 19968 µsec" (Compendium
    // ch. 4.1), and the Gate Array "clocks the Z80A at 4 MHz" (ch. 4.4.4):
    // https://shaker.logonsystem.eu/ACCC1.11-EN.pdf
    const CPC_STANDARD_FRAME_TICKS: u64 = 19968 * 4;

    // 224 T-states to a line and 312 lines to a frame on a 48K's Ferranti
    // ULA: libspectrum's timings.c,
    // https://sourceforge.net/p/fuse-emulator/libspectrum/ci/master/tree/timings.c
    const SPECTRUM_FRAME_TICKS: u64 = 224 * 312;

    // The firmware's 50Hz screen: the table the 6128's ROM holds at &05C5,
    // which Compendium ch. 4.1 names, written as colophon-emulator's
    // test/cpc_test.c writes it — every register the table sets to other than
    // zero, save R14's &C0, which belongs to the cursor.
    const STANDARD_SCREEN: [(u8, u8); 9] = [
        (0, 63),
        (1, 40),
        (2, 46),
        (3, 0x8E),
        (4, 38),
        (6, 25),
        (7, 30),
        (9, 7),
        (12, 0x30),
    ];

    fn rom<const SIZE: usize>(program: &[u8], fill: u8) -> Box<[u8; SIZE]> {
        let mut rom = Box::new([fill; SIZE]);
        rom[..program.len()].copy_from_slice(program);
        rom
    }

    fn write_crtc(program: &mut Vec<u8>, register: u8, value: u8) {
        program.extend([LD_BC, register, 0xBC]);
        program.extend(OUT_C_C);
        program.extend([LD_BC, value, 0xBD]);
        program.extend(OUT_C_C);
    }

    fn standard_screen() -> Vec<u8> {
        let mut program = vec![DI];
        for (register, value) in STANDARD_SCREEN {
            write_crtc(&mut program, register, value);
        }
        program
    }

    fn sixty_four_k() -> Box<[u8]> {
        vec![0; 0x10000].into_boxed_slice()
    }

    fn forty_eight_k() -> Box<[u8]> {
        vec![0; SPECTRUM_RAM_48K as usize].into_boxed_slice()
    }

    // Past the frames that fall while the 6845 is being programmed, and
    // standing just after a retrace.
    fn cpc_on_the_standard_screen() -> Cpc {
        let mut cpc = Cpc::new(sixty_four_k(), rom(&standard_screen(), HALT));
        assert_eq!(cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS).frames, 2);
        cpc
    }

    // A CPC whose 6845 is never programmed never retraces, so a test that
    // leans on the limit programs it first and steps the setup one
    // instruction at a time: a step is tens of ticks, where a run without its
    // limit would go on to a retrace.
    fn cpc_past_the_standard_screen(code: &[u8], fill: u8) -> (Cpc, u16) {
        let mut program = standard_screen();
        let start = program.len() as u16;
        program.extend(code);
        let mut cpc = Cpc::new(sixty_four_k(), rom(&program, fill));
        for _instruction in 0..1 + 4 * STANDARD_SCREEN.len() {
            let step = cpc.run(1, 1);
            assert!(step.frames == 0 && step.ticks < 64, "a step ran {step:?}");
        }
        assert_eq!(cpc.machine().cpu.pc, start);
        (cpc, start)
    }

    fn spectrum_after_a_retrace() -> Spectrum {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[DI], HALT));
        assert_eq!(spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS).frames, 2);
        spectrum
    }

    impl Cpc {
        pub(crate) fn machine(&self) -> &cpc_t {
            // SAFETY: no run is under way while this value is borrowed.
            unsafe { &*self.machine.as_ptr() }
        }
    }

    impl Spectrum {
        pub(crate) fn machine(&self) -> &spectrum_t {
            // SAFETY: no run is under way while this value is borrowed.
            unsafe { &*self.machine.as_ptr() }
        }
    }

    // A frame retrace sends the beam to the top-left corner, as the core's
    // src/monitor.c does it.
    #[test]
    fn a_cpc_run_by_the_frame_stops_at_each_retrace() {
        let mut cpc = cpc_on_the_standard_screen();
        for _ in 0..3 {
            let run = cpc.run(1, 2 * CPC_STANDARD_FRAME_TICKS);
            assert_eq!(
                (run.ticks, run.frames, run.settled),
                (CPC_STANDARD_FRAME_TICKS, 1, true)
            );
            assert_eq!(cpc.machine().monitor.beam_y, 0);
        }
    }

    #[test]
    fn a_spectrum_run_by_the_frame_stops_at_each_retrace() {
        let mut spectrum = spectrum_after_a_retrace();
        for _ in 0..3 {
            let run = spectrum.run(1, 2 * SPECTRUM_FRAME_TICKS);
            assert_eq!(
                (run.ticks, run.frames, run.settled),
                (SPECTRUM_FRAME_TICKS, 1, true)
            );
            assert_eq!(spectrum.machine().monitor.beam_y, 0);
        }
    }

    // A run cut to one tick spends the rest of its HALT finishing it, so
    // most retraces fall where only the finishing can count them.
    #[test]
    fn a_cpc_run_cut_short_misses_no_retrace() {
        let mut cpc = cpc_on_the_standard_screen();
        let half = cpc.run(2, CPC_STANDARD_FRAME_TICKS / 2);
        assert_eq!(half.frames, 0);
        let (mut ticks, mut frames) = (half.ticks, 0);
        while ticks < 3 * CPC_STANDARD_FRAME_TICKS {
            let run = cpc.run(1, 1);
            ticks += run.ticks;
            frames += run.frames;
        }
        assert_eq!(frames, 3);
    }

    #[test]
    fn a_spectrum_run_cut_short_misses_no_retrace() {
        let mut spectrum = spectrum_after_a_retrace();
        let half = spectrum.run(2, SPECTRUM_FRAME_TICKS / 2);
        assert_eq!(half.frames, 0);
        let (mut ticks, mut frames) = (half.ticks, 0);
        while ticks < 3 * SPECTRUM_FRAME_TICKS {
            let run = spectrum.run(1, 1);
            ticks += run.ticks;
            frames += run.frames;
        }
        assert_eq!(frames, 3);
    }

    // LD BC,nn takes 10 T-states and EX (SP),IX 23, the longest of any
    // instruction here: Z80 CPU User Manual (Zilog UM0080),
    // https://www.zilog.com/docs/z80/um0080.pdf. Run from the ROM, with SP
    // at &FFFF and I at zero as the core's z80_init leaves them, nothing
    // either touches is in contended memory.
    #[test]
    fn a_spectrum_stopped_by_its_limit_finishes_the_instruction() {
        let program = [LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1]];
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        let nothing = spectrum.run(1, 0);
        assert_eq!(
            (nothing.ticks, nothing.frames, nothing.settled),
            (0, 0, true)
        );
        let load = spectrum.run(1, 1);
        assert_eq!((load.ticks, load.frames, load.settled), (10, 0, true));
        let processor = &spectrum.machine().cpu;
        assert_eq!((processor.b, processor.c, processor.pc), (0x12, 0x34, 3));
        let exchange = spectrum.run(1, 1);
        assert_eq!(
            (exchange.ticks, exchange.frames, exchange.settled),
            (23, 0, true)
        );
        assert_eq!(spectrum.machine().cpu.pc, 5);
    }

    // A CPC's published durations run from one M1 edge to the next, and a run
    // ends where an instruction completes, which is ragged against the Gate
    // Array's wait states; what the instruction did is the measure.
    #[test]
    fn a_cpc_stopped_by_its_limit_finishes_the_instruction() {
        let code = [LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1]];
        let (mut cpc, start) = cpc_past_the_standard_screen(&code, HALT);
        let nothing = cpc.run(1, 0);
        assert_eq!(
            (nothing.ticks, nothing.frames, nothing.settled),
            (0, 0, true)
        );
        let load = cpc.run(1, 1);
        assert_eq!((load.frames, load.settled), (0, true));
        let processor = &cpc.machine().cpu;
        assert_eq!(
            (processor.b, processor.c, processor.pc),
            (0x12, 0x34, start + 3)
        );
        let exchange = cpc.run(1, 1);
        assert_eq!((exchange.frames, exchange.settled), (0, true));
        assert_eq!(cpc.machine().cpu.pc, start + 5);
    }

    // INC BC ends on two internal T-states with the IR pair on the bus, and
    // with I in contended memory the ULA charges them during the picture
    // ("Contended memory", Sinclair Wiki,
    // https://sinclair.wiki.zxnet.co.uk/wiki/Contended_memory), so the
    // processor finishes before the machine does. Unheld, INC BC takes 6
    // T-states and JR, in the ROM, always takes 12 (UM0080), so a run of any
    // other length waited out a charge.
    #[test]
    fn a_spectrum_stopped_by_its_limit_waits_out_the_ula() {
        let program = [DI, LD_A, 0x40, LD_I_A[0], LD_I_A[1], INC_BC, JR, 0xFD];
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        for _setting_up in 0..3 {
            spectrum.run(1, 1);
        }
        let mut ticks = 0;
        let mut charged = 0;
        while ticks < 2 * SPECTRUM_FRAME_TICKS {
            let run = spectrum.run(1, 1);
            // SAFETY: no run is under way.
            let settled = unsafe { spectrum_instruction_complete(spectrum.machine.as_ptr()) };
            assert!(
                settled,
                "a run ended {ticks} ticks in with the ULA still holding"
            );
            ticks += run.ticks;
            if run.ticks != 6 && run.ticks != 12 {
                charged += 1;
            }
        }
        assert!(charged > 0, "no run waited out a charge");
    }

    // A prefix has not finished until the instruction it modifies has run, so
    // a chain of them never reaches the end of one.
    #[test]
    fn a_cpc_caught_in_a_chain_of_prefixes_ends_unsettled() {
        let (mut cpc, _) = cpc_past_the_standard_screen(&[], PREFIX_DD);
        assert!(!cpc.run(1, 1).settled);
    }

    #[test]
    fn a_spectrum_caught_in_a_chain_of_prefixes_ends_unsettled() {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[], PREFIX_DD));
        assert!(!spectrum.run(1, 1).settled);
    }

    #[test]
    fn each_machine_is_built_with_the_ram_it_is_handed() {
        for size in [0x10000, 0x20000] {
            let cpc = Cpc::new(vec![0; size].into_boxed_slice(), rom(&[], HALT));
            assert_eq!(cpc.machine().ram_size as usize, size);
            assert_eq!(cpc.machine().ram, cpc._ram.as_ptr().cast());
        }
        for size in [SPECTRUM_RAM_16K, SPECTRUM_RAM_48K] {
            let ram = vec![0; size as usize].into_boxed_slice();
            let spectrum = Spectrum::new(ram, rom(&[], HALT));
            assert_eq!(spectrum.machine().ram_size, size);
            assert_eq!(spectrum.machine().ram, spectrum._ram.as_ptr().cast());
        }
    }

    #[test]
    #[should_panic(expected = "a CPC takes 64K or 128K of RAM")]
    fn a_cpc_refuses_ram_its_board_cannot_take() {
        Cpc::new(vec![0; 0x8000].into_boxed_slice(), rom(&[], HALT));
    }

    #[test]
    #[should_panic(expected = "a Spectrum takes 16K or 48K of RAM")]
    fn a_spectrum_refuses_ram_its_board_cannot_take() {
        Spectrum::new(vec![0; 0x8000].into_boxed_slice(), rom(&[], HALT));
    }
}
