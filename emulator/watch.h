/*
 * watch.h — what a run watches for, the same on either machine.
 */
#ifndef COLOPHON_MCP_WATCH_H
#define COLOPHON_MCP_WATCH_H

#include <stdbool.h>
#include <stdint.h>

#include "mcp.h"

/* Before a tick, given whether the processor stands between instructions
 * there. Answers whether the run stops before the tick. */
bool mcp_watch_before(mcp_watch_t *watch, const z80_t *cpu, bool settled, const monitor_t *monitor,
                      uint64_t tick);

/* Whether a condition asks for changes. */
bool mcp_watch_compares(const mcp_watch_t *watch);

/* Whether a request first reaches the bus on this tick. */
bool mcp_request_began(uint64_t before, uint64_t pins, uint64_t request);

/* After a tick, given the bus before it and after it, and whether the tick
 * changed the byte at the address the bus carried before it, as a write there
 * reaches it. */
void mcp_watch_after(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor,
                     uint64_t before, uint64_t pins, uint64_t tick, bool changed);

/* After a tick that wrote one of a CPC's PSG or 6845 registers: the kind, the
 * register's number, the byte it was given, and whether the register then
 * holds other than it held. */
void mcp_watch_register(mcp_watch_t *watch, const monitor_t *monitor, uint64_t tick,
                        mcp_kind_t kind, uint8_t number, uint8_t value, bool changed);

/* At the instruction a run ends before. */
void mcp_watch_end(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor, uint64_t tick);

/* Whether the run is to stop at the end of the instruction under way. */
bool mcp_watch_stopping(const mcp_watch_t *watch);

/* Why a routine is not called: a machine is stopped inside an instruction
   only by a chain of prefixes longer than a run finishes. */
#define MCP_INSIDE_AN_INSTRUCTION                                                                  \
  "the processor is inside a chain of prefixes, and a routine can be called only between "         \
  "instructions; step or run to the end of the chain first"

#endif
