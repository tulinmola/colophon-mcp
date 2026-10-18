/*
 * cpc.c — a CPC, run for the server.
 */
#include <stdbool.h>
#include <stddef.h>

#include "mcp.h"

static void tick(cpc_t *cpc, mcp_run_t *run) {
  bool retraced_before = cpc->monitor.frame_retraced;
  cpc_tick(cpc);
  run->ticks++;
  if (cpc->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
}

/* cpc_finish_instruction's loop, written out so that its ticks are counted,
   and bounded as it is: longer than any instruction can take, wait states
   and all. */
static void finish(cpc_t *cpc, mcp_run_t *run) {
  for (int guard = 0; guard < 256 && !z80_instruction_complete(&cpc->cpu); guard++) {
    tick(cpc, run);
  }
  run->settled = z80_instruction_complete(&cpc->cpu);
}

mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit) {
  mcp_run_t run = {0};
  while (run.frames < frames && run.ticks < limit) {
    tick(cpc, &run);
  }
  finish(cpc, &run);
  return run;
}

mcp_run_t mcp_cpc_step(cpc_t *cpc, uint32_t instructions) {
  mcp_run_t run = {.settled = z80_instruction_complete(&cpc->cpu)};
  for (uint32_t step = 0; step < instructions; step++) {
    tick(cpc, &run);
    finish(cpc, &run);
    if (!run.settled) {
      break;
    }
  }
  return run;
}

/* The bounds below are where the core trusts a field without checking it,
   because no path of its own takes the field outside them: the line quoted
   beside each is the use. colophon-emulator at 9e4b166. */
const char *mcp_cpc_check(const cpc_t *standing, const cpc_t *written) {
  const upd765_t *fdc = &written->fdc;
  const char *problem = mcp_z80_check(&written->cpu);
  if (problem != NULL) {
    return problem;
  }
  /* io_write fits the PAL that pages the second 64K by it:
     `bool pal_fitted = cpc->ram_size >= 0x20000;`. */
  if (written->ram_size != standing->ram_size) {
    return "ram_size is the size of the RAM the machine was built with, and stays so";
  }
  /* cpc_remap: `banking[cpc->mmr & 0x07]` pages in banks 4 to 7, past the
     end of 64K. */
  if (written->ram_size < 0x20000 && (written->mmr & 0x07) != 0) {
    return "mmr selects a RAM configuration, and a machine of 64K has configuration 0 alone: "
           "&C0, or any value whose lowest three bits are 0";
  }
  /* gate_array_write's INKR: `gate_array->inks[gate_array->pen] = data`. */
  if (written->gate_array.pen > 16) {
    return "gate_array.pen selects one of the seventeen inks, 0 to 16";
  }
  /* psg_access: `psg->registers[psg->selected]`. */
  if (written->psg.selected > 15) {
    return "psg.selected selects one of the sixteen registers, 0 to 15";
  }
  problem = mcp_monitor_check(&standing->monitor, &written->monitor);
  if (problem != NULL) {
    return problem;
  }
  /* upd765.c's command phase: `fdc->command[fdc->command_received++]`,
     stopping only when the count meets the length. */
  if (fdc->phase == UPD765_PHASE_COMMAND && !(fdc->command_received < fdc->command_length &&
                                              fdc->command_length <= UPD765_COMMAND_BYTES)) {
    return "fdc.command_received and fdc.command_length count a command's bytes in, and in the "
           "command phase the count is short of the length, which is at most 9";
  }
  /* Its result phase: `fdc->result[fdc->result_sent++]`. */
  if (fdc->phase == UPD765_PHASE_RESULT &&
      !(fdc->result_sent < UPD765_RESULT_BYTES && fdc->result_length <= UPD765_RESULT_BYTES)) {
    return "fdc.result_sent and fdc.result_length count a result's bytes out, and in the result "
           "phase fewer than 7 have been sent, of at most 7";
  }
  /* A format's identities:
     `fdc->identities[fdc->sectors_done][fdc->identity_bytes++]`. */
  if ((fdc->stage == UPD765_STAGE_FORMAT_TO_IDENTITY ||
       fdc->stage == UPD765_STAGE_FORMAT_IDENTITY) &&
      !(fdc->sectors_done < FLOPPY_MAX_SECTORS && fdc->identity_bytes < FLOPPY_IDENTITY_BYTES)) {
    return "fdc.sectors_done and fdc.identity_bytes place a format's identity bytes, and while "
           "it takes them there are fewer than 29 sectors done and 4 bytes taken";
  }
  /* The execution phase runs on the drive a command chose: `drive_t *drive =
     fdc->drive`, then drive_ready(drive). */
  if (fdc->phase == UPD765_PHASE_EXECUTION && fdc->drive == NULL) {
    return "fdc.phase is UPD765_PHASE_EXECUTION only once a command has chosen a drive, which "
           "this controller has not";
  }
  return NULL;
}
