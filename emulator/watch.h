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

/* After a tick, given the bus before it and after it. */
void mcp_watch_after(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor,
                     uint64_t before, uint64_t pins, uint64_t tick);

/* At the instruction a run ends before. */
void mcp_watch_end(mcp_watch_t *watch, const z80_t *cpu, const monitor_t *monitor, uint64_t tick);

/* Whether the run is to stop at the end of the instruction under way. */
bool mcp_watch_stopping(const mcp_watch_t *watch);

/* Why a routine is not called: a machine is stopped inside an instruction
   only by a chain of prefixes longer than a run finishes. */
#define MCP_INSIDE_AN_INSTRUCTION                                                                  \
  "the processor stands inside a chain of prefixes, and a routine is called only between "         \
  "instructions; step or run on to the end of the chain first"

#endif
