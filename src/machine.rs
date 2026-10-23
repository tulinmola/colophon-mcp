use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::c_char;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{PoisonError, RwLock};

use crate::emulator::{
    CPC_FRAMEBUFFER_HEIGHT, CPC_FRAMEBUFFER_WIDTH, FLOPPY_BYTES_PER_REVOLUTION,
    FLOPPY_MAX_CYLINDERS, FLOPPY_MAX_SIDES, MCP_COUNTS_PER_KIND, MCP_KINDS,
    SPECTRUM_FRAMEBUFFER_HEIGHT, SPECTRUM_FRAMEBUFFER_WIDTH, SPECTRUM_RAM_16K, SPECTRUM_RAM_48K,
    SPECTRUM_ROM_SIZE, SPECTRUM_TICKS_PER_MILLISECOND, cpc_connect_monitor, cpc_fit_disc_interface,
    cpc_init, cpc_insert_disc, cpc_peek, cpc_poke, cpc_remap, cpc_set_upper_rom, cpc_snapshot_load,
    cpc_snapshot_save, cpc_snapshot_size, cpc_t, dsk_read, floppy_give_room, floppy_t,
    gate_array_rgb as core_gate_array_rgb, keyboard_key, keyboard_press, keyboard_t, mcp_cpc_call,
    mcp_cpc_check, mcp_cpc_run, mcp_cpc_step, mcp_spectrum_call, mcp_spectrum_check,
    mcp_spectrum_run, mcp_spectrum_seek, mcp_spectrum_step, mcp_watch_t, mcp_z80_halted_for_good,
    spectrum_connect_monitor, spectrum_init, spectrum_insert_tape, spectrum_instruction_complete,
    spectrum_peek, spectrum_poke, spectrum_snapshot_load, spectrum_snapshot_save,
    spectrum_snapshot_size, spectrum_t, tape_init, tape_insert, tape_play, tape_t,
    tzx_machine_TZX_SPECTRUM, tzx_next_pulse, tzx_open, tzx_t, ula_rgb as core_ula_rgb,
    z80_instruction_complete, z80_t,
};

pub use crate::emulator::{
    mcp_condition_t as Condition, mcp_kind_t as Kind, mcp_kind_t_MCP_EXECUTE as EXECUTE,
    mcp_kind_t_MCP_IN as IN, mcp_kind_t_MCP_OUT as OUT, mcp_kind_t_MCP_PSG as PSG,
    mcp_kind_t_MCP_READ as READ, mcp_kind_t_MCP_WRITE as WRITE, mcp_list_t as List,
    mcp_list_t_MCP_COUNT as COUNT, mcp_list_t_MCP_GATHER as GATHER, mcp_list_t_MCP_UNTIL as UNTIL,
    mcp_match_t as Match, mcp_place_t as Place, mcp_run_t as Run,
};

// The kinds in the order mcp_kind_t gives them, which is how a watch's counts
// are laid out.
pub const KINDS: [Kind; MCP_KINDS as usize] = [EXECUTE, READ, WRITE, IN, OUT, PSG];

pub const COUNTS_PER_KIND: usize = MCP_COUNTS_PER_KIND as usize;

// Each kind's counts stand at its own value in mcp_kind_t.
const _: () = {
    let mut kind = 0;
    while kind < KINDS.len() {
        assert!(KINDS[kind] as usize == kind);
        kind += 1;
    }
};

// A watch holds the conditions, the room for matches, the counts and the
// room for places it is given as pointers, so it borrows them for as long as
// it lives, and the bridge never reaches past any of them.
pub struct Watch<'a> {
    watch: mcp_watch_t,
    _borrowed: PhantomData<Borrowed<'a>>,
}

type Borrowed<'a> = (
    &'a [Condition],
    &'a mut [Match],
    &'a mut [u32],
    &'a mut [Place],
);

impl<'a> Watch<'a> {
    // The counts are laid out as mcp_watch_t says and all zero where a
    // condition is in the count list, and empty where none is.
    pub fn new(
        conditions: &'a [Condition],
        matches: &'a mut [Match],
        counts: &'a mut [u32],
        places: &'a mut [Place],
        most_gathered: u32,
        most_counted: u32,
    ) -> Watch<'a> {
        let counting = conditions.iter().any(|condition| condition.list == COUNT);
        assert!(
            counts.len()
                == if counting {
                    KINDS.len() * COUNTS_PER_KIND
                } else {
                    0
                }
                && counts.iter().all(|count| *count == 0),
            "a count of zero for every kind and every address, where the count list has a \
             condition"
        );
        // SAFETY: bindgen renders a watch as integers, bools and pointers,
        // and all-zero bytes are a value of each.
        let mut watch: mcp_watch_t = unsafe { std::mem::zeroed() };
        watch.conditions = conditions.as_ptr();
        watch.condition_count = u32::try_from(conditions.len()).expect("a few conditions");
        watch.matches = matches.as_mut_ptr();
        watch.room = u32::try_from(matches.len()).expect("room for a million matches or so");
        watch.most_gathered = most_gathered;
        if counting {
            watch.counts = counts.as_mut_ptr();
        }
        watch.places = places.as_mut_ptr();
        watch.place_room = u32::try_from(places.len()).expect("room for every place");
        watch.most_counted = most_counted;
        Watch {
            watch,
            _borrowed: PhantomData,
        }
    }

    // The matches kept are the last there is room for, and the gathering
    // never fills.
    pub fn gather_last(&mut self) {
        assert!(self.watch.room > 0, "room for the last match at least");
        self.watch.gathers_last = true;
    }

    // An access is counted by where the instruction that made it begins.
    pub fn count_by_pc(&mut self) {
        self.watch.counts_by_pc = true;
    }

    // The run stops once the processor stands at pc with its stack at sp.
    pub fn returning_to(&mut self, pc: u16, sp: u16) {
        self.watch.calling = true;
        self.watch.return_pc = pc;
        self.watch.return_sp = sp;
    }

    pub fn seen(&self) -> &mcp_watch_t {
        &self.watch
    }
}

// A run watches only when given a watch.
fn watching(watch: Option<&mut Watch>) -> *mut mcp_watch_t {
    watch.map_or(std::ptr::null_mut(), |watch| &raw mut watch.watch)
}

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

// What a CPC's colour code is worth on the cable, as 0xRRGGBB, from the
// levels measured on a 40010, of which the code's lowest five bits pick one.
pub fn gate_array_rgb(colour_code: u8) -> u32 {
    // SAFETY: the core only reads a table of its own.
    unsafe { core_gate_array_rgb(colour_code) }
}

// What a Spectrum's sample is worth on the cable, as 0xRRGGBB, from its lowest
// four bits.
pub fn ula_rgb(sample: u8) -> u32 {
    // SAFETY: the core only computes from the sample.
    unsafe { core_ula_rgb(sample) }
}

// The core and the bridge say what is wrong in a sentence of their own.
fn problem(sentence: *const c_char) -> String {
    // SAFETY: a problem points at a string literal.
    unsafe { CStr::from_ptr(sentence) }
        .to_string_lossy()
        .into_owned()
}

impl z80_t {
    pub fn halted_for_good(&self) -> bool {
        // SAFETY: the bridge reads two fields of the processor.
        unsafe { mcp_z80_halted_for_good(self) }
    }
}

impl keyboard_t {
    // Holds a key down, as line * 8 + bit.
    pub fn press(&mut self, key: keyboard_key) {
        // SAFETY: the core changes the line's byte, past the matrix nothing.
        unsafe { keyboard_press(self, key) }
    }
}

// Room past a disc's image for every track the medium can hold to be
// formatted once more, a revolution's worth each, as the emulator's command
// line gives a disc: FORMAT_ROOM in its cli/main.c.
const FORMAT_ROOM: usize = FLOPPY_MAX_CYLINDERS as usize
    * FLOPPY_MAX_SIDES as usize
    * FLOPPY_BYTES_PER_REVOLUTION as usize;

// A medium, and the image it borrows, which takes the machine's writes; the
// drive holds it, and so does every state kept while it stood there. It
// remembers what it held when it was last kept or restored, so that states
// kept while it does not change share one copy.
struct Disc {
    floppy: Handed<floppy_t>,
    image: Handed<[u8]>,
    last: RefCell<Option<Rc<DiscContents>>>,
}

// A medium's bytes and its image's, as they stood.
struct DiscContents {
    floppy: Box<[u8]>,
    image: Box<[u8]>,
}

// A disc as a state keeps it, which a restore writes back into the disc.
struct DiscState {
    disc: Rc<Disc>,
    contents: Rc<DiscContents>,
}

impl Disc {
    fn new(floppy: Handed<floppy_t>, image: Handed<[u8]>) -> Disc {
        Disc {
            floppy,
            image,
            last: RefCell::new(None),
        }
    }

    fn kept(self: &Rc<Disc>) -> DiscState {
        // SAFETY: the medium and its image stand beside the machine, which
        // is not running. Every byte of the medium holds a value: it was
        // allocated zeroed, and C has written it since, by its fields and by
        // floppy_init's assignment, whose padding at worst costs a copy.
        let (floppy, image) = unsafe {
            let floppy = self.floppy.as_ptr();
            let bytes = std::slice::from_raw_parts(floppy.cast::<u8>(), size_of::<floppy_t>());
            (bytes, &(&*self.image.as_ptr())[..(*floppy).image_length])
        };
        let mut last = self.last.borrow_mut();
        let contents = match &*last {
            Some(contents) if *contents.floppy == *floppy && *contents.image == *image => {
                Rc::clone(contents)
            }
            _ => Rc::new(DiscContents {
                floppy: floppy.into(),
                image: image.into(),
            }),
        };
        *last = Some(Rc::clone(&contents));
        DiscState {
            disc: Rc::clone(self),
            contents,
        }
    }
}

impl DiscState {
    // The disc, as it was when it was kept. Past the image's length its
    // buffer is room that a format fills whole before anything reads it.
    fn restored(&self) -> Rc<Disc> {
        // SAFETY: as for kept(); the image kept is no longer than the buffer,
        // which never shrinks.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.contents.floppy.as_ptr(),
                self.disc.floppy.as_ptr().cast::<u8>(),
                self.contents.floppy.len(),
            );
            (&mut *self.disc.image.as_ptr())[..self.contents.image.len()]
                .copy_from_slice(&self.contents.image);
        }
        *self.disc.last.borrow_mut() = Some(Rc::clone(&self.contents));
        Rc::clone(&self.disc)
    }
}

// The deck a Spectrum plays, and the reader it points at and asks for
// pulses, as colophon-player's emulator/deck.h holds them
// (https://github.com/tulinmola/colophon-player).
#[derive(Clone, Copy)]
struct Deck {
    tape: tape_t,
    reader: tzx_t,
}

// A deck, and the image its reader borrows, held as a disc is.
struct Tape {
    deck: Handed<Deck>,
    _image: Handed<[u8]>,
}

// A CPC as a state keeps it, to be restored into the machine it was kept
// from and no other: its struct points into itself and at buffers of that
// machine's.
pub struct CpcState {
    machine: Box<cpc_t>,
    ram: Box<[u8]>,
    framebuffer: Box<[u8]>,
    discs: [Option<DiscState>; 2],
}

// A Spectrum as a state keeps it, likewise.
pub struct SpectrumState {
    machine: Box<spectrum_t>,
    ram: Box<[u8]>,
    framebuffer: Box<[u8]>,
    tape: Option<(Rc<Tape>, Deck)>,
}

// What a snapshot writer leaves behind: the bytes, or the core's sentence.
fn saved(bytes: Vec<u8>, written: bool, sentence: *const c_char) -> Result<Vec<u8>, String> {
    if written {
        Ok(bytes)
    } else {
        Err(problem(sentence))
    }
}

pub struct Cpc {
    machine: Handed<cpc_t>,
    upper_roms: BTreeMap<u8, Handed<[u8; 0x4000]>>,
    ram: Handed<[u8]>,
    _lower_rom: Handed<[u8; 0x4000]>,
    framebuffer: Handed<[u8]>,
    discs: [Option<Rc<Disc>>; 2],
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
            ram,
            _lower_rom: lower_rom,
            framebuffer,
            discs: [None, None],
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

    pub fn run(&mut self, frames: u32, limit: u64, watch: Option<&mut Watch>) -> Run {
        let _running = EMPTY_SOCKET.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the machine and what it was handed stand where they were,
        // and a watch borrows its conditions, its room for matches and its
        // counts, which are as long as it says, for as long as it lives, and
        // has counts wherever a condition is in the count list.
        unsafe { mcp_cpc_run(self.machine.as_ptr(), frames, limit, watching(watch)) }
    }

    pub fn call(&mut self, address: u16) -> Result<(), String> {
        // SAFETY: the bridge pushes through write_page, which cpc_remap points
        // only into the RAM the machine was handed.
        let sentence = unsafe { mcp_cpc_call(self.machine.as_ptr(), address) };
        if sentence.is_null() {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    pub fn step(&mut self, instructions: u32) -> Run {
        let _running = EMPTY_SOCKET.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: as for a run.
        unsafe { mcp_cpc_step(self.machine.as_ptr(), instructions) }
    }

    pub fn peek(&self, address: u16) -> u8 {
        let _reading = EMPTY_SOCKET.read().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: the memory the machine reads stands where it was handed.
        unsafe { cpc_peek(self.machine.as_ptr(), address) }
    }

    pub fn poke(&mut self, address: u16, value: u8) {
        // SAFETY: the processor writes only to the RAM it was handed.
        unsafe { cpc_poke(self.machine.as_ptr(), address, value) }
    }

    pub fn remap(&mut self) {
        // SAFETY: the machine stands where it was built.
        unsafe { cpc_remap(self.machine.as_ptr()) }
    }

    pub fn snapshot_load(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut sentence = std::ptr::null();
        // SAFETY: the bytes are only read, for as long as the call, and the
        // RAM they are copied into is the machine's own.
        let loaded = unsafe {
            cpc_snapshot_load(
                self.machine.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                &mut sentence,
            )
        };
        if loaded {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    // Reads the image onto a medium of its own, with room to format every
    // track again, and puts it in drive 0 or 1 in place of the disc there,
    // which is freed once neither the drive nor any state holds it. A refused
    // image leaves the drive as it was.
    pub fn insert_disc(&mut self, drive: u8, contents: &[u8]) -> Result<(), String> {
        assert!(drive < 2, "a CPC has drives 0 and 1, not {drive}");
        let length = contents.len();
        let mut image = vec![0; length + FORMAT_ROOM].into_boxed_slice();
        image[..length].copy_from_slice(contents);
        let image = Handed::new(image);
        // SAFETY: as for a machine, all-zero bytes are a value of every field.
        let floppy = Handed::new(unsafe { Box::<floppy_t>::new_zeroed().assume_init() });
        let mut sentence = std::ptr::null();
        // SAFETY: the medium borrows the image, which is held beside it and
        // freed with it, and reads no further than the length given.
        let read = unsafe {
            dsk_read(
                floppy.as_ptr(),
                image.as_ptr().cast(),
                length,
                &mut sentence,
            )
        };
        if !read {
            return Err(problem(sentence));
        }
        // SAFETY: the room given is as far as the image's buffer reaches, and
        // the medium is held beside the machine for as long as the drive or a
        // state holds it.
        unsafe {
            floppy_give_room(floppy.as_ptr(), length + FORMAT_ROOM);
            cpc_insert_disc(self.machine.as_ptr(), drive, floppy.as_ptr());
        }
        self.discs[usize::from(drive)] = Some(Rc::new(Disc::new(floppy, image)));
        Ok(())
    }

    // The machine as it stands: inside an instruction if it stands there,
    // with its picture and each disc as it has been written.
    pub fn state(&self) -> CpcState {
        CpcState {
            // SAFETY: as for a copy set writes to: the struct owns nothing,
            // and this one is only ever written back over its machine.
            machine: Box::new(unsafe { std::ptr::read(self.machine()) }),
            ram: self.ram().into(),
            framebuffer: self.framebuffer().into(),
            discs: self
                .discs
                .each_ref()
                .map(|disc| disc.as_ref().map(Disc::kept)),
        }
    }

    // Puts the machine back as a state of its own kept it, with the discs it
    // held then, as they were, whatever the drives have held since.
    pub fn restore(&mut self, state: &CpcState) {
        for (disc, kept) in self.discs.iter_mut().zip(&state.discs) {
            *disc = kept.as_ref().map(DiscState::restored);
        }
        // SAFETY: as for state(); every pointer in it is one the machine held
        // when it was kept, at buffers held beside it since, its discs among
        // them.
        *self.machine_mut() = unsafe { std::ptr::read(&*state.machine) };
        // SAFETY: as for ram() and framebuffer(), mutably.
        unsafe {
            (*self.ram.as_ptr()).copy_from_slice(&state.ram);
            (*self.framebuffer.as_ptr()).copy_from_slice(&state.framebuffer);
        }
        self.remap();
    }

    // The machine as a version 1 snapshot.
    pub fn snapshot_save(&self) -> Result<Vec<u8>, String> {
        // SAFETY: the machine is only read.
        let size = unsafe { cpc_snapshot_size(self.machine.as_ptr()) };
        let mut bytes = vec![0; size];
        let mut sentence = std::ptr::null();
        // SAFETY: the machine is only read, and the bytes are as many as it
        // says it needs.
        let written = unsafe {
            cpc_snapshot_save(
                self.machine.as_ptr(),
                bytes.as_mut_ptr(),
                size,
                &mut sentence,
            )
        };
        saved(bytes, written, sentence)
    }

    pub fn check(&self, written: &cpc_t) -> Result<(), String> {
        // SAFETY: both are only read.
        let sentence = unsafe { mcp_cpc_check(self.machine.as_ptr(), written) };
        if sentence.is_null() {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    pub fn machine(&self) -> &cpc_t {
        // SAFETY: nothing else reaches the machine while this is borrowed,
        // because every call into the core borrows the machine as well.
        unsafe { &*self.machine.as_ptr() }
    }

    // Whether the processor stands between instructions.
    pub fn instruction_complete(&self) -> bool {
        // SAFETY: the processor is only read.
        unsafe { z80_instruction_complete(&self.machine().cpu) }
    }

    pub fn ram(&self) -> &[u8] {
        // SAFETY: as for machine(): the core writes the RAM only in a call
        // that borrows the machine mutably.
        unsafe { &*self.ram.as_ptr() }
    }

    pub fn framebuffer(&self) -> &[u8] {
        // SAFETY: as for the RAM: the beam paints the framebuffer only in a
        // call that borrows the machine mutably.
        unsafe { &*self.framebuffer.as_ptr() }
    }

    pub fn machine_mut(&mut self) -> &mut cpc_t {
        // SAFETY: as for machine(), and a write through it changes a field and
        // never moves the machine.
        unsafe { &mut *self.machine.as_ptr() }
    }
}

pub struct Spectrum {
    machine: Handed<spectrum_t>,
    ram: Handed<[u8]>,
    _rom: Handed<[u8; SPECTRUM_ROM_SIZE as usize]>,
    framebuffer: Handed<[u8]>,
    tape: Option<Rc<Tape>>,
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
            ram,
            _rom: rom,
            framebuffer,
            tape: None,
        }
    }

    // Opens the image as a Spectrum's tape, puts it in the deck in place of
    // the tape there, which is freed once neither the deck nor any state
    // holds it, and presses PLAY. A refused image leaves the deck as it was.
    pub fn insert_tape(&mut self, contents: &[u8]) -> Result<(), String> {
        let length = u32::try_from(contents.len()).expect("a tape of less than 4GB");
        let image = Handed::new(contents.to_vec().into_boxed_slice());
        // SAFETY: all-zero bytes are a value of every field of the deck and
        // the reader, whose source is then a function pointer of None.
        let deck = Handed::new(unsafe { Box::<Deck>::new_zeroed().assume_init() });
        let deck_at = deck.as_ptr();
        let mut sentence = std::ptr::null();
        // SAFETY: the reader borrows the image, which is held beside it and
        // freed with it, and reads no further than the length given.
        let opened = unsafe {
            tzx_open(
                &raw mut (*deck_at).reader,
                image.as_ptr().cast(),
                length,
                SPECTRUM_TICKS_PER_MILLISECOND,
                tzx_machine_TZX_SPECTRUM,
                &mut sentence,
            )
        };
        if !opened {
            return Err(problem(sentence));
        }
        // SAFETY: the deck points at its reader in the same allocation, and
        // the machine at the deck, which is held beside the machine for as
        // long as it or a state holds it.
        unsafe {
            tape_init(&raw mut (*deck_at).tape);
            tape_insert(
                &raw mut (*deck_at).tape,
                Some(tzx_next_pulse),
                (&raw mut (*deck_at).reader).cast(),
            );
            spectrum_insert_tape(self.machine.as_ptr(), &raw mut (*deck_at).tape);
            tape_play(&raw mut (*deck_at).tape);
        }
        self.tape = Some(Rc::new(Tape {
            deck,
            _image: image,
        }));
        Ok(())
    }

    // As a CPC's: the machine as it stands, its tape where it has played to.
    pub fn state(&self) -> SpectrumState {
        SpectrumState {
            // SAFETY: as for a CPC's.
            machine: Box::new(unsafe { std::ptr::read(self.machine()) }),
            ram: self.ram().into(),
            framebuffer: self.framebuffer().into(),
            tape: self.tape.as_ref().map(|tape| {
                // SAFETY: the deck stands beside the machine, which is not
                // running.
                (Rc::clone(tape), unsafe { *tape.deck.as_ptr() })
            }),
        }
    }

    // As a CPC's: the machine and the tape it held, where it had played to.
    pub fn restore(&mut self, state: &SpectrumState) {
        self.tape = state.tape.as_ref().map(|(tape, deck)| {
            // SAFETY: as for state(); the deck's pointers are into itself and
            // at the image held beside it.
            unsafe { *tape.deck.as_ptr() = *deck };
            Rc::clone(tape)
        });
        // SAFETY: as for a CPC's.
        *self.machine_mut() = unsafe { std::ptr::read(&*state.machine) };
        // SAFETY: as for a CPC's.
        unsafe {
            (*self.ram.as_ptr()).copy_from_slice(&state.ram);
            (*self.framebuffer.as_ptr()).copy_from_slice(&state.framebuffer);
        }
    }

    // The machine as a 48K snapshot, which leaves the machine as it stands:
    // the two bytes its program counter takes below the stack pointer are
    // the snapshot's alone.
    pub fn snapshot_save(&self) -> Result<Vec<u8>, String> {
        // SAFETY: the machine is only read.
        let size = unsafe { spectrum_snapshot_size(self.machine.as_ptr()) };
        let mut bytes = vec![0; size];
        let mut sentence = std::ptr::null();
        // SAFETY: as for a CPC's.
        let written = unsafe {
            spectrum_snapshot_save(
                self.machine.as_ptr(),
                bytes.as_mut_ptr(),
                size,
                &mut sentence,
            )
        };
        saved(bytes, written, sentence)
    }

    // Between instructions and owing the ULA nothing.
    pub fn instruction_complete(&self) -> bool {
        // SAFETY: the machine is only read.
        unsafe { spectrum_instruction_complete(self.machine.as_ptr()) }
    }

    pub fn run(&mut self, frames: u32, limit: u64, watch: Option<&mut Watch>) -> Run {
        // SAFETY: as for a CPC's run.
        unsafe { mcp_spectrum_run(self.machine.as_ptr(), frames, limit, watching(watch)) }
    }

    pub fn call(&mut self, address: u16) -> Result<(), String> {
        // SAFETY: the bridge pushes through spectrum_poke, which writes only to
        // the RAM the machine was handed.
        let sentence = unsafe { mcp_spectrum_call(self.machine.as_ptr(), address) };
        if sentence.is_null() {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    pub fn step(&mut self, instructions: u32) -> Run {
        // SAFETY: as for a run.
        unsafe { mcp_spectrum_step(self.machine.as_ptr(), instructions) }
    }

    pub fn peek(&self, address: u16) -> u8 {
        // SAFETY: the memory the machine reads stands where it was handed.
        unsafe { spectrum_peek(self.machine.as_ptr(), address) }
    }

    pub fn poke(&mut self, address: u16, value: u8) {
        // SAFETY: the processor writes only to the RAM it was handed.
        unsafe { spectrum_poke(self.machine.as_ptr(), address, value) }
    }

    pub fn snapshot_load(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut sentence = std::ptr::null();
        // SAFETY: as for a CPC's snapshot.
        let loaded = unsafe {
            spectrum_snapshot_load(
                self.machine.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                &mut sentence,
            )
        };
        if loaded {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    pub fn seek(&mut self) {
        // SAFETY: the machine stands where it was built.
        unsafe { mcp_spectrum_seek(self.machine.as_ptr()) }
    }

    pub fn check(&self, written: &spectrum_t) -> Result<(), String> {
        // SAFETY: both are only read.
        let sentence = unsafe { mcp_spectrum_check(self.machine.as_ptr(), written) };
        if sentence.is_null() {
            Ok(())
        } else {
            Err(problem(sentence))
        }
    }

    pub fn machine(&self) -> &spectrum_t {
        // SAFETY: as for a CPC's.
        unsafe { &*self.machine.as_ptr() }
    }

    pub fn ram(&self) -> &[u8] {
        // SAFETY: as for a CPC's.
        unsafe { &*self.ram.as_ptr() }
    }

    pub fn framebuffer(&self) -> &[u8] {
        // SAFETY: as for a CPC's.
        unsafe { &*self.framebuffer.as_ptr() }
    }

    pub fn machine_mut(&mut self) -> &mut spectrum_t {
        // SAFETY: as for a CPC's.
        unsafe { &mut *self.machine.as_ptr() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::{
        Z80_WAIT, cpc_insert_disc, floppy_add_track, floppy_layout_track, floppy_mount, floppy_t,
        spectrum_instruction_complete, upd765_phase_UPD765_PHASE_COMMAND,
        upd765_phase_UPD765_PHASE_EXECUTION, upd765_phase_UPD765_PHASE_RESULT,
        upd765_stage_UPD765_STAGE_FORMAT_IDENTITY, upd765_stage_UPD765_STAGE_FORMAT_TO_IDENTITY,
    };
    use crate::fields::{self, Fields};
    use serde_json::Value;

    const DI: u8 = 0xF3;
    const EI: u8 = 0xFB;
    const RET: u8 = 0xC9;
    const EX_SP_IX: [u8; 2] = [0xDD, 0xE3];
    const HALT: u8 = 0x76;
    const IM_1: [u8; 2] = [0xED, 0x56];
    const INC_BC: u8 = 0x03;
    const JR: u8 = 0x18;
    const LD_A: u8 = 0x3E;
    const LD_AT_HL_A: u8 = 0x77;
    const LD_BC: u8 = 0x01;
    const LD_HL: u8 = 0x21;
    const LD_I_A: [u8; 2] = [0xED, 0x47];
    const NOP: u8 = 0x00;
    const OUT_C_C: [u8; 2] = [0xED, 0x49];
    const PREFIX_DD: u8 = 0xDD;

    // A standard frame is 312 lines of 64µs, "exactly 19968 μsec" (Compendium
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
    // standing just after a retrace, halted for good: the first run stops at
    // the HALT, and those from there run as asked.
    fn cpc_on_the_standard_screen() -> Cpc {
        let mut cpc = Cpc::new(sixty_four_k(), rom(&standard_screen(), HALT));
        assert_eq!(cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None).frames, 0);
        assert_eq!(cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None).frames, 2);
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
            let step = cpc.run(1, 1, None);
            assert!(step.frames == 0 && step.ticks < 64, "a step ran {step:?}");
        }
        assert_eq!(cpc.machine().cpu.pc, start);
        (cpc, start)
    }

    // As cpc_on_the_standard_screen.
    fn spectrum_after_a_retrace() -> Spectrum {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[DI], HALT));
        assert_eq!(spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None).frames, 0);
        assert_eq!(spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None).frames, 2);
        spectrum
    }

    // A frame retrace sends the beam to the top-left corner, as the core's
    // src/monitor.c does it.
    #[test]
    fn a_cpc_run_by_the_frame_stops_at_each_retrace() {
        let mut cpc = cpc_on_the_standard_screen();
        for _ in 0..3 {
            let run = cpc.run(1, 2 * CPC_STANDARD_FRAME_TICKS, None);
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
            let run = spectrum.run(1, 2 * SPECTRUM_FRAME_TICKS, None);
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
        let half = cpc.run(2, CPC_STANDARD_FRAME_TICKS / 2, None);
        assert_eq!(half.frames, 0);
        let (mut ticks, mut frames) = (half.ticks, 0);
        while ticks < 3 * CPC_STANDARD_FRAME_TICKS {
            let run = cpc.run(1, 1, None);
            ticks += run.ticks;
            frames += run.frames;
        }
        assert_eq!(frames, 3);
    }

    #[test]
    fn a_spectrum_run_cut_short_misses_no_retrace() {
        let mut spectrum = spectrum_after_a_retrace();
        let half = spectrum.run(2, SPECTRUM_FRAME_TICKS / 2, None);
        assert_eq!(half.frames, 0);
        let (mut ticks, mut frames) = (half.ticks, 0);
        while ticks < 3 * SPECTRUM_FRAME_TICKS {
            let run = spectrum.run(1, 1, None);
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
        let nothing = spectrum.run(1, 0, None);
        assert_eq!(
            (nothing.ticks, nothing.frames, nothing.settled),
            (0, 0, true)
        );
        let load = spectrum.run(1, 1, None);
        assert_eq!((load.ticks, load.frames, load.settled), (10, 0, true));
        let processor = &spectrum.machine().cpu;
        assert_eq!((processor.b, processor.c, processor.pc), (0x12, 0x34, 3));
        let exchange = spectrum.run(1, 1, None);
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
        let nothing = cpc.run(1, 0, None);
        assert_eq!(
            (nothing.ticks, nothing.frames, nothing.settled),
            (0, 0, true)
        );
        let load = cpc.run(1, 1, None);
        assert_eq!((load.frames, load.settled), (0, true));
        let processor = &cpc.machine().cpu;
        assert_eq!(
            (processor.b, processor.c, processor.pc),
            (0x12, 0x34, start + 3)
        );
        let exchange = cpc.run(1, 1, None);
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
            spectrum.run(1, 1, None);
        }
        let mut ticks = 0;
        let mut charged = 0;
        while ticks < 2 * SPECTRUM_FRAME_TICKS {
            let run = spectrum.run(1, 1, None);
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
        assert!(!cpc.run(1, 1, None).settled);
    }

    #[test]
    fn a_spectrum_caught_in_a_chain_of_prefixes_ends_unsettled() {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[], PREFIX_DD));
        assert!(!spectrum.run(1, 1, None).settled);
    }

    // LD BC,nn takes 10 T-states, EX (SP),IX 23 and NOP 4 (UM0080), from the
    // ROM, where nothing is contended.
    #[test]
    fn a_spectrum_steps_an_instruction_at_a_time() {
        let program = [LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1], NOP];
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        let steps: Vec<(u64, u32, bool)> = (0..3)
            .map(|_| {
                let step = spectrum.step(1);
                (step.ticks, step.frames, step.settled)
            })
            .collect();
        assert_eq!(steps, [(10, 0, true), (23, 0, true), (4, 0, true)]);
        assert_eq!(spectrum.machine().cpu.pc, 6);
    }

    // On a CPC the Gate Array stretches every instruction to whole
    // microseconds: "NOP 1", "LD rp,nnnn 3" and "EX (SP),IX 7" in the
    // measured table of Kevin Thacker's cpctech,
    // https://cpctech.cpcwiki.de/docs/instrtim.html. An instruction's end is
    // ragged against those microseconds, so the span measured runs from the
    // end of one NOP to the end of another, which end alike.
    #[test]
    fn a_cpc_steps_an_instruction_at_a_time() {
        let program = [NOP, LD_BC, 0x34, 0x12, EX_SP_IX[0], EX_SP_IX[1], NOP];
        let mut cpc = Cpc::new(sixty_four_k(), rom(&program, HALT));
        cpc.step(1);
        let mut ticks = 0;
        for pc in [4, 6, 7] {
            let step = cpc.step(1);
            assert!(step.settled);
            assert_eq!(cpc.machine().cpu.pc, pc);
            ticks += step.ticks;
        }
        assert_eq!(ticks, (3 + 7 + 1) * 4);
    }

    #[test]
    fn a_step_of_several_instructions_is_those_steps_one_at_a_time() {
        let program = [DI, LD_A, 0x40, LD_I_A[0], LD_I_A[1], INC_BC, JR, 0xFD];
        let mut whole = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        let mut split = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        for instructions in [1, 7, 1000, 25_000] {
            let at_once = whole.step(instructions);
            let (mut ticks, mut frames) = (0, 0);
            for _ in 0..instructions {
                let step = split.step(1);
                ticks += step.ticks;
                frames += step.frames;
            }
            assert_eq!((at_once.ticks, at_once.frames), (ticks, frames));
            assert_eq!(whole.machine().cpu.pc, split.machine().cpu.pc);
        }
    }

    // "When a software HALT instruction is executed, the CPU executes NOPs
    // until an interrupt is received", and "each cycle in the HALT state is
    // a normal M1 (fetch) cycle" (UM0080, HALT Exit): a NOP's four T-states.
    #[test]
    fn a_halted_processor_steps_by_the_fetches_it_idles_on() {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[DI, HALT], NOP));
        spectrum.step(2);
        assert!(spectrum.machine().cpu.halted);
        let halted_at = spectrum.machine().cpu.pc;
        for _ in 0..3 {
            let step = spectrum.step(1);
            assert_eq!((step.ticks, step.settled), (4, true));
            assert_eq!(spectrum.machine().cpu.pc, halted_at);
        }
    }

    // HALT with IFF1 clear waits for good, as neither machine raises NMI: a
    // run stops at the end of it, watched or not, and one that begins there
    // runs as asked. With IFF1 set, an interrupt wakes the processor and the
    // run goes on. DI, NOP and HALT take four T-states each (UM0080), and a
    // CPC stops where a twin stands once stepped through the three.
    #[test]
    fn a_run_stops_where_its_processor_halts_for_good() {
        let elsewhere = [Condition {
            kind: EXECUTE,
            first: 0x8000,
            last: 0x8000,
            mask: 0,
            list: UNTIL,
        }];
        for watched in [false, true] {
            // SAFETY: all-zero bytes are a value of every field of a match.
            let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; 16];
            let mut watch = Watch::new(&elsewhere, &mut matches, &mut [], &mut [], 1, 0);
            let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[DI, NOP, HALT], NOP));
            let run = spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, watched.then_some(&mut watch));
            assert_eq!((run.ticks, run.frames, run.settled), (12, 0, true));
            assert!(spectrum.machine().cpu.halted_for_good());
            assert_eq!(spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None).frames, 2);
        }
        let mut cpc = Cpc::new(sixty_four_k(), rom(&[DI, NOP, HALT], NOP));
        let mut twin = Cpc::new(sixty_four_k(), rom(&[DI, NOP, HALT], NOP));
        let run = cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None);
        assert_eq!(
            (run.ticks, run.frames, run.settled),
            (twin.step(3).ticks, 0, true)
        );
        assert!(cpc.machine().cpu.halted_for_good());
        let program = [IM_1[0], IM_1[1], EI, HALT];
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, NOP));
        assert_eq!(spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None).frames, 2);
    }

    // In mode 1 the processor answers an interrupt with a restart at &0038,
    // and "the number of cycles required to complete the restart instruction
    // is two more than normal due to the two added wait states" (UM0080,
    // Interrupt Response): an RST's 11 T-states and two, 13. JR takes 12, so
    // the step at whose end the ULA's interrupt is taken lands in the
    // handler, 25 T-states after it began.
    #[test]
    fn a_step_answers_an_interrupt_taken_at_its_end() {
        let program = [IM_1[0], IM_1[1], EI, JR, 0xFE];
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        spectrum.step(2);
        let mut ticks = 0;
        while ticks < 2 * SPECTRUM_FRAME_TICKS {
            let step = spectrum.step(1);
            ticks += step.ticks;
            if spectrum.machine().cpu.pc == 0x0038 {
                assert_eq!((step.ticks, step.settled), (12 + 13, true));
                return;
            }
            assert_eq!((step.ticks, spectrum.machine().cpu.pc), (12, 3));
        }
        panic!("no interrupt was taken in two frames");
    }

    #[test]
    fn a_step_caught_in_a_chain_of_prefixes_stops_unsettled() {
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&[], PREFIX_DD));
        assert!(!spectrum.step(3).settled);
        let (mut cpc, _) = cpc_past_the_standard_screen(&[], PREFIX_DD);
        assert!(!cpc.step(3).settled);
    }

    // A program polls VSYNC on the PPI's port B, at &F5xx, with IN A,(C), and
    // writes what it read. A CPC holds IN r,(C) over several ticks and answers
    // it again on each, and VSYNC can change among them; each port read
    // gathered gives the byte the processor took, which the write after it
    // shows.
    #[test]
    fn a_port_read_gathered_gives_the_byte_the_processor_took() {
        let polling = [
            LD_BC, 0x00, 0xF5, LD_HL, 0x00, 0x40, IN_A_C[0], IN_A_C[1], LD_AT_HL_A, NOP, NOP, JR,
            0xF9,
        ];
        let (mut cpc, _) = cpc_past_the_standard_screen(&polling, HALT);
        let gathering =
            [(IN, 0xF500, 0x0800), (WRITE, 0x4000, 0)].map(|(kind, first, mask)| Condition {
                kind,
                first,
                last: first,
                mask,
                list: GATHER,
            });
        // SAFETY: all-zero bytes are a value of every field of a match.
        let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; 400_000];
        let mut watch = Watch::new(&gathering, &mut matches, &mut [], &mut [], u32::MAX, 0);
        cpc.run(60, 120 * CPC_STANDARD_FRAME_TICKS, Some(&mut watch));
        let count = watch.seen().match_count as usize;
        assert!(count < matches.len(), "{count} matches");
        let (mut pairs, mut changes, mut previous) = (0, 0, None);
        for pair in matches[..count].chunks(2) {
            let [read, written] = pair else { break };
            assert_eq!((read.kind, written.kind), (IN, WRITE));
            assert_eq!(read.value, written.value, "at tick {}", read.tick);
            changes += usize::from(previous.is_some_and(|value| value != read.value));
            previous = Some(read.value);
            pairs += 1;
        }
        assert!(
            pairs > 100_000 && changes > 100,
            "{pairs} reads, {changes} changes"
        );
    }

    // A gathering that fills at the end of an instruction the processor takes
    // an interrupt at keeps the response's two pushes as well, and the run
    // stops where every run stops, at the handler's first fetch. From &0100
    // on, every instruction is LD (HL),A, so an interrupt is taken at the end
    // of a write; the first run finds which, and its twin fills there.
    #[test]
    fn a_full_gathering_keeps_the_response_to_an_interrupt_taken_as_it_fills() {
        let mut program = vec![LD_SP, 0x00, 0x80, LD_HL, 0x00, 0x90, IM_1[0], IM_1[1], EI];
        program.extend([JP, 0x00, 0x01]);
        program.resize(0x38, NOP);
        program.extend([EI, RET]);
        let writes = [Condition {
            kind: WRITE,
            first: 0x7FFE,
            last: 0x9000,
            mask: 0,
            list: GATHER,
        }];
        // SAFETY: all-zero bytes are a value of every field of a match.
        let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; 100_000];
        let mut finding = Spectrum::new(forty_eight_k(), rom(&program, LD_AT_HL_A));
        let mut watch = Watch::new(&writes, &mut matches, &mut [], &mut [], u32::MAX, 0);
        finding.run(2, 4 * SPECTRUM_FRAME_TICKS, Some(&mut watch));
        let count = watch.seen().match_count as usize;
        let followed = matches[..count]
            .iter()
            .position(|seen| seen.address != 0x9000)
            .expect("an interrupt's push");
        assert!(followed > 0 && matches[followed + 1].response);
        let mut filling = Spectrum::new(forty_eight_k(), rom(&program, LD_AT_HL_A));
        let mut watch = Watch::new(&writes, &mut matches, &mut [], &mut [], followed as u32, 0);
        let run = filling.run(2, 4 * SPECTRUM_FRAME_TICKS, Some(&mut watch));
        let seen = *watch.seen();
        assert!(seen.full && run.settled);
        assert_eq!(seen.match_count as usize, followed + 2);
        assert_eq!(filling.machine().cpu.pc, 0x0038);
    }

    // A chain of DD prefixes is one instruction, since "no interrupts are
    // accepted during the execution of these sequences" ("The Undocumented Z80
    // Documented", Sean Young, 5.5), and a place keeps the first four bytes of
    // it; a fifth would be written past them, where `make test-sanitized`
    // sees it.
    #[test]
    fn a_place_keeps_the_first_four_bytes_of_a_chain_of_prefixes() {
        let program = [PREFIX_DD; 5]
            .into_iter()
            .chain([LD_HL, 0x34, 0x12, DI, HALT])
            .collect::<Vec<_>>();
        let mut spectrum = Spectrum::new(forty_eight_k(), rom(&program, HALT));
        let everywhere = [Condition {
            kind: EXECUTE,
            first: 0,
            last: 0xFFFF,
            mask: 0,
            list: COUNT,
        }];
        let mut counts = vec![0; KINDS.len() * COUNTS_PER_KIND];
        // SAFETY: all-zero bytes are a value of every field of a place.
        let mut places = vec![unsafe { std::mem::zeroed::<Place>() }; 3];
        let mut watch = Watch::new(&everywhere, &mut [], &mut counts, &mut places, 0, u32::MAX);
        spectrum.run(1, SPECTRUM_FRAME_TICKS, Some(&mut watch));
        assert_eq!(watch.seen().counted, 3);
        assert_eq!((places[0].length, places[0].bytes), (4, [PREFIX_DD; 4]));
    }

    // Every value set could write, swept across machines kept busy with every
    // chip the core trusts a field of, under the sanitizers: whatever the
    // check lets through must run without reading or writing out of bounds.
    // Run by `make test-sanitized`, where the core is built with
    // AddressSanitizer and UBSan's bounds checks; elsewhere it shows only that
    // nothing crashes outright. 4 is the step at which z80.c begins a memory
    // read, which z80_reading_operand answers from the cycle's place in the
    // micro-program.
    const EXTREMES: [u64; 18] = [
        0,
        1,
        4,
        7,
        8,
        9,
        15,
        16,
        17,
        28,
        29,
        30,
        255,
        256,
        65535,
        65536,
        u32::MAX as u64,
        u64::MAX,
    ];

    // What a value could be set to: each extreme of a number, both truths,
    // every enumerator, each in every context given. A context reaches the
    // single values under its prefix, and holds them to their extremes with
    // its own values written beside.
    fn trials(encoded: &Value, contexts: &[(&str, &[(&str, Value)])]) -> Vec<Vec<(String, Value)>> {
        let mut values = Vec::new();
        fields::leaves(encoded, "", &mut values);
        let mut trials = Vec::new();
        for (path, value) in values {
            let candidates: Vec<Value> = match value {
                Value::Bool(_) => vec![Value::from(false), Value::from(true)],
                Value::String(_) => fields::ENUMERATORS
                    .iter()
                    .map(|name| Value::from(*name))
                    .collect(),
                _ => EXTREMES
                    .iter()
                    .map(|extreme| Value::from(*extreme))
                    .collect(),
            };
            for (prefix, context) in contexts {
                let reached =
                    path.starts_with(prefix) && (prefix.is_empty() || !path.contains('['));
                if !reached {
                    continue;
                }
                for candidate in &candidates {
                    let mut trial: Vec<(String, Value)> = context
                        .iter()
                        .map(|(path, value)| ((*path).to_owned(), value.clone()))
                        .collect();
                    trial.push((path.clone(), candidate.clone()));
                    trials.push(trial);
                }
            }
        }
        trials
    }

    // Each trial is written to the machine as it stood when the sweep began,
    // and run half a frame.
    fn sweep<T: Fields>(
        standing: &T,
        trials: &[Vec<(String, Value)>],
        mut attempt: impl FnMut(Option<T>) -> bool,
    ) -> (usize, usize) {
        let (mut ran, mut checked_out) = (0, 0);
        for trial in trials {
            // SAFETY: a machine's struct owns nothing; the copy is written to
            // and handed on, or dropped.
            let mut written = unsafe { std::ptr::read(standing) };
            let typed = trial
                .iter()
                .all(|(path, value)| written.set(&fields::parse(path).unwrap(), value).is_ok());
            let accepted = attempt(typed.then_some(written));
            if typed && accepted {
                ran += 1;
            } else if typed {
                checked_out += 1;
            }
        }
        (ran, checked_out)
    }

    const IN_A_C: [u8; 2] = [0xED, 0x78];
    const IN_A_N: u8 = 0xDB;
    const JP: u8 = 0xC3;
    const LD_A_FROM: u8 = 0x3A;
    const LD_A_TO: u8 = 0x32;
    const LD_SP: u8 = 0x31;
    const OUT_C_A: [u8; 2] = [0xED, 0x79];
    const OUT_N_A: u8 = 0xD3;
    const POP_HL: u8 = 0xE1;
    const PUSH_HL: u8 = 0xE5;

    // Reads, writes and the stack in every quarter of the processor's
    // memory, so that RAM paged wrongly is reached.
    fn every_quarter(program: &mut Vec<u8>) {
        for address in [0x0010u16, 0x4001, 0x8001, 0xC001] {
            let [low, high] = address.to_le_bytes();
            program.extend([LD_A_FROM, low, high, LD_A_TO, low, high]);
        }
        program.extend([PUSH_HL, POP_HL]);
    }

    // The standard screen, and then every chip's work in an order that uses
    // what was selected before it selects anew, so that a selection written
    // from outside is used: an ink given to the pen standing, then a pen
    // chosen; the second 64K paged in and memory read and written through
    // it, then paged out; a 6845 register written, then one selected; a
    // sound register written and read, then one selected; seven bytes read
    // from the controller and a READ ID for drive 0 sent it, with the
    // drive's motor on. Then round again.
    fn cpc_workload() -> (Vec<u8>, u16) {
        let mut program = standard_screen();
        program.extend([LD_SP, 0x00, 0xC0]);
        let looped = u16::try_from(program.len()).unwrap();
        let out = |program: &mut Vec<u8>, port: u16, value: u8| {
            let [_, high] = port.to_le_bytes();
            program.extend([LD_BC, value, high, OUT_C_C[0], OUT_C_C[1]]);
        };
        out(&mut program, 0x7F00, 0x54); // an ink for the pen standing
        out(&mut program, 0x7F00, 0x00); // pen 0
        out(&mut program, 0x7F00, 0xC4); // RAM configuration 4
        every_quarter(&mut program);
        out(&mut program, 0x7F00, 0xC0); // configuration 0
        every_quarter(&mut program);
        out(&mut program, 0xBD00, 0x30); // the register standing
        out(&mut program, 0xBC00, 12); // R12
        out(&mut program, 0xF400, 0x3F); // a value
        out(&mut program, 0xF600, 0x80); // written to the register standing
        out(&mut program, 0xF600, 0x00);
        out(&mut program, 0xF700, 0x92); // port A in
        out(&mut program, 0xF600, 0x40); // read
        out(&mut program, 0xF700, 0x82); // port A out
        out(&mut program, 0xF600, 0x00);
        out(&mut program, 0xF400, 7); // register 7
        out(&mut program, 0xF600, 0xC0); // selected
        out(&mut program, 0xF600, 0x00);
        out(&mut program, 0xFA00, 0x01); // motor on
        program.extend([LD_BC, 0x7F, 0xFB]); // the controller's data register
        for _ in 0..7 {
            program.extend(IN_A_C);
        }
        program.extend([LD_A, 0x0A, OUT_C_A[0], OUT_C_A[1]]); // READ ID
        program.extend([LD_A, 0x00, OUT_C_A[0], OUT_C_A[1]]); // drive 0
        let [low, high] = looped.to_le_bytes();
        program.extend([JP, low, high]);
        (program, looped)
    }

    // Borders, the screen in contended memory, every quarter of memory, and
    // the ULA's port read.
    fn spectrum_workload() -> Vec<u8> {
        let mut program = vec![DI, LD_SP, 0x00, 0xFF];
        program.extend([LD_A, 0x07, OUT_N_A, 0xFE]);
        every_quarter(&mut program);
        program.extend([IN_A_N, 0xFE]);
        let back = 4 - i64::try_from(program.len() + 2).unwrap();
        program.extend([JR, back as u8]);
        program
    }

    // A disc of one formatted track, so that the controller's execution
    // runs on a drive that is ready rather than ending at once.
    struct Disc {
        floppy: Box<floppy_t>,
        _image: Box<[u8]>,
    }

    impl Disc {
        fn formatted() -> Disc {
            let mut image = vec![0; 0x20000].into_boxed_slice();
            // SAFETY: as for a machine, all-zero bytes are a floppy_t, which
            // floppy_mount then lays over an image that outlives it.
            let mut floppy = unsafe { Box::<floppy_t>::new_zeroed().assume_init() };
            // SAFETY: the image is borrowed for as long as the disc stands.
            unsafe {
                floppy_mount(&mut *floppy, image.as_mut_ptr(), image.len());
                assert!(floppy_add_track(&mut *floppy, 0, 0, 0, 0x10000, 0x4E, 0xE5));
                floppy_layout_track(&mut *floppy, 0, 0);
            }
            Disc {
                floppy,
                _image: image,
            }
        }
    }

    // A CPC running the workload with a disc in drive A, stood at the head of
    // its loop: the first time, before any disc command, so that the
    // controller has chosen no drive; or a quarter of a frame before a
    // retrace, after some.
    fn busy_cpc(ram: Box<[u8]>, disc: &mut Disc, before_any_command: bool) -> Cpc {
        let (workload, looped) = cpc_workload();
        let mut cpc = Cpc::new(ram, rom(&workload, HALT));
        cpc.fit_disc_interface(true);
        // SAFETY: the disc outlives the machine, which the caller holds it
        // beside.
        unsafe { cpc_insert_disc(cpc.machine_mut(), 0, &mut *disc.floppy) };
        if !before_any_command {
            cpc.run(3, 8 * CPC_STANDARD_FRAME_TICKS, None);
            cpc.run(u32::MAX, CPC_STANDARD_FRAME_TICKS * 3 / 4, None);
        }
        while cpc.machine().cpu.pc != looped {
            cpc.step(1);
        }
        assert_eq!(cpc.machine().fdc.drive.is_null(), before_any_command);
        cpc
    }

    // An image of one track holding one sector, laid out as "Extended DiSK
    // image definition" (Kevin Thacker's cpctech,
    // https://cpctech.cpcwiki.de/docs/extdsk.html) lays one out, which the
    // controller's READ ID finds.
    fn one_track_image(filler: u8) -> Vec<u8> {
        let mut image = vec![0; 0x100];
        image[..0x22].copy_from_slice(b"EXTENDED CPC DSK File\r\nDisk-Info\r\n");
        image[0x30] = 1;
        image[0x31] = 1;
        image[0x34] = 3;
        let mut track = vec![0; 0x100];
        track[..0x0C].copy_from_slice(b"Track-Info\r\n");
        track[0x14] = 2;
        track[0x15] = 1;
        track[0x16] = 0x4E;
        track[0x17] = filler;
        track[0x18..0x20].copy_from_slice(&[0, 0, 0xC1, 2, 0, 0, 0x00, 0x02]);
        track.resize(0x300, filler);
        image.extend(track);
        image
    }

    // Under the sanitizers, a disc or a tape given in place of another, or
    // refused, leaves nothing pointing at what was freed: the machine runs on,
    // reading the drive or the deck, after each.
    #[test]
    fn a_disc_or_a_tape_replaced_is_freed_once_nothing_points_at_it() {
        let (workload, _) = cpc_workload();
        let mut cpc = Cpc::new(sixty_four_k(), rom(&workload, HALT));
        cpc.fit_disc_interface(true);
        for image in [
            one_track_image(0xE5),
            one_track_image(0x00),
            vec![0; 0x100],
            one_track_image(0xE5),
        ] {
            let _ = cpc.insert_disc(0, &image);
            cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None);
        }
        assert!(!cpc.machine().fdc.drive.is_null());
        // The room to format every track again is the command line's.
        let command_line = std::fs::read_to_string(
            std::path::Path::new(env!("COLOPHON_MCP_EMULATOR_DIR")).join("cli/main.c"),
        )
        .expect("the command line's source");
        assert!(command_line.contains(
            "#define FORMAT_ROOM ((size_t)FLOPPY_MAX_CYLINDERS * FLOPPY_MAX_SIDES * \
             FLOPPY_BYTES_PER_REVOLUTION)"
        ));
        let disc = cpc.discs[0].as_ref().expect("a disc in drive 0");
        // SAFETY: the medium stands beside the machine, which is not running.
        let capacity = unsafe { (*disc.floppy.as_ptr()).image_capacity };
        assert_eq!(capacity, one_track_image(0xE5).len() + FORMAT_ROOM);
        let mut spectrum = Spectrum::new(
            vec![0; SPECTRUM_RAM_48K as usize].into_boxed_slice(),
            rom(&spectrum_workload(), HALT),
        );
        // One block of a flag, a byte and its checksum, "calculated such that
        // XORing all the data bytes together (including the flag byte)
        // produces 0x00" (16K/48K reference), after its length.
        let tape = [3, 0, 0xFF, 0x01, 0xFE];
        for image in [&tape[..], &tape, &[1, 0], &tape] {
            let _ = spectrum.insert_tape(image);
            spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None);
        }
        assert!(!spectrum.machine().tape.is_null());
    }

    // Under the sanitizers, a state kept with a disc in the drive, or a tape
    // in the deck, puts it back though another has taken its place since,
    // and the machine runs on, reading the drive or the deck, once the state
    // itself is dropped.
    #[test]
    fn a_state_keeps_its_disc_and_its_tape_through_a_swap() {
        let (workload, _) = cpc_workload();
        let mut cpc = Cpc::new(sixty_four_k(), rom(&workload, HALT));
        cpc.fit_disc_interface(true);
        cpc.insert_disc(0, &one_track_image(0xE5)).unwrap();
        cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None);
        let state = cpc.state();
        let kept = Rc::clone(cpc.discs[0].as_ref().expect("a disc"));
        // SAFETY: the medium and its image stand beside the machine, which is
        // not running.
        let held = |cpc: &Cpc| unsafe {
            let disc = cpc.discs[0].as_ref().expect("a disc");
            (
                (*disc.floppy.as_ptr()).write_protected,
                (*disc.image.as_ptr())[0x100],
            )
        };
        let as_kept = held(&cpc);
        // SAFETY: as above.
        unsafe {
            (*kept.floppy.as_ptr()).write_protected = !as_kept.0;
            (*kept.image.as_ptr())[0x100] = !as_kept.1;
        }
        cpc.insert_disc(0, &one_track_image(0x00)).unwrap();
        cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None);
        cpc.restore(&state);
        drop(state);
        let restored = cpc.discs[0].as_ref().expect("a disc");
        assert!(Rc::ptr_eq(restored, &kept));
        assert_eq!(cpc.machine().drives[0].floppy, kept.floppy.as_ptr());
        assert_eq!(held(&cpc), as_kept);
        cpc.run(2, 4 * CPC_STANDARD_FRAME_TICKS, None);
        let mut spectrum = Spectrum::new(
            vec![0; SPECTRUM_RAM_48K as usize].into_boxed_slice(),
            rom(&spectrum_workload(), HALT),
        );
        let tape = [3, 0, 0xFF, 0x01, 0xFE];
        spectrum.insert_tape(&tape).unwrap();
        spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None);
        let state = spectrum.state();
        let kept = Rc::clone(spectrum.tape.as_ref().expect("a tape"));
        // SAFETY: the deck stands beside the machine, which is not running.
        let played = unsafe { (*kept.deck.as_ptr()).tape.ticks_left };
        spectrum.insert_tape(&tape).unwrap();
        spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None);
        // SAFETY: as above.
        unsafe { (*kept.deck.as_ptr()).tape.ticks_left = played + 1 };
        spectrum.restore(&state);
        drop(state);
        assert!(Rc::ptr_eq(spectrum.tape.as_ref().expect("a tape"), &kept));
        // SAFETY: only the address is taken.
        let deck = unsafe { &raw mut (*kept.deck.as_ptr()).tape };
        assert_eq!(spectrum.machine().tape, deck);
        // SAFETY: as above.
        assert_eq!(unsafe { (*kept.deck.as_ptr()).tape.ticks_left }, played);
        spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None);
    }

    // Runs with a watch on every instruction, every access and every port,
    // gathering without end into room for a few and counting each, at room
    // for a few places, and answers whether the run saw more than there was
    // room for: matches past it are counted and never kept, or kept round
    // and round where the last are gathered, and places past it counted and
    // never kept. Every other run gathers the last and counts by pc.
    // The room is on the heap, where AddressSanitizer sees past its end.
    fn watching_everything(last: bool, run: impl FnOnce(Option<&mut Watch>) -> Run) -> bool {
        let conditions: Vec<Condition> = [GATHER, COUNT]
            .into_iter()
            .flat_map(|list| {
                KINDS.map(|kind| Condition {
                    kind,
                    first: 0,
                    last: 0xFFFF,
                    mask: 0,
                    list,
                })
            })
            .collect();
        // SAFETY: all-zero bytes are a value of every field of a match.
        let mut matches = vec![unsafe { std::mem::zeroed::<Match>() }; 4];
        let mut counts = vec![0; KINDS.len() * COUNTS_PER_KIND];
        // SAFETY: all-zero bytes are a value of every field of a place.
        let mut places = vec![unsafe { std::mem::zeroed::<Place>() }; 4];
        let mut watch = Watch::new(
            &conditions,
            &mut matches,
            &mut counts,
            &mut places,
            u32::MAX,
            u32::MAX,
        );
        if last {
            watch.gather_last();
            watch.count_by_pc();
        }
        run(Some(&mut watch));
        let seen = watch.seen();
        seen.match_count > seen.room && seen.counted > seen.place_room
    }

    fn sweep_cpc(cpc: &mut Cpc, contexts: &[(&str, &[(&str, Value)])]) -> (usize, usize) {
        // SAFETY: as in sweep.
        let standing = unsafe { std::ptr::read(cpc.machine()) };
        let trials = trials(&standing.encode(), contexts);
        let (mut watched, mut overflowed) = (0, 0);
        let (ran, checked_out) = sweep(&standing, &trials, |written| {
            // SAFETY: as in sweep; restored into the machine it was taken from.
            *cpc.machine_mut() = unsafe { std::ptr::read(&standing) };
            let Some(written) = written else { return false };
            if cpc.check(&written).is_err() {
                return false;
            }
            *cpc.machine_mut() = written;
            cpc.remap();
            watched += 1;
            if watching_everything(watched % 2 == 0, |watch| {
                cpc.run(u32::MAX, CPC_STANDARD_FRAME_TICKS / 2, watch)
            }) {
                overflowed += 1;
            }
            true
        });
        assert!(overflowed > ran / 2, "{overflowed} of {ran} overflowed");
        (ran, checked_out)
    }

    #[test]
    #[ignore = "under the sanitizers: make test-sanitized"]
    fn nothing_the_check_lets_through_runs_out_of_bounds_on_a_cpc() {
        let phase = |name: &str| ("fdc.phase", Value::from(name));
        let stage = |name: &str| ("fdc.stage", Value::from(name));
        let execution = phase("UPD765_PHASE_EXECUTION");
        let contexts: [(&str, &[(&str, Value)]); 6] = [
            ("", &[]),
            ("fdc.", &[phase("UPD765_PHASE_COMMAND")]),
            ("fdc.", &[phase("UPD765_PHASE_RESULT")]),
            (
                "fdc.",
                &[execution.clone(), stage("UPD765_STAGE_FORMAT_TO_IDENTITY")],
            ),
            (
                "fdc.",
                &[execution.clone(), stage("UPD765_STAGE_FORMAT_IDENTITY")],
            ),
            ("fdc.", &[execution, stage("UPD765_STAGE_DATA")]),
        ];
        let mut disc = Disc::formatted();
        let mut busy = busy_cpc(sixty_four_k(), &mut disc, false);
        let (ran, checked_out) = sweep_cpc(&mut busy, &contexts);
        assert!(
            ran > 5000 && checked_out > 500,
            "{ran} ran, {checked_out} checked out"
        );
        let mut disc = Disc::formatted();
        let mut fresh = busy_cpc(sixty_four_k(), &mut disc, true);
        let (ran, checked_out) = sweep_cpc(&mut fresh, &contexts[..1]);
        assert!(
            ran > 4000 && checked_out > 50,
            "{ran} ran, {checked_out} checked out"
        );
        let mut disc = Disc::formatted();
        let mut larger = busy_cpc(vec![0; 0x20000].into_boxed_slice(), &mut disc, false);
        let (ran, checked_out) = sweep_cpc(&mut larger, &contexts[..1]);
        assert!(
            ran > 4000 && checked_out > 50,
            "{ran} ran, {checked_out} checked out"
        );
    }

    #[test]
    #[ignore = "under the sanitizers: make test-sanitized"]
    fn nothing_the_check_lets_through_runs_out_of_bounds_on_a_spectrum() {
        for size in [SPECTRUM_RAM_16K, SPECTRUM_RAM_48K] {
            let ram = vec![0; size as usize].into_boxed_slice();
            let mut spectrum = Spectrum::new(ram, rom(&spectrum_workload(), HALT));
            spectrum.run(2, 4 * SPECTRUM_FRAME_TICKS, None);
            spectrum.run(u32::MAX, SPECTRUM_FRAME_TICKS * 3 / 4, None);
            // SAFETY: as in sweep.
            let standing = unsafe { std::ptr::read(spectrum.machine()) };
            let trials = trials(&standing.encode(), &[("", &[])]);
            let (mut watched, mut overflowed) = (0, 0);
            let (ran, checked_out) = sweep(&standing, &trials, |written| {
                // SAFETY: as for a CPC.
                *spectrum.machine_mut() = unsafe { std::ptr::read(&standing) };
                let Some(written) = written else { return false };
                if spectrum.check(&written).is_err() {
                    return false;
                }
                *spectrum.machine_mut() = written;
                spectrum.seek();
                watched += 1;
                if watching_everything(watched % 2 == 0, |watch| {
                    spectrum.run(u32::MAX, SPECTRUM_FRAME_TICKS / 2, watch)
                }) {
                    overflowed += 1;
                }
                true
            });
            assert!(
                overflowed > ran / 2,
                "{size}: {overflowed} of {ran} overflowed"
            );
            assert!(
                ran > 1000 && checked_out > 50,
                "{size}: {ran} ran, {checked_out} checked out"
            );
        }
    }

    // Each bound the check holds a written machine to, met and then passed by
    // one, as the core trusts it.
    #[test]
    fn the_check_holds_a_cpc_to_every_bound_the_core_trusts() {
        let mut cpc = Cpc::new(sixty_four_k(), rom(&[], HALT));
        cpc.fit_disc_interface(true);
        let check = |change: &dyn Fn(&mut cpc_t)| {
            // SAFETY: a machine's struct owns nothing, and the copy is only
            // written and checked.
            let mut written = unsafe { std::ptr::read(cpc.machine()) };
            change(&mut written);
            cpc.check(&written)
        };
        let refused = |change: &dyn Fn(&mut cpc_t), field: &str| {
            let problem = check(change).unwrap_err();
            assert!(problem.starts_with(field), "{problem}");
        };
        assert_eq!(check(&|_| {}), Ok(()));
        refused(&|cpc| cpc.ram_size = 0x20000, "ram_size");
        assert_eq!(check(&|cpc| cpc.mmr = 0xF8), Ok(()));
        refused(&|cpc| cpc.mmr = 0xC4, "mmr");
        assert_eq!(check(&|cpc| cpc.gate_array.pen = 16), Ok(()));
        refused(&|cpc| cpc.gate_array.pen = 17, "gate_array.pen");
        assert_eq!(check(&|cpc| cpc.psg.selected = 15), Ok(()));
        refused(&|cpc| cpc.psg.selected = 16, "psg.selected");
        refused(&|cpc| cpc.monitor.width += 1, "monitor.width");
        refused(&|cpc| cpc.monitor.height -= 1, "monitor.width");
        assert_eq!(check(&|cpc| cpc.cpu.pc = 0x4000), Ok(()));
        refused(&|cpc| cpc.cpu.program_length = 1, "cpu.program_length");
        refused(&|cpc| cpc.cpu.program_index = 1, "cpu.program_length");
        let command = |received: u8, length: u8| {
            move |cpc: &mut cpc_t| {
                cpc.fdc.phase = upd765_phase_UPD765_PHASE_COMMAND;
                cpc.fdc.command_received = received;
                cpc.fdc.command_length = length;
            }
        };
        assert_eq!(check(&command(8, 9)), Ok(()));
        refused(&command(9, 9), "fdc.command_received");
        refused(&command(9, 10), "fdc.command_received");
        let result = |sent: u8, length: u8| {
            move |cpc: &mut cpc_t| {
                cpc.fdc.phase = upd765_phase_UPD765_PHASE_RESULT;
                cpc.fdc.result_sent = sent;
                cpc.fdc.result_length = length;
            }
        };
        assert_eq!(check(&result(6, 7)), Ok(()));
        refused(&result(7, 7), "fdc.result_sent");
        refused(&result(0, 8), "fdc.result_sent");
        for stage in [
            upd765_stage_UPD765_STAGE_FORMAT_TO_IDENTITY,
            upd765_stage_UPD765_STAGE_FORMAT_IDENTITY,
        ] {
            let format = |sectors: u8, bytes: u8| {
                move |cpc: &mut cpc_t| {
                    cpc.fdc.stage = stage;
                    cpc.fdc.sectors_done = sectors;
                    cpc.fdc.identity_bytes = bytes;
                }
            };
            assert_eq!(check(&format(28, 3)), Ok(()));
            refused(&format(29, 0), "fdc.sectors_done");
            refused(&format(0, 4), "fdc.sectors_done");
        }
        assert!(cpc.machine().fdc.drive.is_null());
        refused(
            &|cpc| cpc.fdc.phase = upd765_phase_UPD765_PHASE_EXECUTION,
            "fdc.phase",
        );
    }

    #[test]
    fn the_check_holds_a_spectrum_to_every_bound_the_core_trusts() {
        let spectrum = Spectrum::new(forty_eight_k(), rom(&[], HALT));
        let check = |change: &dyn Fn(&mut spectrum_t)| {
            // SAFETY: as for a CPC.
            let mut written = unsafe { std::ptr::read(spectrum.machine()) };
            change(&mut written);
            spectrum.check(&written)
        };
        assert_eq!(check(&|_| {}), Ok(()));
        assert_eq!(check(&|spectrum| spectrum.cpu.pc = 0x8000), Ok(()));
        for (change, field) in [
            (
                &(|spectrum: &mut spectrum_t| spectrum.ram_size = SPECTRUM_RAM_16K)
                    as &dyn Fn(&mut spectrum_t),
                "ram_size",
            ),
            (
                &|spectrum: &mut spectrum_t| spectrum.monitor.width = 1,
                "monitor.width",
            ),
            (
                &|spectrum: &mut spectrum_t| spectrum.cpu.program_index = 1,
                "cpu.program_length",
            ),
            (
                &|spectrum: &mut spectrum_t| spectrum.pins |= u64::from(Z80_WAIT),
                "pins",
            ),
        ] {
            let problem = check(change).unwrap_err();
            assert!(problem.starts_with(field), "{problem}");
        }
    }

    #[test]
    fn each_machine_is_built_with_the_ram_it_is_handed() {
        for size in [0x10000, 0x20000] {
            let cpc = Cpc::new(vec![0; size].into_boxed_slice(), rom(&[], HALT));
            assert_eq!(cpc.machine().ram_size as usize, size);
            assert_eq!(cpc.machine().ram, cpc.ram.as_ptr().cast());
        }
        for size in [SPECTRUM_RAM_16K, SPECTRUM_RAM_48K] {
            let ram = vec![0; size as usize].into_boxed_slice();
            let spectrum = Spectrum::new(ram, rom(&[], HALT));
            assert_eq!(spectrum.machine().ram_size, size);
            assert_eq!(spectrum.machine().ram, spectrum.ram.as_ptr().cast());
        }
    }

    // A return address the processor would not read back where it was pushed
    // is refused, and the processor left as it stood, whichever of its two
    // bytes would not: pushed under a CPC's lower or upper ROM, into a
    // Spectrum's ROM, or past a 16K Spectrum's RAM, which ends at &7FFF.
    #[test]
    fn a_call_is_refused_where_its_return_address_would_not_read_back() {
        let mut cpc = Cpc::new(sixty_four_k(), rom(&[DI], HALT));
        let standing = cpc.machine().cpu;
        for (sp, called) in [
            (0x4002, true),
            (0x4001, false),
            (0xC000, true),
            (0xC001, false),
        ] {
            cpc.machine_mut().cpu = standing;
            cpc.machine_mut().cpu.sp = sp;
            assert_eq!(cpc.call(0x0100).is_ok(), called, "&{sp:04X}");
            let cpu = cpc.machine().cpu;
            assert_eq!(
                (cpu.pc, cpu.sp),
                if called { (0x0100, sp - 2) } else { (0, sp) }
            );
        }
        let sixteen_k = vec![0; SPECTRUM_RAM_16K as usize].into_boxed_slice();
        let mut spectrum = Spectrum::new(sixteen_k, rom(&[DI], HALT));
        let standing = spectrum.machine().cpu;
        for (sp, called) in [
            (0x4002, true),
            (0x4001, false),
            (0x8000, true),
            (0x8001, false),
        ] {
            spectrum.machine_mut().cpu = standing;
            spectrum.machine_mut().cpu.sp = sp;
            assert_eq!(spectrum.call(0x0100).is_ok(), called, "&{sp:04X}");
            let cpu = spectrum.machine().cpu;
            assert_eq!(
                (cpu.pc, cpu.sp),
                if called { (0x0100, sp - 2) } else { (0, sp) }
            );
        }
    }

    fn counting_everything() -> [Condition; 1] {
        [Condition {
            kind: EXECUTE,
            first: 0,
            last: 0xFFFF,
            mask: 0,
            list: COUNT,
        }]
    }

    #[test]
    #[should_panic(expected = "a count of zero for every kind and every address")]
    fn a_watch_that_counts_refuses_to_go_without_its_counts() {
        Watch::new(&counting_everything(), &mut [], &mut [], &mut [], 1, 1);
    }

    #[test]
    #[should_panic(expected = "a count of zero for every kind and every address")]
    fn a_watch_that_counts_refuses_counts_already_begun() {
        let mut counts = vec![0; KINDS.len() * COUNTS_PER_KIND];
        counts[0x38] = 1;
        Watch::new(&counting_everything(), &mut [], &mut counts, &mut [], 1, 1);
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
