/** Announce panel scroll commands to `MessageList` before moving the conversation.
 *  Following must respond to user intent, since the engine can also move `scrollTop` upward.
 *  In WebKitGTK, CSS size query containers on rows caused such movement on status clock ticks.
 *  `MessageList` tracks wheels, touch drags, held pointers, browser scroll keys and focus changes.
 *  For `j`/`k`, `Ctrl+d`/`Ctrl+u`, `G`/`gg` and HINT landings, `App.tsx` sends this event on the list
 *  before scrolling. A DOM event lets its key handler communicate with `MessageList`'s element
 *  without either component holding the other's internals. */
export const USER_SCROLL_EVENT = "nv-user-scroll";

/** `up`: the user is moving toward older text, so following stops at once -- before the scroll's own
 *  event, which arrives a frame later, and before the next streamed delta could snap the view back.
 *  `down` / `unknown`: the scroll that follows is the user's, so its direction decides (down to the
 *  bottom re-arms following; up stops it). */
export type UserScrollDirection = "up" | "down" | "unknown";

export function noteUserScroll(list: Element | null | undefined, direction: UserScrollDirection): void {
  list?.dispatchEvent(new CustomEvent<UserScrollDirection>(USER_SCROLL_EVENT, { detail: direction }));
}
