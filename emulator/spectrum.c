/*
 * spectrum.c — a Spectrum, run for the server.
 */
#include <stdbool.h>
#include <stddef.h>

#include "mcp.h"

static void tick(spectrum_t *spectrum, mcp_run_t *run) {
  bool retraced_before = spectrum->monitor.frame_retraced;
  spectrum_tick(spectrum);
  run->ticks++;
  if (spectrum->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
}

/* spectrum_finish_instruction's loop, written out so that its ticks are
   counted, and bounded as it is: longer than the longest instruction, the
   charges it can earn along a line of the picture and an interrupt taken at
   the end of it. */
static void finish(spectrum_t *spectrum, mcp_run_t *run) {
  for (int guard = 0; guard < 256 && !spectrum_instruction_complete(spectrum); guard++) {
    tick(spectrum, run);
  }
  run->settled = spectrum_instruction_complete(spectrum);
}

mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit) {
  mcp_run_t run = {0};
  while (run.frames < frames && run.ticks < limit) {
    tick(spectrum, &run);
  }
  finish(spectrum, &run);
  return run;
}

mcp_run_t mcp_spectrum_step(spectrum_t *spectrum, uint32_t instructions) {
  mcp_run_t run = {.settled = spectrum_instruction_complete(spectrum)};
  for (uint32_t step = 0; step < instructions; step++) {
    tick(spectrum, &run);
    finish(spectrum, &run);
    if (!run.settled) {
      break;
    }
  }
  return run;
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
