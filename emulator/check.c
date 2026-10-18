/*
 * check.c — the bounds the core trusts in the parts both machines share.
 */
#include <stddef.h>

#include "mcp.h"

/* z80.c appends an instruction's machine cycles to `cpu->program`, five at
   most, and takes them back out by `program_index`, both counts starting
   from 0 at every instruction's end. A program counted from anywhere else
   can append past the five, so a machine is written only between
   instructions; with both counts at 0, any other field of the processor is
   safe. */
const char *mcp_z80_check(const z80_t *written) {
  if (written->program_length != 0 || written->program_index != 0) {
    return "cpu.program_length and cpu.program_index count the steps of an instruction under "
           "way, and a machine is written only between instructions, where both are 0";
  }
  return NULL;
}

/* monitor.c draws
   `monitor->framebuffer[(size_t)monitor->beam_y * monitor->width + monitor->beam_x]`
   while the beam is inside width and height, which are the size of the
   framebuffer it was connected to. */
const char *mcp_monitor_check(const monitor_t *standing, const monitor_t *written) {
  if (written->width != standing->width || written->height != standing->height) {
    return "monitor.width and monitor.height are the size of the framebuffer the monitor draws "
           "into, and stay so";
  }
  return NULL;
}
