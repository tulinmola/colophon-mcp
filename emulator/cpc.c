/*
 * cpc.c — a CPC, run for the server.
 */
#include <stdbool.h>
#include <stddef.h>

#include "mcp.h"
#include "watch.h"

/* Ticks the machine, unless the watch stops it first. */
static bool tick(cpc_t *cpc, mcp_run_t *run, mcp_watch_t *watch) {
  if (watch != NULL && mcp_watch_before(watch, &cpc->cpu, z80_instruction_complete(&cpc->cpu),
                                        &cpc->monitor, run->ticks)) {
    return false;
  }
  bool retraced_before = cpc->monitor.frame_retraced;
  uint64_t before = cpc->pins;
  uint64_t pins = cpc_tick(cpc);
  run->ticks++;
  if (cpc->monitor.frame_retraced && !retraced_before) {
    run->frames++;
  }
  if (watch != NULL) {
    mcp_watch_after(watch, &cpc->cpu, &cpc->monitor, before, pins, run->ticks);
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
  while (run.frames < frames && run.ticks < limit && !mcp_watch_stopping(watch)) {
    if (!tick(cpc, &run, watch)) {
      run.settled = true;
      return run;
    }
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
    return "the return address would be pushed under a ROM, which answers the processor's "
           "reads there, and the routine's return would not read it back; give cpu.sp in "
           "values, pointing into RAM that no ROM enabled covers";
  }
  cpc_poke(cpc, high, (uint8_t)(cpu->pc >> 8));
  cpc_poke(cpc, low, (uint8_t)cpu->pc);
  cpu->sp = low;
  cpu->pc = address;
  return NULL;
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
