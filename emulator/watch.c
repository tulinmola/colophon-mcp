/*
 * watch.c — what a run watches for, the same on either machine.
 */
#include "watch.h"

#include <stddef.h>

static bool agrees(const mcp_condition_t *condition, mcp_kind_t kind, uint16_t address) {
  if (condition->kind != kind) {
    return false;
  }
  if (kind == MCP_IN || kind == MCP_OUT) {
    return ((address ^ condition->first) & condition->mask) == 0;
  }
  return address >= condition->first && address <= condition->last;
}

/* Keeps what was seen if a condition of the kinds allowed matches it. */
static void see(mcp_watch_t *watch, const monitor_t *monitor, uint64_t tick, mcp_kind_t kind,
                uint16_t address, uint8_t value, bool may_stop, bool may_gather) {
  bool stops = false;
  bool gathers = false;
  for (uint32_t index = 0; index < watch->condition_count; index++) {
    const mcp_condition_t *condition = &watch->conditions[index];
    if (agrees(condition, kind, address)) {
      stops |= condition->stops && may_stop;
      gathers |= !condition->stops && may_gather;
    }
  }
  if (!stops && !gathers) {
    return;
  }
  if (watch->match_count < watch->room) {
    watch->matches[watch->match_count] = (mcp_match_t){
        .tick = tick,
        .kind = kind,
        .address = address,
        .value = value,
        .pc = watch->pc,
        .response = kind != MCP_EXECUTE && watch->responding,
        .beam_x = monitor->beam_x,
        .beam_y = monitor->beam_y,
        .stops = stops,
        .gathers = gathers,
    };
  }
  watch->match_count++;
  watch->matched |= stops;
  if (gathers && ++watch->gathered >= watch->most_gathered) {
    watch->full = true;
  }
}

/* At an instruction the run has come to: whether it stops before it. */
static bool arrived(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor, uint64_t tick) {
  if (!cpu->halted) {
    see(watch, monitor, tick, MCP_EXECUTE, cpu->pc, 0, true, false);
  }
  if (watch->calling && cpu->pc == watch->return_pc && cpu->sp == watch->return_sp) {
    watch->returned = true;
  }
  return watch->matched || watch->returned;
}

bool mcp_watch_before(mcp_watch_t *watch, const z80_t *cpu, bool settled, const monitor_t *monitor,
                      uint64_t tick) {
  bool first = !watch->begun;
  watch->begun = true;
  if (!settled) {
    if (first) {
      watch->pc = cpu->pc;
    }
    /* Neither machine drives NMI, so the only response is to INT, which
       begins with its acknowledge. */
    if (z80_next_cycle(cpu) == Z80_CYCLE_INTERRUPT) {
      watch->responding = true;
    }
    return false;
  }
  watch->pc = cpu->pc;
  watch->responding = false;
  if (!first && arrived(watch, cpu, monitor, tick)) {
    return true;
  }
  if (!cpu->halted) {
    see(watch, monitor, tick, MCP_EXECUTE, cpu->pc, 0, false, true);
  }
  return false;
}

static bool began(uint64_t before, uint64_t pins, uint64_t request) {
  return (pins & request) == request && (before & request) != request;
}

/* A request is seen as it first reaches the bus: a machine whose board holds
   the processor's clock or WAIT keeps it there for more than one tick. A CPC
   answers a port read again on every tick WAIT holds it (its cpc_tick), and
   the processor takes the byte the last of them leaves on the bus (z80.c's
   IO_READ_T4), so a port read matched is given each tick's byte while it is
   held; memory answers the same byte throughout. */
void mcp_watch_after(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor,
                     uint64_t before, uint64_t pins, uint64_t tick) {
  uint16_t address = z80_address(pins);
  uint8_t value = z80_data(pins);
  if ((pins & (Z80_IORQ | Z80_RD)) != (Z80_IORQ | Z80_RD)) {
    watch->port_read_held = false;
  } else if (watch->port_read_held) {
    watch->matches[watch->port_read_match].value = value;
  }
  if (began(before, pins, Z80_MREQ | Z80_RD)) {
    if ((pins & Z80_M1) == 0 && !z80_reading_operand(cpu)) {
      see(watch, monitor, tick, MCP_READ, address, value, true, true);
    }
  } else if (began(before, pins, Z80_MREQ | Z80_WR)) {
    see(watch, monitor, tick, MCP_WRITE, address, value, true, true);
  } else if (began(before, pins, Z80_IORQ | Z80_RD)) {
    uint32_t match = watch->match_count;
    see(watch, monitor, tick, MCP_IN, address, value, true, true);
    watch->port_read_held = watch->match_count > match && match < watch->room;
    watch->port_read_match = match;
  } else if (began(before, pins, Z80_IORQ | Z80_WR)) {
    see(watch, monitor, tick, MCP_OUT, address, value, true, true);
  }
}

void mcp_watch_end(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor, uint64_t tick) {
  if (watch->begun) {
    arrived(watch, cpu, monitor, tick);
  }
}

bool mcp_watch_stopping(const mcp_watch_t *watch) {
  return watch != NULL && (watch->matched || watch->full || watch->returned);
}
