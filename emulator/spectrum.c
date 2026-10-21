/*
 * spectrum.c — a Spectrum, run for the server.
 */
#include <stdbool.h>
#include <stddef.h>

#include "mcp.h"
#include "watch.h"

/* Ticks the machine, unless the watch stops it first. */
static bool tick(spectrum_t *spectrum, mcp_run_t *run, mcp_watch_t *watch) {
  if (watch != NULL &&
      mcp_watch_before(watch, &spectrum->cpu, spectrum_instruction_complete(spectrum),
                       &spectrum->monitor, run->ticks)) {
    return false;
  }
  bool retraced_before = spectrum->monitor.frame_retraced;
  uint64_t before = spectrum->pins;
  uint64_t pins = spectrum_tick(spectrum);
  run->ticks++;
  if (spectrum->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
  if (watch != NULL) {
    mcp_watch_after(watch, &spectrum->cpu, &spectrum->monitor, before, pins, run->ticks);
  }
  return true;
}

/* spectrum_finish_instruction's loop, written out so that its ticks are
   counted, and bounded as it is: longer than the longest instruction, the
   charges it can earn along a line of the picture and an interrupt taken at
   the end of it. */
static void finish(spectrum_t *spectrum, mcp_run_t *run, mcp_watch_t *watch) {
  for (int guard = 0; guard < 256 && !spectrum_instruction_complete(spectrum); guard++) {
    tick(spectrum, run, watch);
  }
  run->settled = spectrum_instruction_complete(spectrum);
  if (watch != NULL && run->settled) {
    mcp_watch_end(watch, &spectrum->cpu, &spectrum->monitor, run->ticks);
  }
}

mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit,
                           mcp_watch_t *watch) {
  mcp_run_t run = {0};
  bool halting_stops = !mcp_z80_halted_for_good(&spectrum->cpu);
  while (run.frames < frames && run.ticks < limit && !mcp_watch_stopping(watch) &&
         !(halting_stops && mcp_z80_halted_for_good(&spectrum->cpu))) {
    if (!tick(spectrum, &run, watch)) {
      run.settled = true;
      return run;
    }
    halting_stops = halting_stops || !mcp_z80_halted_for_good(&spectrum->cpu);
  }
  finish(spectrum, &run, watch);
  return run;
}

mcp_run_t mcp_spectrum_step(spectrum_t *spectrum, uint32_t instructions) {
  mcp_run_t run = {.settled = spectrum_instruction_complete(spectrum)};
  for (uint32_t step = 0; step < instructions; step++) {
    tick(spectrum, &run, NULL);
    finish(spectrum, &run, NULL);
    if (!run.settled) {
      break;
    }
  }
  return run;
}

/* spectrum.c's write_memory lands a byte in the RAM, ram_size bytes from
   SPECTRUM_RAM_BASE, and nowhere else (colophon-emulator at 3a0e8ad). */
static bool reads_back(const spectrum_t *spectrum, uint16_t address) {
  return address >= SPECTRUM_RAM_BASE &&
         (uint32_t)(address - SPECTRUM_RAM_BASE) < spectrum->ram_size;
}

const char *mcp_spectrum_call(spectrum_t *spectrum, uint16_t address) {
  z80_t *cpu = &spectrum->cpu;
  if (!spectrum_instruction_complete(spectrum)) {
    return MCP_INSIDE_AN_INSTRUCTION;
  }
  uint16_t high = (uint16_t)(cpu->sp - 1);
  uint16_t low = (uint16_t)(cpu->sp - 2);
  if (!reads_back(spectrum, high) || !reads_back(spectrum, low)) {
    return "the return address would be pushed into the ROM or past the RAM, where it lands "
           "nowhere, and the routine's return would not read it back; give cpu.sp in values, "
           "pointing into the RAM";
  }
  spectrum_poke(spectrum, high, (uint8_t)(cpu->pc >> 8));
  spectrum_poke(spectrum, low, (uint8_t)cpu->pc);
  cpu->sp = low;
  cpu->pc = address;
  return NULL;
}

/* As for a CPC's, at the same commit of the emulator. */
const char *mcp_spectrum_check(const spectrum_t *standing, const spectrum_t *written) {
  const char *problem = mcp_z80_check(&written->cpu);
  if (problem != NULL) {
    return problem;
  }
  /* spectrum.c's peek, poke and fetch, and the snapshot's copies: RAM is
     reached as far as ram_size says. */
  if (written->ram_size != standing->ram_size) {
    return "ram_size is the size of the RAM the machine was built with, and stays so";
  }
  /* The processor is handed the bus as the last tick left it, with only its
     INT recomputed, and z80.c holds a machine cycle for as long as WAIT is
     on it (`if (pins & Z80_WAIT) {` ... `return pins;`), so a WAIT written
     there would hold the processor for good. Nothing on a Spectrum drives
     WAIT. */
  if ((written->pins & Z80_WAIT) != 0) {
    return "pins carries WAIT, which nothing on a Spectrum drives and which would hold its "
           "processor for good; write pins with bit 31, WAIT, clear";
  }
  return mcp_monitor_check(&standing->monitor, &written->monitor);
}

void mcp_spectrum_seek(spectrum_t *spectrum) { ula_seek(&spectrum->ula, spectrum->ula.frame_tick); }
