/** R2 (spec §4.2): chat-ui's hysteresis, Claude Code's words. */
export const PILL_SHOW_PX = 200;
export const PILL_HIDE_PX = 60;

export function pillShown(wasShown: boolean, following: boolean, distance: number): boolean {
  if (following) return false;
  return wasShown ? distance > PILL_HIDE_PX : distance > PILL_SHOW_PX;
}

/** `bin:` `` `${l} new …message` `` / "Jump to bottom"; a card beats a count (ruling 23). */
export function pillLabel(newRows: number, card: boolean): string {
  if (card) return "↓ ⚑ approval";
  return newRows > 0 ? `↓ ${newRows} new` : "↓ Jump to bottom";
}
