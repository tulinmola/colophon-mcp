/*
 * spectrum.c — a Spectrum, run for the server.
 */
#include <stdbool.h>

#include "mcp.h"

static void tick(spectrum_t *spectrum, mcp_run_t *run) {
  bool retraced_before = spectrum->monitor.frame_retraced;
  spectrum_tick(spectrum);
  run->ticks++;
  if (spectrum->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
}

mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit) {
  mcp_run_t run = {0};
  while (run.frames < frames && run.ticks < limit) {
    tick(spectrum, &run);
  }
  /* spectrum_finish_instruction's loop, written out so that its ticks are
     counted, and bounded as it is: longer than the longest instruction, the
     charges it can earn along a line of the picture and an interrupt taken
     at the end of it. */
  for (int guard = 0; guard < 256 && !spectrum_instruction_complete(spectrum); guard++) {
    tick(spectrum, &run);
  }
  run.settled = spectrum_instruction_complete(spectrum);
  return run;
}
