/*
 * cpc.c — a CPC, run for the server.
 */
#include <stdbool.h>

#include "mcp.h"

static void tick(cpc_t *cpc, mcp_run_t *run) {
  bool retraced_before = cpc->monitor.frame_retraced;
  cpc_tick(cpc);
  run->ticks++;
  if (cpc->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
}

mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit) {
  mcp_run_t run = {0};
  while (run.frames < frames && run.ticks < limit) {
    tick(cpc, &run);
  }
  /* cpc_finish_instruction's loop, written out so that its ticks are
     counted, and bounded as it is: longer than any instruction can take,
     wait states and all. */
  for (int guard = 0; guard < 256 && !z80_instruction_complete(&cpc->cpu); guard++) {
    tick(cpc, &run);
  }
  run.settled = z80_instruction_complete(&cpc->cpu);
  return run;
}
