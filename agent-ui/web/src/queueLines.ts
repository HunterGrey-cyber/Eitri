import type { QueueItem } from "./types";

/** How many of the tab's typed messages wait for the turn's end: the `⧗` lines, which `↑` takes back. A
 *  message sent into the running turn (`↪`) is not one of them: the agent may already have read it. */
export function heldCount(items: readonly QueueItem[]): number {
  return items.filter((item) => item.steered !== true).length;
}

/** The empty box's words while a turn runs: what Enter does with the message in this tab, and, where Enter
 *  steers, the key that holds one for the turn's end instead. */
export function runningPlaceholder(steers: boolean): string {
  return steers ? "Enter steers the running turn, Ctrl+x Enter waits for its end…" : "Enter holds it until the turn ends…";
}
