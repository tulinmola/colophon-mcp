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

/* Where the match counted as this one is kept, if below room. */
static uint32_t kept_at(const mcp_watch_t *watch, uint32_t match) {
  return watch->gathers_last ? match % watch->room : match;
}

/* Counts a place matched, keeps it as it is first matched, and fills the
   count at its last new place. An instruction is counted only as it begins,
   where the pc is its own address. */
static void count(mcp_watch_t *watch, const mcp_match_t *match) {
  uint16_t place = watch->counts_by_pc ? match->pc : match->address;
  uint32_t *counted = &watch->counts[(uint32_t)match->kind * MCP_COUNTS_PER_KIND + place];
  if (*counted == 0) {
    if (watch->counted < watch->place_room) {
      mcp_place_t *kept = &watch->places[watch->counted];
      *kept = (mcp_place_t){.first = *match};
      if (match->kind == MCP_EXECUTE) {
        watch->fetching = kept;
      }
    }
    if (++watch->counted >= watch->most_counted) {
      watch->full = true;
    }
  }
  (*counted)++;
}

/* Keeps what was seen if a condition of the lists allowed matches it. A
   match is counted where it may be gathered. */
static void see(mcp_watch_t *watch, const monitor_t *monitor, uint64_t tick, mcp_kind_t kind,
                uint16_t address, uint8_t value, bool may_stop, bool may_gather) {
  bool stops = false;
  bool gathers = false;
  bool counts = false;
  for (uint32_t index = 0; index < watch->condition_count; index++) {
    const mcp_condition_t *condition = &watch->conditions[index];
    if (agrees(condition, kind, address)) {
      stops |= condition->list == MCP_UNTIL && may_stop;
      gathers |= condition->list == MCP_GATHER && may_gather;
      counts |= condition->list == MCP_COUNT && may_gather;
    }
  }
  mcp_match_t match = {
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
  if (counts && !(watch->counts_by_pc && match.response)) {
    count(watch, &match);
  }
  if (!stops && !gathers) {
    return;
  }
  uint32_t slot = kept_at(watch, watch->match_count);
  if (slot < watch->room) {
    watch->matches[slot] = match;
  }
  watch->match_count++;
  watch->matched |= stops;
  if (gathers && ++watch->gathered >= watch->most_gathered && !watch->gathers_last) {
    watch->full = true;
  }
}

/* A byte of the instruction under way, kept where it was first matched. */
static void fetch(mcp_watch_t *watch, uint8_t value) {
  mcp_place_t *place = watch->fetching;
  if (place != NULL && place->length < MCP_INSTRUCTION_BYTES) {
    place->bytes[place->length++] = value;
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
      watch->fetched = true;
    }
    /* The response to INT begins with its acknowledge; the response to an
       NMI, which neither machine drives but set may write in, is told by
       mcp_watch_after. */
    if (z80_next_cycle(cpu) == Z80_CYCLE_INTERRUPT) {
      watch->responding = true;
    }
    return false;
  }
  watch->pc = cpu->pc;
  watch->responding = false;
  watch->fetching = NULL;
  watch->fetched = false;
  if (!first && arrived(watch, cpu, monitor, tick)) {
    return true;
  }
  if (!cpu->halted) {
    see(watch, monitor, tick, MCP_EXECUTE, cpu->pc, 0, false, true);
  }
  return false;
}

bool mcp_request_began(uint64_t before, uint64_t pins, uint64_t request) {
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
  bool opcode = (pins & Z80_M1) != 0;
  if (mcp_request_began(before, pins, Z80_MREQ | Z80_RD)) {
    if (opcode && watch->fetched && cpu->prefix == 0) {
      /* An instruction's opcodes after its first are fetched with a prefix
         pending, since a prefix has not finished until the instruction it
         modifies has run (z80.h), so this is the response to an NMI, whose
         acknowledge "is an ordinary opcode fetch whose byte is discarded"
         (z80.c, on z80_t.accepting). */
      watch->responding = true;
    } else if (opcode || z80_reading_operand(cpu)) {
      watch->fetched |= opcode;
      fetch(watch, value);
    } else {
      see(watch, monitor, tick, MCP_READ, address, value, true, true);
    }
  } else if (mcp_request_began(before, pins, Z80_MREQ | Z80_WR)) {
    see(watch, monitor, tick, MCP_WRITE, address, value, true, true);
  } else if (mcp_request_began(before, pins, Z80_IORQ | Z80_RD)) {
    uint32_t match = watch->match_count;
    uint32_t slot = kept_at(watch, match);
    see(watch, monitor, tick, MCP_IN, address, value, true, true);
    watch->port_read_held = watch->match_count > match && slot < watch->room;
    watch->port_read_match = slot;
  } else if (mcp_request_began(before, pins, Z80_IORQ | Z80_WR)) {
    see(watch, monitor, tick, MCP_OUT, address, value, true, true);
  }
}

void mcp_watch_psg(mcp_watch_t *watch, const monitor_t *monitor, uint64_t tick, uint8_t selected,
                   uint8_t value) {
  see(watch, monitor, tick, MCP_PSG, selected, value, true, true);
}

void mcp_watch_end(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor, uint64_t tick) {
  if (watch->begun) {
    arrived(watch, cpu, monitor, tick);
  }
}

bool mcp_z80_halted_for_good(const z80_t *cpu) { return cpu->halted && !cpu->iff1; }

bool mcp_watch_stopping(const mcp_watch_t *watch) {
  return watch != NULL && (watch->matched || watch->full || watch->returned);
}
