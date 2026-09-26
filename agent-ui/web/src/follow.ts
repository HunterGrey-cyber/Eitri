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

/** The user sent a message (`Enter`, or `Ctrl+Enter` interrupting a running turn): follow the
 *  newest content again, wherever the view was (2026-09-25, the phase-3 GUI pass). Claude Code does
 *  this, and a reader who scrolled up and then sends is asking for the reply, not for the older text
 *  they were reading -- so, unlike every other signal here, this re-arms following outright and snaps
 *  the view to the end at once. Only a send the user made announces it: a queue flushed at a turn's
 *  end is not a keypress of theirs, and `Enter` that only QUEUES behind a running turn sends nothing
 *  yet. */
export const RESUME_FOLLOW_EVENT = "nv-resume-follow";

export function resumeFollowing(list: Element | null | undefined): void {
  list?.dispatchEvent(new CustomEvent(RESUME_FOLLOW_EVENT));
}
