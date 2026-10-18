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

/* What a run did. */
typedef struct {
  /* The ticks it took. */
  uint64_t ticks;
  /* The frames the monitor retraced in them. */
  uint32_t frames;
  /* Whether it ended between instructions and owing the machine nothing,
     which on a board that stops the processor's clock is later than the
     instruction's end. */
  bool settled;
} mcp_run_t;

/* Run until the monitor has retraced this many times or the limit in ticks
 * is reached, then on to the end of the instruction standing there, and
 * through the response to an interrupt taken at its end, to the first fetch
 * of its handler; that may pass the limit, and may cross a retrace that is
 * counted too. A frame is the monitor's, and ends as its retrace sends the
 * beam to the top-left corner. A CPC's frame is whatever its CRTC is
 * programmed to make, and may never end, so there is always a limit. The end
 * of the instruction is waited for only as long as the core's own finishing
 * waits for it, so a chain of prefixes longer than that leaves the run
 * unsettled. */
mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit);
mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit);

/* Run this many instructions, each as a run limited to one tick: on to its
 * end and through the response to an interrupt taken there. A halted
 * processor's instruction is one of the fetches it idles on. Stops at the
 * first that does not settle, as a chain of prefixes does not. */
mcp_run_t mcp_cpc_step(cpc_t *cpc, uint32_t instructions);
mcp_run_t mcp_spectrum_step(spectrum_t *spectrum, uint32_t instructions);

/* Whether the core can run from a machine written from outside: NULL if it
 * can, or a sentence naming the first field it would trust past what that
 * field may hold. The core indexes and sizes by some of its fields without
 * checking them, because nothing it does itself can take them out of range;
 * a write from outside can, and these hold it to the same ranges, and keep
 * a processor from being held for good. `standing` is the machine as it
 * stands, whose buffers are of the sizes it was handed when it was built. */
const char *mcp_cpc_check(const cpc_t *standing, const cpc_t *written);
const char *mcp_spectrum_check(const spectrum_t *standing, const spectrum_t *written);

/* Stand a Spectrum's ULA at the T-state of the frame its frame_tick names, as
 * anything that places the chip from outside must: its beam counters are
 * derived from that tick (ula.h, ula_seek). */
void mcp_spectrum_seek(spectrum_t *spectrum);

/* The checks of the parts both machines have. */
const char *mcp_z80_check(const z80_t *written);
const char *mcp_monitor_check(const monitor_t *standing, const monitor_t *written);

#endif
