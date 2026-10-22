/** chat-ui's hysteresis (show far from the bottom, hide only near it, so the pill does not flicker at
 *  one threshold), in Claude Code's words. */
export const PILL_SHOW_PX = 200;
export const PILL_HIDE_PX = 60;

export function pillShown(wasShown: boolean, following: boolean, distance: number): boolean {
  if (following) return false;
  return wasShown ? distance > PILL_HIDE_PX : distance > PILL_SHOW_PX;
}

/** `bin:` `` `${l} new …message` `` / "Jump to bottom"; a card beats a count, since a waiting card
 *  blocks the agent and a count only says there is more to read. */
export function pillLabel(newRows: number, card: boolean): string {
  if (card) return "↓ ⚑ approval";
  return newRows > 0 ? `↓ ${newRows} new` : "↓ Jump to bottom";
}
