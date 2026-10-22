/** One line per page event on the console, which a probe build of the host prints; the format is fixed so a script
 *  can read it. */
export function trace(event: string, data: Record<string, unknown>): void {
  console.info("[page-trace] " + event + " " + JSON.stringify(data));
}
