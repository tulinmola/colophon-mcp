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

/* What a condition watches for. */
typedef enum {
  /* An instruction beginning at an address in the range, matched before it
     begins. A halted processor's idle fetches begin none. */
  MCP_EXECUTE,
  /* The program reading a byte of memory in the range as data. The bytes of
     an instruction are not data: its opcodes carry M1, and its operands are
     told apart by z80_reading_operand. */
  MCP_READ,
  /* The processor writing a byte of memory in the range. */
  MCP_WRITE,
  /* The processor reading a port whose address agrees with `first` on the
     bits of `mask`. */
  MCP_IN,
  /* The processor writing such a port. */
  MCP_OUT,
} mcp_kind_t;

/* How many kinds mcp_kind_t gives, and how many places each can name: an
   address or a port, sixteen bits of either. */
#define MCP_KINDS 5
#define MCP_COUNTS_PER_KIND 0x10000

/* The list a condition is given in, which says what its matches do. */
typedef enum {
  /* A match stops the run. */
  MCP_UNTIL,
  /* A match is kept, in the order it happened. */
  MCP_GATHER,
  /* A match is counted, by its kind and its address. */
  MCP_COUNT,
} mcp_list_t;

typedef struct {
  mcp_kind_t kind;
  /* The first address of the range, or the port. */
  uint16_t first;
  /* The last address of the range. */
  uint16_t last;
  /* The bits of a port's address that its device decodes. */
  uint16_t mask;
  mcp_list_t list;
} mcp_condition_t;

typedef struct {
  /* The ticks into the run: before an instruction begins, or once the tick
     that put an access on the bus has run. */
  uint64_t tick;
  mcp_kind_t kind;
  /* The address, or the port. */
  uint16_t address;
  /* The byte read or written, a port's as the processor took it; 0 for an
     instruction. */
  uint8_t value;
  /* Where the instruction that made an access begins, unless the response to
     an interrupt made it. */
  uint16_t pc;
  bool response;
  uint16_t beam_x;
  uint16_t beam_y;
  /* Whether it matched a condition that stops the run, and one that
     gathers. */
  bool stops;
  bool gathers;
} mcp_match_t;

/* What a run watches for and what it saw. The caller sets what comes before
 * match_count and zeroes the rest; the run keeps the rest. */
typedef struct {
  const mcp_condition_t *conditions;
  uint32_t condition_count;
  /* Where the matches are kept, in the order they happened. */
  mcp_match_t *matches;
  uint32_t room;
  /* The run stops at the end of the instruction that gathers this many. */
  uint32_t most_gathered;
  /* Where the matches of the count list are counted, from zero:
     MCP_COUNTS_PER_KIND counts to a kind, by address, for each of the
     MCP_KINDS kinds in the order mcp_kind_t gives them; NULL if the list is
     empty. */
  uint32_t *counts;
  /* The run stops at the end of the instruction that counts at this many
     places, a place being a kind and an address. */
  uint32_t most_counted;
  /* A call stops the run once the processor stands at return_pc with its
     stack at return_sp, where the routine's own return leaves it. */
  bool calling;
  uint16_t return_pc;
  uint16_t return_sp;

  /* Every match, which may be more than there is room for. */
  uint32_t match_count;
  uint32_t gathered;
  uint32_t counted;
  /* Why the run stopped, if the watch stopped it. */
  bool matched;
  bool full;
  bool returned;

  /* The run's own reckoning, from one tick to the next. */
  bool begun;
  bool responding;
  uint16_t pc;
  /* A port read matched and kept, and still on the bus: which match it is. */
  bool port_read_held;
  uint32_t port_read_match;
} mcp_watch_t;

/* Run until the monitor has retraced this many times or the limit in ticks
 * is reached, then on to the end of the instruction standing there, and
 * through the response to an interrupt taken at its end, to the first fetch
 * of its handler; that may pass the limit, and may cross a retrace that is
 * counted too. A frame is the monitor's, and ends as its retrace sends the
 * beam to the top-left corner. A CPC's frame is whatever its CRTC is
 * programmed to make, and may never end, so there is always a limit. The end
 * of the instruction is waited for only as long as the core's own finishing
 * waits for it, so a chain of prefixes longer than that leaves the run
 * unsettled.
 *
 * A watch, if given, stops the run sooner: before an instruction a stopping
 * condition matches, at the end of the instruction whose access matched one,
 * at the end of the instruction whose match fills the gathering or the
 * count, or once a call has returned; the ends are reached as the run's own
 * end is. The instruction a run begins on is matched only as it begins,
 * since the run that stopped before it has matched it already.
 *
 * A run also stops at the end of a HALT that leaves the processor halted for
 * good, as mcp_z80_halted_for_good tells, watched or not, once the
 * processor has stood anywhere else in the run: one that begins halted so
 * runs as asked, unless an NMI written in from outside wakes it first. */
mcp_run_t mcp_cpc_run(cpc_t *cpc, uint32_t frames, uint64_t limit, mcp_watch_t *watch);
mcp_run_t mcp_spectrum_run(spectrum_t *spectrum, uint32_t frames, uint64_t limit,
                           mcp_watch_t *watch);

/* Whether the processor waits on HALT with IFF1 clear, which it does for good
 * on either machine: INT is refused while IFF1 is clear, and neither drives
 * NMI, the only other way out of HALT but a reset, so only an NMI written in
 * from outside wakes it. */
bool mcp_z80_halted_for_good(const z80_t *cpu);

/* Call the routine at this address: push the program counter, as the
 * processor's CALL does, and jump. NULL if called, or a sentence saying why
 * not, the machine left as it stood: the processor is inside an instruction,
 * or the two bytes pushed would not read back where the routine's own return
 * reads them. */
const char *mcp_cpc_call(cpc_t *cpc, uint16_t address);
const char *mcp_spectrum_call(spectrum_t *spectrum, uint16_t address);

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
