/** How the panel's scroll commands tell `MessageList` that the user moved the conversation.
 *  Following stops on user intent, not an unexplained upward `scrollTop` change: WebKitGTK can
 *  move it during layout (it did so on every status-clock tick while `.row` was a size query
 *  container, which no longer exists).
 *  Wheel, touch drag, a pointer held on the list, browser scroll keys and focus inside it signal
 *  intent. `App.tsx` announces `j`/`k`, `Ctrl+d`/`Ctrl+u`, `G`/`gg` and HINT landings before
 *  scrolling. A DOM event keeps the deep key handler and `MessageList`'s element independent of
 *  each other's internals. */
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
 *  newest content again, wherever the view was. Claude Code does
 *  this, and a reader who scrolled up and then sends is asking for the reply, not for the older text
 *  they were reading -- so, unlike every other signal here, this re-arms following outright and snaps
 *  the view to the end at once. Only a send the user made announces it: a queue flushed at a turn's
 *  end is not a keypress of theirs, and `Enter` that only QUEUES behind a running turn sends nothing
 *  yet. */
export const RESUME_FOLLOW_EVENT = "nv-resume-follow";

export function resumeFollowing(list: Element | null | undefined): void {
  list?.dispatchEvent(new CustomEvent(RESUME_FOLLOW_EVENT));
}
