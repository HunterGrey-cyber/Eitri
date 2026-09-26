/** How the panel's own scroll commands tell `MessageList` that the USER moved the conversation
 *  (2026-09-24).
 *
 *  `MessageList` follows a streaming reply while the reader is at the bottom, and stops when the
 *  reader leaves it. Until 2026-09-24 "left" meant "a scroll event saw `scrollTop` go down", which
 *  assumed only a user ever moves the view up. WebKitGTK broke that assumption: while `.row` was a
 *  size query container, the engine itself moved `scrollTop` up on every tick of the status line's
 *  clock, and the panel took that for the reader scrolling away (dated record, 2026-09-24 (later)).
 *  The container is gone, but the follow decision no longer trusts the direction of an unexplained
 *  scroll either: it listens for what the user DID -- a wheel, a touch drag, a pointer held on the
 *  list, and (fix round 1, the same day) the browser's own scroll keys from a control inside the
 *  list and focus landing inside it -- and, for the panel's own keys, for this event.
 *
 *  `App.tsx` moves the list itself for `j`/`k`, `Ctrl+d`/`Ctrl+u`, `G`/`gg` and a HINT landing,
 *  so it announces each one here, on the list, before it scrolls. A DOM event rather than a prop or
 *  a ref, because the scroll happens deep in `App.tsx`'s key handler and the list is `MessageList`'s
 *  own element; neither component has to hold the other's internals. */
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
