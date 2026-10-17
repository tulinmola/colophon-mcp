/*
 * mcp.h — the bridge between the core and the server.
 *
 * Whatever has to happen once a tick happens here, in C, and the server
 * crosses into it once a run.
 */
#ifndef COLOPHON_MCP_H
#define COLOPHON_MCP_H

#include <stdbool.h>
#include <stdint.h>

#include "cpc.h"
#include "spectrum.h"

/* What a run did: the ticks it took, the frames the monitor retraced in
   them, and whether it ended settled — between instructions and owing the
   machine nothing, which on a board that stops the processor's clock is
   later than the instruction's end. */
typedef struct {
  uint64_t ticks;
  uint32_t frames;
  bool settled;
} mcp_run_t;

/* Run until the monitor has retraced this many times or the limit in ticks
 * is reached, then on to the end of the instruction standing there, and
 * through the response to an interrupt taken at its end, to the first fetch
 * of its handler; that may pass the limit, and may cross a retrace that is
 * counted too. A frame is the
 * monitor's, and ends as its retrace sends the beam to the top-left corner. A
 * CPC's frame is whatever its CRTC is programmed to make, and may never end,
 * so there is always a limit. The end of the instruction is waited for only
 * as long as the core's own finishing waits for it, so a chain of prefixes
 * longer than that leaves the run unsettled. */
mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit);
mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit);

#endif
