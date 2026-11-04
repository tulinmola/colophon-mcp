/*
 * cpc.c — a CPC, run for the server.
 */
#include <stdbool.h>
#include <stddef.h>

#include "mcp.h"
#include "watch.h"

/* Whether an I/O request, begun at this port, wrote one of the PSG's
   registers. The core runs the chip on every access to the PPI, which the
   board decodes from A11 low, and port C's top two bits are the chip's BDIR
   and BC1: each access that leaves them giving it WRITE writes port A's byte
   to the selected register while the chip is selected (colophon-emulator's
   cpc.c, cpc_init, io_write, io_read and run_psg, and its ay38910.c,
   ay38910_init and ay38910_access, at 2898ddd). */
static bool psg_written(const cpc_t *cpc, uint16_t port) {
  return (port & 0x0800) == 0 && ppi_output_of(&cpc->ppi, PPI_PORT_C) >> 6 == AY38910_WRITE &&
         cpc->psg.chip_selected;
}

/* Whether an I/O request at this port writes one of the 6845's registers. The
   board strobes the chip at any I/O request with A14 low, RS on A8 and R/W on
   A9, so A8 high and A9 low write the register the address register names; a
   read writes too, the byte the address bus carries in its top half, once,
   where RD falls (colophon-emulator's cpc.c, crtc_bus_pins and io_read, at
   78c56f2). */
static bool crtc_written(uint16_t port) { return (port & 0x4300) == 0x0100; }

/* The byte the 6845 was given: the data an OUT puts on the bus, and the top
   half of the address an IN does. */
static uint8_t crtc_given(uint64_t pins) {
  return (pins & Z80_WR) != 0 ? z80_data(pins) : (uint8_t)(z80_address(pins) >> 8);
}

/* What a tick's write can change, as it stands: the byte a write to the
   address on the bus reaches, which is the RAM's beneath an enabled ROM, and
   the PSG's and the 6845's selected registers, -1 for an address register
   naming none of the 6845's eighteen. */
typedef struct {
  uint8_t memory;
  uint8_t psg;
  int crtc;
} held_t;

static held_t held(const cpc_t *cpc, uint16_t address) {
  uint8_t selected = cpc->crtc.address_register;
  return (held_t){
      .memory = cpc->write_page[address >> 14][address & 0x3FFF],
      .psg = cpc->psg.registers[cpc->psg.selected],
      .crtc = selected < sizeof cpc->crtc.registers / sizeof cpc->crtc.registers[0]
                  ? cpc->crtc.registers[selected]
                  : -1,
  };
}

uint32_t mcp_cpc_locate(const cpc_t *cpc, uint16_t address, bool writing) {
  uint32_t quarter = address >> 14;
  uint32_t offset = address & 0x3FFF;
  if (!writing && quarter == 0 && cpc->gate_array.lower_rom_enabled) {
    return MCP_FIRST_ROM_LOCATION + offset;
  }
  if (!writing && quarter == 3 && cpc->gate_array.upper_rom_enabled) {
    uint32_t number = cpc->upper_roms[cpc->upper_rom_number] != NULL ? cpc->upper_rom_number : 0;
    return MCP_FIRST_ROM_LOCATION + (1 + number) * MCP_ROM_BYTES + offset;
  }
  const uint8_t *page = writing ? cpc->write_page[quarter] : cpc->read_page[quarter];
  return (uint32_t)(page - cpc->ram) + offset;
}

static uint32_t locate(const void *machine, uint16_t address, bool writing) {
  return mcp_cpc_locate(machine, address, writing);
}

static uint8_t peek(const void *machine, uint16_t address) { return cpc_peek(machine, address); }

uint32_t mcp_cpc_locations(const cpc_t *cpc) {
  uint32_t highest = 0;
  for (uint32_t number = 0; number < 256; number++) {
    if (cpc->upper_roms[number] != NULL) {
      highest = number;
    }
  }
  return MCP_FIRST_ROM_LOCATION + (2 + highest) * MCP_ROM_BYTES;
}

/* What the watch sees of a tick, given the bus before it and after it, what a
   write could change as it stood before it, and whether a write to the 6845
   awaited the clock as the tick began. A read held by WAIT writes the PSG again
   on every tick it is held, the same byte to the same register, and is seen
   once, as it begins. A write to the 6845 is seen on the tick the chip takes
   it: the tick its request begins, or, on a type 3 or 4, whose board holds an
   OUT's write to the clock, the first character clock the cycle spans, the
   cycle still on the bus (colophon-emulator's cpc.c, io_write and cpc_tick, at
   78c56f2). */
static void see_tick(cpc_t *cpc, mcp_watch_t *watch, uint64_t before, uint64_t pins, uint64_t ticks,
                     held_t was, bool write_awaited_the_clock) {
  held_t is = watch->compares ? held(cpc, z80_address(before)) : was;
  mcp_watch_after(watch, &cpc->cpu, &cpc->monitor, before, pins, ticks, is.memory != was.memory);
  bool began = mcp_request_began(before, pins, Z80_IORQ | Z80_WR) ||
               mcp_request_began(before, pins, Z80_IORQ | Z80_RD);
  bool taken = write_awaited_the_clock && (pins & (Z80_IORQ | Z80_WR)) == (Z80_IORQ | Z80_WR);
  uint16_t port = z80_address(pins);
  if ((began || taken) && crtc_written(port) && !cpc->crtc_write_awaits_the_clock) {
    mcp_watch_register(watch, &cpc->monitor, ticks, MCP_CRTC, cpc->crtc.address_register,
                       crtc_given(pins), is.crtc != was.crtc);
  }
  if (began && psg_written(cpc, port)) {
    mcp_watch_register(watch, &cpc->monitor, ticks, MCP_PSG, cpc->psg.selected,
                       ppi_output_of(&cpc->ppi, PPI_PORT_A), is.psg != was.psg);
  }
}

/* Ticks the machine, unless the watch stops it first. */
static bool tick(cpc_t *cpc, mcp_run_t *run, mcp_watch_t *watch) {
  if (watch != NULL && mcp_watch_before(watch, &cpc->cpu, z80_instruction_complete(&cpc->cpu),
                                        &cpc->monitor, run->ticks)) {
    return false;
  }
  bool retraced_before = cpc->monitor.frame_retraced;
  bool write_awaited_the_clock = cpc->crtc_write_awaits_the_clock;
  uint64_t before = cpc->pins;
  /* z80.c's MEM_WRITE_T1 puts a write's address on the bus a tick before
     MEM_WRITE_T2 writes the byte there. */
  held_t was = watch != NULL && watch->compares ? held(cpc, z80_address(before)) : (held_t){0};
  uint64_t pins = cpc_tick(cpc);
  run->ticks++;
  if (cpc->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
  if (watch != NULL) {
    see_tick(cpc, watch, before, pins, run->ticks, was, write_awaited_the_clock);
  }
  return true;
}

/* cpc_finish_instruction's loop, written out so that its ticks are counted,
   and bounded as it is: longer than any instruction can take, wait states
   and all. */
static void finish(cpc_t *cpc, mcp_run_t *run, mcp_watch_t *watch) {
  for (int guard = 0; guard < 256 && !z80_instruction_complete(&cpc->cpu); guard++) {
    tick(cpc, run, watch);
  }
  run->settled = z80_instruction_complete(&cpc->cpu);
  if (watch != NULL && run->settled) {
    mcp_watch_end(watch, &cpc->cpu, &cpc->monitor, run->ticks);
  }
}

mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit, mcp_watch_t *watch) {
  mcp_run_t run = {0};
  if (watch != NULL) {
    watch->locate = locate;
    watch->peek = peek;
    watch->machine = cpc;
    watch->compares = mcp_watch_compares(watch);
  }
  bool halting_stops = !mcp_z80_halted_for_good(&cpc->cpu);
  while (run.frames < frames && run.ticks < limit && !mcp_watch_stopping(watch) &&
         !(halting_stops && mcp_z80_halted_for_good(&cpc->cpu))) {
    if (!tick(cpc, &run, watch)) {
      run.settled = true;
      return run;
    }
    halting_stops = halting_stops || !mcp_z80_halted_for_good(&cpc->cpu);
  }
  finish(cpc, &run, watch);
  return run;
}

mcp_run_t mcp_cpc_step(cpc_t *cpc, uint32_t instructions) {
  mcp_run_t run = {.settled = z80_instruction_complete(&cpc->cpu)};
  for (uint32_t step = 0; step < instructions; step++) {
    tick(cpc, &run, NULL);
    finish(cpc, &run, NULL);
    if (!run.settled) {
      break;
    }
  }
  return run;
}

/* cpc_peek reads through read_page and cpc_poke writes through write_page,
   which differ wherever a ROM answers the reads (colophon-emulator at
   3a0e8ad). */
static bool reads_back(const cpc_t *cpc, uint16_t address) {
  return cpc->read_page[address >> 14] == cpc->write_page[address >> 14];
}

const char *mcp_cpc_call(cpc_t *cpc, uint16_t address) {
  z80_t *cpu = &cpc->cpu;
  if (!z80_instruction_complete(cpu)) {
    return MCP_INSIDE_AN_INSTRUCTION;
  }
  uint16_t high = (uint16_t)(cpu->sp - 1);
  uint16_t low = (uint16_t)(cpu->sp - 2);
  if (!reads_back(cpc, high) || !reads_back(cpc, low)) {
    return "the return address would be pushed under an enabled ROM, where the routine's return "
           "could not read it back; give cpu.sp in values so that the 2 bytes below it are RAM "
           "that no enabled ROM covers";
  }
  cpc_poke(cpc, high, (uint8_t)(cpu->pc >> 8));
  cpc_poke(cpc, low, (uint8_t)cpu->pc);
  cpu->sp = low;
  cpu->pc = address;
  return NULL;
}

/* The bounds below are where the core trusts a field without checking it,
   because no path of its own takes the field outside them: the line quoted
   beside each is the use. colophon-emulator at 78c56f2. */
const char *mcp_cpc_check(const cpc_t *standing, const cpc_t *written) {
  const upd765_t *fdc = &written->fdc;
  const char *problem = mcp_z80_check(&written->cpu);
  if (problem != NULL) {
    return problem;
  }
  /* io_write fits the PAL that pages the second 64K by it:
     `bool pal_fitted = cpc->ram_size >= 0x20000;`. */
  if (written->ram_size != standing->ram_size) {
    return "ram_size is the size of the machine's RAM and cannot change; write it as it reads";
  }
  /* Not a bound: a type changed under the 6845 would run on from latches the
     other type set. A kept room is copied back into the chip, its type with it
     (`*crtc = *room;` in crtc.c), room 1 only while room 0 keeps a line end
     too, and room 0 may be copied into room 1 first (enter_character). A room
     that keeps nothing is not read before a line end fills it. */
  if (written->crtc.type != standing->crtc.type) {
    return "crtc.type is the type of 6845 the machine was built with and cannot change; write it "
           "as it reads, or open a machine with machine_open's crtc_type";
  }
  bool kept[] = {written->crtc.a_line_end_is_kept,
                 written->crtc.a_line_end_is_kept && written->crtc.the_line_end_before_is_kept};
  for (size_t room = 0; room < sizeof kept / sizeof kept[0]; room++) {
    if (kept[room] && written->crtc_line_end_rooms[room].type != written->crtc.type) {
      return "a line-end room that keeps a line end has crtc.type as its type: "
             "crtc_line_end_rooms[0].type while crtc.a_line_end_is_kept is true, and "
             "crtc_line_end_rooms[1].type while crtc.the_line_end_before_is_kept is true as well; "
             "write the room's type as crtc.type reads, or write crtc.a_line_end_is_kept false";
    }
  }
  /* cpc_remap: `banking[cpc->mmr & 0x07]` pages in banks 4 to 7, past the
     end of 64K. */
  if (written->ram_size < 0x20000 && (written->mmr & 0x07) != 0) {
    return "mmr selects a RAM configuration, and a 64K machine has only configuration 0; write "
           "&C0, or any value whose lowest 3 bits are 0";
  }
  /* gate_array_write's INKR: `gate_array->inks[gate_array->pen] = data`. */
  if (written->gate_array.pen > 16) {
    return "gate_array.pen selects one of the 17 inks; write 0 to 16";
  }
  /* ay38910_access: `psg->registers[psg->selected]`, as held() above reads it
     too. */
  if (written->psg.selected > 15) {
    return "psg.selected selects one of the 16 registers; write 0 to 15";
  }
  problem = mcp_monitor_check(&standing->monitor, &written->monitor);
  if (problem != NULL) {
    return problem;
  }
  /* upd765.c's command phase: `fdc->command[fdc->command_received++]`,
     stopping only when the count meets the length. */
  if (fdc->phase == UPD765_PHASE_COMMAND && !(fdc->command_received < fdc->command_length &&
                                              fdc->command_length <= UPD765_COMMAND_BYTES)) {
    return "fdc.command_received and fdc.command_length count a command's bytes in; in the command "
           "phase write command_received below command_length, and command_length at most 9";
  }
  /* Its result phase: `fdc->result[fdc->result_sent++]`. */
  if (fdc->phase == UPD765_PHASE_RESULT &&
      !(fdc->result_sent < UPD765_RESULT_BYTES && fdc->result_length <= UPD765_RESULT_BYTES)) {
    return "fdc.result_sent and fdc.result_length count a result's bytes out; in the result phase "
           "write result_sent below 7, and result_length at most 7";
  }
  /* A format's identities:
     `fdc->identities[fdc->sectors_done][fdc->identity_bytes++]`. */
  if ((fdc->stage == UPD765_STAGE_FORMAT_TO_IDENTITY ||
       fdc->stage == UPD765_STAGE_FORMAT_IDENTITY) &&
      !(fdc->sectors_done < FLOPPY_MAX_SECTORS && fdc->identity_bytes < FLOPPY_IDENTITY_BYTES)) {
    return "fdc.sectors_done and fdc.identity_bytes place a format's identity bytes; while it "
           "takes them, write sectors_done below 29 and identity_bytes below 4";
  }
  /* The execution phase runs on the drive a command chose: `drive_t *drive =
     fdc->drive`, then drive_ready(drive). */
  if (fdc->phase == UPD765_PHASE_EXECUTION && fdc->drive == NULL) {
    return "fdc.phase can be UPD765_PHASE_EXECUTION only once a command has chosen a drive, and "
           "none is chosen; write another phase";
  }
  return NULL;
}

/* reading: `floppy->image + sector->image_offset +
   (size_t)(revolution % sector->copies) * sector->recorded`, and data_crc
   over extent bytes of it. floppy_add_sector's own tests, a sector of no
   copies spanning one. */
static const char *sector_problem(const floppy_t *written, const floppy_sector_t *sector) {
  if (sector->extent > sector->recorded) {
    return "a sector's extent is larger than its recorded bytes";
  }
  uint64_t span = (uint64_t)sector->recorded * (sector->copies > 0 ? sector->copies : 1);
  if (sector->recorded > 0 && (sector->image_offset > written->image_length ||
                               (uint64_t)(written->image_length - sector->image_offset) < span)) {
    return "a sector's copies, recorded bytes each from image_offset, run past image_length";
  }
  return NULL;
}

/* A format takes any track, formatted or not: it fills the track's room
   from image_offset, `floppy->image[offset + byte] = filler`, and sets
   its sectors afresh from `track->sector_count = 0`. Every other use
   waits for a formatted track: `track->formatted && !track->unreadable`
   in readable_track, and `!track->formatted` refused in
   floppy_write_byte and floppy_data_written. */
static const char *track_problem(const floppy_t *written, const floppy_track_t *track) {
  if (track->image_offset > written->image_length ||
      written->image_length - track->image_offset < track->room) {
    return "a track's room, from its image_offset, runs past image_length";
  }
  if (!track->formatted) {
    return NULL;
  }
  /* floppy_byte and floppy_write_byte: `position %= track->length`. */
  if (track->length == 0) {
    return "a formatted track's length is 0, and it must be at least 1";
  }
  /* Every walk of a track's sectors: `index < track->sector_count`, over
     `track->sectors[index]`. */
  if (track->sector_count > FLOPPY_MAX_SECTORS) {
    return "a formatted track's sector_count is above 29, the most sectors a track holds";
  }
  for (uint8_t index = 0; index < track->sector_count; index++) {
    const char *problem = sector_problem(written, &track->sectors[index]);
    if (problem != NULL) {
      return problem;
    }
  }
  return NULL;
}

/* The bounds below are where floppy.c trusts a disc's fields without checking
   them, because no path of its own takes a field outside them: the line
   quoted beside each is the use. colophon-emulator at 20f1c15. */
const char *mcp_floppy_check(const floppy_t *written) {
  /* floppy_format_track's room past the image: `if (needed >
     floppy->image_capacity - floppy->image_length) {`. */
  if (written->image_length > written->image_capacity) {
    return "image_length is larger than image_capacity, the size of the image's buffer";
  }
  for (int cylinder = 0; cylinder < FLOPPY_MAX_CYLINDERS; cylinder++) {
    for (int side = 0; side < FLOPPY_MAX_SIDES; side++) {
      const char *problem = track_problem(written, &written->tracks[cylinder][side]);
      if (problem != NULL) {
        return problem;
      }
    }
  }
  return NULL;
}
