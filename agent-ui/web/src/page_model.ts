import { EMPTY_PANEL_TABLE } from "./keymap";
import type { KeymapHelp } from "./keymap";
import type { TrustPromptEnvelope } from "./trust";
import type { NavKeyDirection } from "./bridge";
import type { InboundPayload } from "./bridge";
import { isMirrorRequest } from "./bridge";

/** The host's words for a mirror whose question, tab or session is already gone. */
const STALE_MIRROR_REFUSAL = /no longer waiting|no tab \d+|no such tab|has no live session/;
import { withChooserTitles } from "./chooser";
import { failureEvidence } from "./problems";
import { applyCallNotes, applyEvents, applySnapshot, initialState, keepAfterFailure, resumeAttached, withEditorCards } from "./reducer";
import { failRequest, receiveDiff, receiveDraft, receivePreview, receiveRecovery, receiveReview, withStatus } from "./review";
import type { ReviewState } from "./review";
import { acceptsEnvelope, withoutHandoff } from "./tabs";
import { durationsFromSnapshot, durationsWith, NO_TURN_DURATIONS } from "./turnTimes";
import type { TurnDurations } from "./turnTimes";
import { REVIEW_SETTINGS_OFF, THINKING_DISPLAYS } from "./types";
import type {
  AgentUiState, ChooserEnvelope, ContextSummary, EditorLink, HandoffCommand, Hello, QueueItem,
  ReviewRecoveryEntry, ReviewSettings, TabId, TabsEnvelope, ThinkingDisplay, TurnClock,
} from "./types";

/** Receipt time is supplied by the caller so replaying an envelope never reads a clock. */
export type Envelope = InboundPayload & { receivedAt: number; monotonicAt?: number };

/** What an in-flight request actually was, so its reply can be handled as that thing.
 *
 *  Only the kinds whose replies need special handling are recorded. An interrupt has no entry and comes back as
 *  `undefined`, which is correct rather than a gap: its refusal takes the plain "show the reason" path. Every
 *  recorded request gets exactly one answer that ends its record (a `command_result`, or for the review overlay's
 *  requests the envelope that answers them), so the table cannot grow.
 *
 *  - `"send"`: an ordinary send. Its text really did leave the composer box (the composer clears it
 *    optimistically), so a refusal puts it back there.
 *  - `"picker-send"`: the `/model`/`/effort` picker's own send. It posts the same `send_message`, but the choice
 *    never touched the box, so restoring it would corrupt whatever the box holds right then (a draft the user
 *    quoted or typed for a wholly unrelated reason). Its refusal is a footer flash alone.
 *  - `"handoff"`: a `handoff_to_terminal`; its answer ends the tab's closing state.
 *  - `"permission"`, with its card's id: Rust can refuse an answer and leave the card waiting (an "Always allow"
 *    whose rule could not be saved), and the card must then take an answer again instead of staying answered here
 *    for good.
 *  - `"editor"`: `edit_draft`, `open_path`, `view_in_editor`. All three reach the scratch editor in Rust, and a
 *    refusal of any of them is a footer flash, not the banner: a scratch-editor round trip that failed to start is
 *    a nicety, not something that should fill the space a real conversation error gets.
 *  - `"link"`: `gx`'s `open_url`. Rust refuses an address that is not plain http(s); the same footer flash.
 *  - `"review"`: the review overlay's `review_request`/`review_diff_request`. Rust refuses one with a reason (no
 *    session, no such turn, git unavailable) and no envelope follows, so the overlay says it in its own header
 *    rather than in the conversation's banner.
 *  - `"review-command"`: what the overlay's keys do beyond reading (revert, undo, comment, send, recover). A
 *    refusal, or a word of success, is the overlay's status line; a comment's answer is a `review_draft` and a
 *    send's first answer a `review_send_preview`, which end the record without a `command_result`.
 *  - `"trust"`: a trust question's answer or `Escape`, and `:trust`/`:untrust`. Rust says in a notice what the
 *    user needs to know about one (out of date, asked again, could not be recorded); a refusal without one is an
 *    `n`, an `Escape` or a move away from the question, which the user did themselves. Neither is drawn again,
 *    and never in the banner reserved for what breaks the conversation.
 *  - `"tabnew"`: `:tabnew <agent>`. A word Rust does not know as an agent is refused naming the accepted ones, a
 *    footer flash like `gx`'s.
 *  - `"stop-background"`: the confirmed `<leader>k`. Rust refuses one whose moment has passed (a turn started,
 *    the commands ended, the count moved), and the session goes on, so a flash says why.
 *  - `"follow"`: `<leader>e`. Rust refuses it for a tab a float covers, one that is moving, or one that is gone; the
 *    page has already said what it expects, so the refusal is the flash that follows it. */
export type CommandRecord = {
  kind: "send" | "handoff" | "editor" | "link" | "permission" | "picker-send" | "review" | "review-command" | "trust" | "tabnew" | "stop-background" | "follow";
  tab: TabId;
  text?: string;
  permissionId?: string;
  /** An `open_in_editor` from the review overlay: its answer, good or bad, is the overlay's status line. */
  review?: true;
};

/** The footer's y/n prompt, or `null` when none is open. It is drawn in place of the which-key strip and takes
 *  every key while it is open. Every kind that asks before something irreversible (`bypass`, `restore`, `trust`,
 *  `stop_background`) carries `openedAt`, stamped on the monotonic clock the moment its envelope is recorded, so
 *  it shares a clock with the guard that refuses a `y` given before the prompt had been on screen long enough. A
 *  second envelope while one is open replaces the whole object, restarting that wait along with everything else.
 *  Values that came from Rust (`tab`, `scope`, `nonce`, `lines`) are echoed back verbatim on the answer: this
 *  side never counts cards or invents a nonce. */
export type Confirmation =
  /** Close one named tab (`prefix &`, or the chooser's own `x`). */
  | { kind: "close"; tab: TabId; lines: string[] }
  /** Close every other tab; window-level, so it names no tab. */
  | { kind: "close_others"; lines: string[] }
  /** A move into bypass. */
  | { kind: "bypass"; tab: TabId | null; scope: "tab" | "default"; nonce: number; lines: string[]; openedAt: number }
  /** A restore found a tab saved in bypass and asks before giving it back: `y` as saved, `n` in auto. A `y` here
   *  also enters bypass, so it gets the same wait and lone-key guard. */
  | { kind: "restore"; nonce: number; lines: string[]; openedAt: number }
  /** The question before this tab's session loads the project's own configuration for its agent. `view` is the
   *  envelope as shown, and an answer echoes its `fingerprint` and `findingsDigest` and nothing recomputed here;
   *  a `y` loads a repository's hooks. `flashAfter` is the flash counter when it opened: only a flash made since
   *  then is about this question. */
  | { kind: "trust"; tab: TabId; nonce: number; view: TrustPromptEnvelope; lines: string[]; openedAt: number; flashAfter: number }
  /** `<leader>k`: stop the commands this tab's agent left running, which ends its session. Bound to the tab and
   *  the count it names: a `y` given after either moved is not an answer to it. */
  | { kind: "stop_background"; tab: TabId; running: number; lines: string[]; openedAt: number }
  | null;

/** Everything the envelope router decides. Counters (`inputRequest`, `arriveRequest`, `permissionRequest`,
 *  `navKey.seq`, `queueTaken.seq`, `answerEpoch`) rather than flags, so two envelopes in a row both act and the
 *  effects that carry them out decide against the render they run in. */
export type PageModel = {
  /** Bumped by every envelope that takes the keys or the view from a card a waiting `a`/`d`/`D` was aimed at
   *  (a pane focus change, an arrival, a landing, a HINT, a key GTK claimed, a new key table) and by a switch
   *  of tab. The page cancels the waiting answer whenever it moves. A new key table is in the list because a
   *  deferred single-key binding closed over the old table's binding when it was pressed. */
  answerEpoch: number;
  /** Bumped by each `enter_input`, whose only sender is a brand-new tab: a plain "open the composer" request,
   *  with never a card to land on instead. Also handed to the composer, which re-focuses a textarea that is
   *  already mounted, since entering INPUT alone does nothing when the mode was already INPUT. */
  inputRequest: number;
  /** Bumped by each `arrive`: every keyboard arrival into the chat (`Ctrl+h/j/k/l`, `prefix a` or a tray chip
   *  with no card, a launch that starts with the keys in the chat). The effect decides against the current
   *  render whether a card is waiting (it lands there) or the panel lands BROWSE -- on the last row with
   *  following resumed for a reader who was following, else where they were -- never the composer. */
  arriveRequest: number;
  /** Bumped by each `focus_permission` (the tray's `agent ⚑N` chip, or `prefix a` with a card waiting); the
   *  effect finds the card against the current render. */
  permissionRequest: number;
  /** A chord GTK claimed before the WebView saw it, with its direction. Whether an overlay owns the keys or the
   *  empty tab gets it is decided in an effect against the current mode and overlays. */
  navKey: { seq: number; direction: NavKeyDirection } | null;
  /** The keys overlay's last two sections and its heading, from `shell`'s `keymap` envelope (sent on every
   *  `ready`, right after the theme). `prefix` is `Ctrl+b`, the stock tmux default, and the panel table has no
   *  bindings until that envelope arrives. */
  keymapHelp: KeymapHelp;
  /** The trust question Rust sent last, held until it can take the keys. It is not drawn on arrival: Rust asks
   *  again whenever a tab waiting for the answer is on screen without its question, including the moment after a
   *  close prompt, a rename field or the chooser took the keys. Drawing it at once could take the `y` meant for
   *  that prompt or typed into that field. */
  heldTrust: TrustPromptEnvelope | null;
  confirm: Confirmation;
  /** The inline rename field's tab and prefilled text (`prefix ,`), or `null`. The tab bar is shown even with
   *  one tab while this is set. */
  renaming: { tab: TabId; initial: string } | null;
  /** `queue_taken`, for the composer to merge into its box. */
  queueTaken: { texts: string[]; seq: number } | null;
  queueTakenSequence: number;
  state: AgentUiState;
  hello: Hello | null;
  /** Every tab and which is active, from Rust's `tabs` envelope; `null` until the first one arrives. */
  tabs: TabsEnvelope | null;
  /** The conversation each failed tab showed when its session stopped under it, so it stays on screen as a lost
   *  session -- across a switch away and back too, since Rust sends no snapshot for a failed tab. Dropped once
   *  the tab is no longer failed (`r`) or is closed. */
  keptStates: ReadonlyMap<TabId, AgentUiState>;
  /** A fatal, session-ending failure, shown in the panel so it can be copied and can carry multi-line
   *  startup diagnostics without blocking the WebView as `window.alert` would. */
  fatalError: string | null;
  /** The command for a conversation that has just been closed here and moved to a terminal, set by the `handoff`
   *  envelope, which Rust sends only after the real session close finished.
   *
   *  This is a view of Rust's own `last_handoff`, not the only copy: Rust keeps the command and re-sends it in
   *  the `ready` handshake, so a panel reload or a WebView crash gets it back -- on the legacy backend the id in
   *  it is recoverable from nowhere else at all. Cleared here when a real `snapshot` arrives, which is also when
   *  Rust clears its copy: a session that is actually running, not one merely asked for. */
  handoff: HandoffCommand | null;
  /** The request id of each tab's in-flight `handoff_to_terminal`, keyed by the tab it closes.
   *
   *  An entry means that tab's conversation is CLOSING: Rust has already taken the backend out of its own state
   *  and a worker thread is running the real shutdown. Nothing is torn down here until the `handoff` envelope
   *  arrives, so without this the composer would stay live and a typed Enter would clear the box into a session
   *  that no longer exists. Per tab, so handing off tab 1 does not lock a live tab 2's composer while tab 1's
   *  close is pending. An entry is removed by its own `command_result` (Rust owes one for every handoff, including one
   *  whose tab was closed), or by a `handoff`/`error` envelope naming its tab. */
  handoffRequests: ReadonlyMap<TabId, string>;
  /** A refused command, in words, on screen so Rust's human-readable reason reaches the user. */
  commandNotice: string | null;
  /** Why the project folder is no boundary, as the `root_note` envelope said. Its own field: a send, a snapshot
   *  and a refused command's notice never touch it, only its Dismiss (or a reloaded document, which is told
   *  again) does. */
  rootNote: string | null;
  /** See `CommandRecord`. Bookkeeping that nothing draws: the page records an outgoing request without a render
   *  and reads this only when the answer is reduced. */
  inFlight: ReadonlyMap<string, CommandRecord>;
  /** How long the current turn has been running, for the activity line's elapsed clock. `exact` only from a real
   *  `turn_started` inside an `events` envelope, or from a snapshot that says when the turn started; a turn id
   *  first seen inside a snapshot without that (a reload, a resync mid-turn) gets `exact: false`, drawn as "at
   *  least", because this page cannot know how long it had already been running. The clock never shows a number
   *  it cannot stand behind. */
  turnClock: TurnClock | null;
  /** How long each finished turn of the active tab ran, by turn id (stamped by Rust's tab set): the "Worked for"
   *  line under a turn and the duration on a turn-ending row. A snapshot replaces the whole table, an events
   *  envelope adds the turns that ended in it, and a switch empties it until the new tab's snapshot lands. */
  turnDurations: TurnDurations;
  /** How thinking is shown (`init.lua`'s `agent.thinking`), as the `display` envelope last said. */
  thinkingDisplay: ThinkingDisplay;
  /** Whether this pane has the window's keyboard focus, as `shell` reports it. `false` at first because `shell`
   *  focuses the editor at startup, and `ready` re-sends the real value to every freshly loaded document. The mode
   *  block is bright only while this is `true`, the one sign left of which pane has the keys. */
  paneFocused: boolean;
  /** The queue behind a running turn, with Rust's error and its note on why messages wait for the turn's end. */
  queue: QueueItem[];
  queueError: string | null;
  queueNote: string | null;
  /** Whether Enter puts a message into the running turn, as the tab's latest queue envelope says (`null` before
   *  one came): it follows the agent live, which the snapshot's capability does not. */
  queueSteers: boolean | null;
  /** The shared prompt history; window-wide, like `editorContext`. */
  history: string[];
  ruleOffers: Record<string, string>;
  editorContext: ContextSummary | null;
  /** Where the panel stands with the editor beside it in companion mode; `null` in the one-window mode, where
   *  Rust never sends one. */
  editorLink: EditorLink | null;
  /** Whether the scratch editor currently holds this tab's draft. */
  scratchEditing: boolean;
  /** The turn review overlay, or `null` when it is closed. Drawn over the conversation and owning every key while
   *  open; closed wherever the keys overlay is forced shut, and by a switch of tab. */
  review: ReviewState | null;
  /** The finished turn of each tab that changed files, for the band -- kept per tab, because the shell sends it
   *  for the tab it names even while another is on screen. Gone once the overlay is opened on that turn, when a
   *  new turn starts, or when the shell sends `files: 0`. */
  reviewHints: Record<number, { turn: number; files: number }>;
  /** Interrupted reverts the journal still holds (the project's, so not a tab's). Nothing is kept but what the
   *  shell last said. */
  reviewRecovery: ReviewRecoveryEntry[];
  /** The window's turn review switches; off until the shell says otherwise, so a host that never sends them
   *  leaves the change line undrawn. */
  reviewSettings: ReviewSettings;
  /** `prefix w`'s open-tabs-then-records overlay, or `null`. Window-wide, not a view of the active tab. */
  chooser: ChooserEnvelope | null;
};

export function initialPageModel(): PageModel {
  return {
    answerEpoch: 0, inputRequest: 0, arriveRequest: 0, permissionRequest: 0, navKey: null,
    keymapHelp: { prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "" },
    heldTrust: null, confirm: null, renaming: null, queueTaken: null, queueTakenSequence: 0,
    state: initialState(), hello: null, tabs: null, keptStates: new Map(), fatalError: null,
    handoff: null, handoffRequests: new Map(), commandNotice: null, rootNote: null, inFlight: new Map(),
    turnClock: null, turnDurations: NO_TURN_DURATIONS, thinkingDisplay: "collapsed", paneFocused: false,
    queue: [], queueError: null, queueNote: null, queueSteers: null, history: [], ruleOffers: {},
    editorContext: null, editorLink: null, scratchEditing: false, review: null, reviewHints: {},
    reviewRecovery: [], reviewSettings: REVIEW_SETTINGS_OFF, chooser: null,
  };
}

/** `hints` without `tab`'s entry -- the same object when it has none, so a clear that changes nothing renders
 *  nothing. */
export function withoutHint(hints: PageModel["reviewHints"], tab: TabId): PageModel["reviewHints"] {
  if (hints[tab] === undefined) return hints;
  const rest = { ...hints };
  delete rest[tab];
  return rest;
}

function withoutRequest(model: PageModel, requestId: string | null): PageModel["inFlight"] {
  if (requestId === null || !model.inFlight.has(requestId)) return model.inFlight;
  const requests = new Map(model.inFlight);
  requests.delete(requestId);
  return requests;
}

/** Each result is the complete input for the next envelope, including envelopes in one dispatch batch. */
export function pageModel(model: PageModel, envelope: Envelope): PageModel {
  // Every session-scoped envelope names its tab, and the page keeps only the active tab's state, so one for any
  // other tab -- a batch still in flight when the user switched away -- changes nothing.
  if (!acceptsEnvelope(envelope as { kind: string; tab?: number }, model.tabs?.active ?? null)) return model;
  const next = reduceEnvelope(model, envelope);
  return ["pane_focus", "arrive", "enter_input", "focus_permission", "hint_collect", "nav_key", "keymap"].includes(envelope.kind)
    ? { ...next, answerEpoch: model.answerEpoch + 1 } : next;
}

function reduceEnvelope(model: PageModel, envelope: Envelope): PageModel {
  switch (envelope.kind) {
    // The review overlay is drawn over the same area as the keys overlay and ends wherever that one is forced
    // shut: every route that lands the keys somewhere, opens another overlay, or starts a HINT.
    case "enter_input":
      return { ...model, inputRequest: model.inputRequest + 1, review: null };
    case "arrive":
      return { ...model, arriveRequest: model.arriveRequest + 1, review: null };
    case "focus_permission":
      // The chooser would cover the card the cursor is about to land on.
      return { ...model, permissionRequest: model.permissionRequest + 1, review: null, chooser: null };
    case "open_keymap":
    case "tab_detail":
      return { ...model, review: null };
    case "open_command_line":
    case "hint_collect":
      // A HINT started elsewhere in the window must not label rows hidden under an overlay, and the `:` line
      // takes the keys from every overlay over the conversation.
      return { ...model, review: null, chooser: null };
    case "nav_key":
      return { ...model, navKey: { seq: (model.navKey?.seq ?? 0) + 1, direction: envelope.direction } };
    case "keymap":
      return {
        ...model, keymapHelp: {
          prefix: envelope.prefix, window: envelope.window, prefixKeys: envelope.prefixKeys,
          panel: envelope.panel, newTabChord: envelope.newTabChord, tmuxSkipped: envelope.tmuxSkipped ?? [],
        }
      };
    case "begin_rename":
      return { ...model, renaming: { tab: envelope.tab, initial: envelope.current ?? "" } };
    case "confirm_close":
      return { ...model, confirm: { kind: "close", tab: envelope.tab, lines: envelope.lines } };
    case "confirm_close_others":
      return { ...model, confirm: { kind: "close_others", lines: envelope.lines } };
    case "confirm_bypass":
      // The one prompt that leaves an open chooser open: it is drawn over the chooser, where Shift+Tab cycles the
      // mode without leaving it. Every other prompt and overlay that takes the keys closes the chooser.
      return {
        ...model, confirm: {
          kind: "bypass", tab: envelope.tab, scope: envelope.scope, nonce: envelope.nonce,
          lines: envelope.lines, openedAt: envelope.monotonicAt ?? envelope.receivedAt,
        }
      };
    case "confirm_restore":
      return {
        ...model, confirm: {
          kind: "restore", nonce: envelope.nonce, lines: envelope.lines,
          openedAt: envelope.monotonicAt ?? envelope.receivedAt,
        }
      };
    case "trust_prompt":
      return { ...model, heldTrust: envelope };
    case "queue_taken":
      return {
        ...model, queueTakenSequence: model.queueTakenSequence + 1,
        queueTaken: { texts: envelope.texts, seq: model.queueTakenSequence + 1 }
      };
    case "hello":
      return { ...model, hello: envelope };
    case "display":
      // A word this build does not know keeps the level it has, rather than drawing thinking some way nobody
      // asked for.
      return (THINKING_DISPLAYS as readonly string[]).includes(envelope.thinking)
        ? { ...model, thinkingDisplay: envelope.thinking } : model;
    case "root_note":
      return typeof envelope.text === "string" && envelope.text !== "" ? { ...model, rootNote: envelope.text } : model;
    case "review_settings":
      // Only a real `true` turns a switch on: anything else from the wire leaves the line undrawn.
      return { ...model, reviewSettings: { enabled: envelope.enabled === true, hint: envelope.hint === true } };
    case "pane_focus":
      // The review overlay stays open: this envelope also reports the window being deactivated and activated
      // again (another window taking focus, a resize that comes with one), and an overlay the reader was in the
      // middle of must survive that. A way back by key is an `arrive`, which closes it.
      return { ...model, paneFocused: envelope.focused };
    case "tabs": {
      // A switch to a different tab clears every piece of per-conversation state before the new tab's own
      // snapshot (if it has one) arrives: the page holds only the active tab's state, so nothing may carry over
      // from the tab just left. The first `tabs` this page sees takes this branch too.
      const switched = envelope.active !== model.tabs?.active;
      const keptStates = new Map(model.keptStates);
      let state = switched ? initialState() : model.state;
      // A kept conversation lives only while its tab is still failed.
      for (const id of keptStates.keys()) {
        const tab = envelope.tabs.find((entry) => entry.id === id);
        if (tab?.state === "failed") continue;
        keptStates.delete(id);
        // The tab on screen was restarted (`r`, a first send): the lost conversation and its reason must not
        // linger under the new session until its first snapshot lands.
        if (tab !== undefined && id === envelope.active && !switched) state = initialState();
      }
      // A switch to a failed tab brings its kept conversation back, since no snapshot follows for it.
      if (switched) state = keptStates.get(envelope.active) ?? state;
      // The banner belongs to the session that ended. Once the active tab is starting a new one or running it,
      // that reason is not about this tab any more; a start that fails again sends its own `error`.
      // `awaiting_trust` keeps it: nothing has started under it, and a waiting tab draws no ended-session notice
      // of its own.
      const activeState = envelope.tabs.find((tab) => tab.id === envelope.active)?.state;
      return {
        ...model,
        ...(switched ? {
          answerEpoch: model.answerEpoch + 1, queueTaken: null,
          turnClock: null, turnDurations: NO_TURN_DURATIONS, handoff: null, fatalError: null,
          commandNotice: null, queue: [], queueError: null, queueNote: null, queueSteers: null,
          ruleOffers: {}, scratchEditing: false, chooser: null,
        } : {}),
        state, keptStates,
        fatalError: switched || activeState === "starting" || activeState === "live" ? null : model.fatalError,
        tabs: { active: envelope.active, tabs: envelope.tabs, defaultMode: envelope.defaultMode },
      };
    }
    case "snapshot": {
      // A turn id first seen inside a snapshot gets the time the envelope says the turn started (exact), or,
      // without one, the receipt time as an "at least" reading. The same turn already tracked keeps its clock (a
      // resync must not restart it) unless only now is its real start known.
      const activeTurnId = envelope.state.activeTurnId;
      const startedAt = envelope.turnStartedAtMs;
      let turnClock = model.turnClock;
      if (activeTurnId === null) turnClock = null;
      else if (turnClock?.turnId !== activeTurnId || (!turnClock.exact && typeof startedAt === "number")) {
        turnClock = { turnId: activeTurnId, since: startedAt ?? envelope.receivedAt, exact: typeof startedAt === "number" };
      }
      // A snapshot means a session is genuinely RUNNING, which is also when Rust clears its own copy of the
      // handoff command. Clearing it when a start was merely requested would throw it away on a start that then
      // failed -- and on the legacy backend that is the last reference to a conversation nothing else
      // remembers. A banner left from an earlier session of this tab (a failed start that `r` redid) is stale for
      // the same reason.
      return {
        ...model, state: applySnapshot(model.state, envelope.state, envelope.throughRevision),
        turnClock, turnDurations: durationsFromSnapshot(envelope.turnTimes),
        handoff: null, commandNotice: null, fatalError: null,
      };
    }
    case "editor_cards":
      // The tab on screen (the router dropped any other's): its broker cards, replaced whole.
      return { ...model, state: withEditorCards(model.state, envelope.cards) };
    case "events": {
      // The clock's exact provenance is a real `turn_started` inside this batch. Everything that ends a turn
      // clears it, mirroring what clears `state.activeTurnId` in the conversation reducer -- read here off the raw
      // events because the folded state does not carry which envelope a turn id arrived in.
      let turnClock = model.turnClock;
      for (const event of envelope.events) {
        if (event.type === "turn_started") {
          if (turnClock?.turnId !== event.turn_id) turnClock = { turnId: event.turn_id, since: envelope.receivedAt, exact: true };
        } else if (event.type === "turn_completed" || event.type === "session_unavailable" ||
          event.type === "session_closed" || (event.type === "resume_outcome" && !resumeAttached(event))) {
          turnClock = null;
        }
      }
      return {
        ...model, state: applyCallNotes(applyEvents(model.state, envelope.events, envelope.revisions), envelope),
        turnClock, turnDurations: durationsWith(model.turnDurations, envelope.turnTimes),
      };
    }
    case "command_result": {
      const record = model.inFlight.get(envelope.requestId);
      // A handoff's answer ends the closing state of the tab it was sent from, which need not be the one on
      // screen now: either the handoff finished (the `handoff` envelope came first and reset everything) or it
      // was refused and the session is untouched.
      let next = {
        ...model, inFlight: withoutRequest(model, envelope.requestId),
        handoffRequests: record?.kind === "handoff"
          ? withoutHandoff(model.handoffRequests, record.tab, envelope.requestId) : model.handoffRequests,
      };
      if (record?.kind === "review-command" || record?.review === true) {
        // The overlay's own command: the shell's word on what it did, or why it refused, goes on the overlay's
        // status line, where the key was pressed -- not the conversation's banner, and not a flash that would
        // fade before it was read.
        const message = envelope.ok ? envelope.message : envelope.error;
        if (message !== undefined && next.review !== null) next = { ...next, review: withStatus(next.review, message) };
      } else if (!envelope.ok) {
        if (record?.kind === "send") {
          // On the tab on screen the page puts the text back in the box; a message that vanishes with no trace
          // is the outcome this exists to prevent. A tab switched away from before Rust answered must not get it
          // in a different tab's box, so the refusal names the tab and carries the full text instead.
          next.commandNotice = record.tab === model.tabs?.active
            ? `That message was not sent (${envelope.error}). It is back in the box.`
            : `A message to tab ${record.tab} was not sent (${envelope.error}): ${record.text}`;
        } else if (record?.kind === "review") {
          // Said in the overlay's own header; the banner is for what breaks the conversation.
          if (next.review !== null) next.review = failRequest(next.review, envelope.requestId, envelope.error);
        } else if (!record && isMirrorRequest(envelope.requestId) && STALE_MIRROR_REFUSAL.test(envelope.error)) {
          // A mirror refused because what it mirrored is gone (its question settled, its tab or session closed):
          // nothing waited for it and the conversation is untouched, so it is not the user's error. The page's
          // log keeps it. Any other refusal of a mirror (a draft too large to keep) means the person's text will
          // not survive a switch, so it is said like any other refusal.
        } else if (!record || !["trust", "picker-send", "editor", "link", "tabnew", "stop-background", "follow"].includes(record.kind)) {
          // The kinds left out are already said (trust) or are a footer flash the page shows; everything else,
          // a refused permission answer included, is the banner.
          next.commandNotice = envelope.error;
        }
      }
      return next;
    }
    case "handoff":
      // The session is genuinely closed by the time this arrives (Rust dispatches it only after its own shutdown
      // returned), so the conversation goes with it rather than being left on screen looking live, as it does on
      // a fatal error. The turn clock goes too: a later snapshot naming an equal turn id would otherwise keep the
      // old `since` and `exact`, an inflated elapsed time with no "at least" mark. Whether a turn id can repeat
      // across sessions is a question about ids this page does not own, so it clears it rather than answering it.
      return {
        ...model, state: initialState(), handoffRequests: withoutHandoff(model.handoffRequests, envelope.tab),
        commandNotice: null, turnClock: null, turnDurations: NO_TURN_DURATIONS, handoff: envelope,
        // The resume offer drops exactly the session just given away, from the `hello` this page already has.
        // Rust applies the same rule whenever it builds `hello`, which is the durable half; every other stored
        // session stays offered.
        hello: model.hello === null ? null : {
          ...model.hello,
          resumableSessions: model.hello.resumableSessions.filter((session) => session.providerSessionId !== envelope.providerSessionId),
        },
      };
    case "error": {
      // Rust sends this only for the active tab; its own `tabs` entry carries the reason for when it is shown
      // again, and the window's other tabs are untouched. A tab that had shown anything (restored history,
      // prompts, a transcript, tool calls) keeps it on screen as a lost session with the failure's own reason,
      // whatever the failure; only a tab that never showed anything falls back to the empty tab's failure row.
      // A kept conversation says why in its own lost-session row, so the banner would say it twice. No new
      // `hello` is asked for: Rust re-sends one whenever the set of open provider sessions changes, and this is
      // such a change.
      const kept = keepAfterFailure(model.state, failureEvidence(envelope.message));
      const keptStates = new Map(model.keptStates);
      if (kept !== null) keptStates.set(envelope.tab, kept);
      return {
        ...model, keptStates, state: kept ?? initialState(),
        handoffRequests: withoutHandoff(model.handoffRequests, envelope.tab), turnClock: null,
        turnDurations: kept === null ? NO_TURN_DURATIONS : model.turnDurations,
        fatalError: kept === null ? envelope.message : null,
      };
    }
    case "queue":
      return { ...model, queue: envelope.items, queueError: envelope.error, queueNote: envelope.note ?? null, queueSteers: envelope.enterSteers ?? null };
    case "history":
      return { ...model, history: envelope.entries };
    case "rule_offers":
      return { ...model, ruleOffers: envelope.offers };
    case "editor_context":
      return { ...model, editorContext: envelope.file === null ? null : { file: envelope.file, lines: envelope.lines } };
    case "editor_link":
      return { ...model, editorLink: { state: envelope.state, text: envelope.text } };
    case "scratch":
      return { ...model, scratchEditing: envelope.editing };
    case "chooser":
      // An open rename field would fight the chooser for the keys.
      return { ...model, chooser: { open: envelope.open, records: envelope.records }, renaming: null };
    case "chooser_titles":
      // Only the titles of the rows already drawn change: same rows, same order, so the cursor stays on its row,
      // and a chooser the user closed is not opened again.
      return model.chooser === null ? model : { ...model, chooser: withChooserTitles(model.chooser, envelope.titles) };
    case "review":
      // A review that was worked out is answered by this envelope alone, never by a `command_result`, so its
      // record ends here. `receiveReview` drops any reply but the newest request's, and an overlay closed
      // meanwhile takes nothing. Opened on that turn, the band's pointer to it has done its job.
      return {
        ...model, inFlight: withoutRequest(model, envelope.requestId),
        review: model.review === null ? null : receiveReview(model.review, envelope),
        reviewHints: model.reviewHints[envelope.tab] !== undefined && envelope.current >= model.reviewHints[envelope.tab].turn
          ? withoutHint(model.reviewHints, envelope.tab) : model.reviewHints,
      };
    case "review_diff":
      return { ...model, inFlight: withoutRequest(model, envelope.requestId), review: model.review === null ? null : receiveDiff(model.review, envelope) };
    case "review_draft":
      // The answer to a comment add or remove, or the draft pushed after a change the page did not ask for (no
      // request id). A closed overlay keeps nothing: the next `review` carries it.
      return { ...model, inFlight: withoutRequest(model, envelope.requestId), review: model.review === null ? null : receiveDraft(model.review, envelope) };
    case "review_send_preview":
      return { ...model, inFlight: withoutRequest(model, envelope.requestId), review: model.review === null ? null : receivePreview(model.review, envelope) };
    case "review_recovery":
      return { ...model, reviewRecovery: envelope.entries, review: model.review === null ? null : receiveRecovery(model.review, envelope.entries) };
    case "review_hint":
      // Kept for the tab it names, shown only while that tab is on screen. One that arrives while a turn runs in
      // the tab on screen is about the turn before it, which the new turn has already replaced.
      return {
        ...model,
        reviewHints: envelope.files <= 0 || (envelope.tab === model.tabs?.active && model.state.activeTurnId !== null)
          ? withoutHint(model.reviewHints, envelope.tab)
          : { ...model.reviewHints, [envelope.tab]: { turn: envelope.turn, files: envelope.files } },
      };
    default:
      return model;
  }
}
