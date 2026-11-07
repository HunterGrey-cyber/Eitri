import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { NavKeyDirection } from "../bridge";
import type { EditorLink, HandoffCommand, Hello, QueueItem, TabInfo } from "../types";
import { EMPTY_PANEL_TABLE } from "../keymap";
import type { PanelBinding, PanelMode, PanelTable } from "../keymap";
import { advanceSequence, boxEntries, isPendingFirst, pendingPairBinding, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "../leader";
import type { BoxEntry, SeqStep } from "../leader";
import { isActivatableControl, webUrl } from "../nav";
import { isImeKey } from "../composerKeys";
import { Composer } from "./Composer";
import type { SlashAction } from "../slashCommands";
import { completionSource } from "../slashCommands";
import type { RestoredDraft } from "./Composer";
import { HandoffCommandCard } from "./TerminalHandoff";
import { QueueLines } from "./QueueLines";
import { Row } from "./Row";
import { DashboardHead, DashboardItems, dashItems } from "./Dashboard";
import type { DashItem } from "./Dashboard";
import { WhichKeyBox } from "./WhichKeyBox";
import { classify, failureEvidence, remedyNamesR } from "../problems";
import { agentTitle } from "../externalTools";
import { copyToPrimary } from "../primaryCopy";
import { isExternalAgent } from "../tabs";
import { hintTypingFlash, isModifierKey, leaderTypingFlash, tableKeyTypingFlash, TYPE_HINT_FLASH, TypingGuard } from "../typingGuard";

/** Below this width the dashboard's where-line cuts the cwd and drops the
 *  account. Measured the same way `StatusBand` measures its own box: a `ResizeObserver` on this
 *  screen's own root, falling back to "not narrow" at the `widthPx <= 0` a real host shows for one
 *  frame before its first measurement lands (and the only thing jsdom, which lays nothing out,
 *  ever reports). */
const DASH_NARROW_PX = 360;

/** What `App.tsx` bumps on each `nav_key` it has not already answered with
 *  `nav_fallthrough` itself (an overlay owning the keys, a y/n, `?`) -- `seq` for the same reason
 *  `RestoredDraft`/`QueueTaken` carry one: the same direction twice in a row must still re-fire the
 *  effect below, which a bare boolean or a repeated string would coalesce away. */
export type NavKeyRequest = { seq: number; direction: NavKeyDirection };

/** The dashboard's cursor indices in the order the screen draws them, top to bottom: the box (index 0, the
 *  New session item) sits at the foot under the other rows, so `j` walks the rows, reaches it, and then enters it. */
export function screenOrder(count: number): number[] {
  return [...Array.from({ length: count }, (_, i) => i).slice(1), 0];
}

export type EmptyTabProps = {
  hello: Hello | null;
  tab: TabInfo;
  handoff: HandoffCommand | null;
  failure: string | null;
  paneFocused: boolean;
  /** The key that brings the keyboard into this panel (`panelEntryKey`), for the box's hint while the panel lacks
   *  it. `null`/absent -- a companion window, or the key not known yet -- draws no hint. */
  entryKey?: string | null;
  /** `:` in BROWSE: opens the window's command line (`:tabnew codex` from a fresh window). */
  onExLine?: () => void;
  focusRequest: number;
  /** Bumped by App's `arrive` handling: lands BROWSE with the
   *  cursor on the dashboard's `New session` item, the same reversal a live tab's arrival gets. */
  arriveRequest: number;
  /** Bumped by `App`'s own `[keysRequest]` effect when this is the empty layout -- a `pane_focus`
   *  round trip, or an overlay closing, must never leave the keys on `document.body`. Landed on the composer (INPUT)
   *  or this screen's own root (BROWSE), whichever currently has the live control. */
  keysRequest: number;
  /** Window cancel requests drop this screen's pending prefix and leader sequence too,
   *  so `g`, Shift+Tab, `i` cannot swallow the INPUT request as a stale continuation.
   *  Optional for callers with no window. */
  dropKeysRequest?: number;
  /** True while the `prefix w` chooser is drawn over this screen. Guards
   *  `focusRequest`/`arriveRequest`/`keysRequest` from stealing the keys out from under it -- the
   *  same `overlayOpen` `App.tsx` computes, threaded through since this screen has no
   *  `containerRef` of its own for `App` to check. */
  overlayOpen: boolean;
  /** A claimed `Ctrl+j`/`Ctrl+k` `App.tsx` has already checked against its OWN
   *  overlays (`keymapOpen`, a y/n `confirm`) -- `null`/absent, or a `seq` already seen, is not a
   *  request. This screen decides the rest itself: a `starting`/`failed` tab, or its own menu
   *  (`mode === "browse"`) vs. composer (`mode === "input"`) not matching the direction, both answer
   *  through `onNavFallthrough` rather than transitioning. */
  navKeyRequest?: NavKeyRequest | null;
  /** See `navKeyRequest`. Optional, like `onChooseSessions`, so this component's own tests need no
   *  stand-in. */
  onNavFallthrough?: (direction: NavKeyDirection) => void;
  /** Where this screen starts on mount or a switch to another empty tab: `"browse"` after a switch
   *  or arrival, `"input"` after `enter_input` and at launch. The window's `focusRequest` and
   *  `arriveRequest` counters outlive a tab: only a later change is a request to this screen.
   *  Defaults to `"input"`, the launch state. */
  landing?: PanelMode;
  /** This screen's mode, so the outer band agrees with where the keys land. */
  onModeChange?: (mode: PanelMode) => void;
  restoredDraft: RestoredDraft | null;
  onSend: (text: string) => void;
  /** The box's `!` line and `@` menu, as the conversation's composer has them (`Composer`). */
  onRunShell?: (text: string) => void;
  atPaths?: { items: string[]; seq: number } | null;
  onAtQuery?: (query: string, seq: number) => void;
  onSlashAction?: (action: SlashAction) => void;
  onResume: (providerSessionId: string) => void;
  /** The dashboard's `s` item / key: bring back the tabs the last window had open. Present with the
   *  item (`hello.restore`); optional so this component's own tests need no stand-in. */
  onRestoreLast?: () => void;
  onCycleMode: () => void;
  onReset: () => void;
  onHint: (repeat: boolean) => void;
  /** The dashboard's `w` item / key ("All sessions"): the same window-level `tab_verb
   *  choose` a live tab's `prefix w` posts (`App.tsx`'s `runPanelAction`, `tab.choose`). Optional,
   *  like `onOpenKeymap` below, so this component's own tests need no stand-in. */
  onChooseSessions?: () => void;
  onChooseProject?: () => void;
  /** Mirrors `Composer`'s own prop of the same name: this tab's
   *  draft is saved and restored across a switch like any other, so an empty tab's composer needs
   *  the same hook into it. */
  onDraftChange?: (text: string) => void;
  /** The window-close prompt, lifted out of `App.tsx` so both layouts' `onKeyDown`
   *  agree: called first, and if it returns `true` this component's own key handling stops there.
   *  Optional so a caller with no window (this component's own tests) needs no stand-in. */
  answerConfirm?: (event: KeyboardEvent<HTMLDivElement>) => boolean;
  /** This tab's own queue and prompt-history plumbing, threaded through to `Composer` --
   *  see that component's own props of the same names. `running` is this tab's own `starting`, not
   *  a turn: a `NotStarted`/`starting` tab has no turn yet, but starting is still not "idle" (the
   *  box queues behind the connect rather than lazily starting a second session). No `onInterrupt`
   *  here -- there is no live turn to interrupt while merely connecting, so `Composer`'s Ctrl+c
   *  branch for a running box is inert (the draft stays put, which is harmless).
   *
   *  `Composer` DOES need something wired to `onSendNow`, though:
   *  its `submit()` clears the box on the `now` branch unconditionally, whether or not `onSendNow`
   *  did anything, so leaving it as the default no-op silently threw away whatever the user typed
   *  on Ctrl+Enter. A starting tab has no turn to send to "now" either, so it falls back to the same
   *  effect as `onQueue` below, guarded against an empty box (`queue_message` has no such guard of
   *  its own and would happily queue a blank entry). */
  onQueue?: (text: string) => void;
  history?: string[];
  queueCount?: number;
  /** Queue lines above the composer: a starting tab can already have messages queued behind its connect.
   *  Editor context is built directly into the outer band by `App.tsx`. */
  queue?: QueueItem[];
  queueError?: string | null;
  /** Why its messages are held (the queue envelope's `note`). */
  queueNote?: string | null;
  onTakeBackQueue?: () => void;
  queueTaken?: { texts: string[]; seq: number } | null;
  onHistoryPush?: (text: string) => void;
  onEditInNvim?: (text: string) => void;
  editingInNvim?: boolean;
  onOpenKeymap?: () => void;
  /** The panel's leader/which-key table (`keymapHelp.panel` in `App.tsx`, itself `keymap` envelope's
   *  `panel`), and where a completed sequence's binding goes. The dashboard's own root once never ran
   *  the leader engine that `App.tsx`'s `onKeyDown` already had for the live conversation, so Space
   *  (which starts a leader sequence) did nothing here. Both optional, defaulting to `EMPTY_PANEL_TABLE` (no
   *  bindings) and a no-op, so this component's own tests need no stand-in for either -- the same reason
   *  `onChooseSessions` above is optional. `onPanelAction` is `App.tsx`'s own `runPanelAction`, reused rather than
   *  duplicated: its `mode.cycle` case already branches on whether a session has started (posting `cycle_mode`
   *  directly here, since this tab has none), and its `tab.*` cases post the same window-level `tab_verb` regardless
   *  of which tab held the keys. */
  panelTable?: PanelTable;
  onPanelAction?: (binding: PanelBinding) => void;
  /** Companion mode (`App.tsx`'s `editorLink`): passed through to `Dashboard`, whose words change when
   *  the editor is another window. `null` (the default) is the one-window mode. */
  editorLink?: EditorLink | null;
  /** The panel's one typing guard (`App.tsx`'s own), so the leader
   *  here starts a sequence only on a key that stands alone or ends a quick motion, as it does over
   *  a live conversation -- "set up my" typed onto this dashboard ran `<leader>m` and flipped the
   *  stored mode to bypass, the thing the removal of the bare `m` binding was meant to stop. Optional: without one this
   *  screen keeps its own, so its tests need no stand-in. */
  typingGuard?: TypingGuard;
  /** Says a refused leader in the band (`App.tsx`'s `showFlash`); optional for the same reason. */
  onFlash?: (text: string) => void;
  /** `gx` on a failed tab's row: hands the address to Rust's own web-link path (`App.tsx`'s `openUrl`, which
   *  Rust re-checks), the one `gx` takes over a live conversation. Optional for the same reason. */
  onOpenUrl?: (url: string) => void;
};

/** An empty session tab: Claude Code's fresh prompt. The composer is live in INPUT;
 *  the first send creates the session in Rust, so an untouched tab costs no process. `Shift+Tab`
 *  cycles the mode (entering bypass asks first). Nothing here
 *  spawns a process. */
export function EmptyTab(props: EmptyTabProps) {
  const { hello, tab, handoff, failure } = props;
  const [mode, setMode] = useState<PanelMode>(props.landing === "browse" ? "browse" : "input");
  /** What a deferred `f`/single-key table binding below
   *  reads at fire time, mirroring `App.tsx`'s own `modeRef` -- a plain `mode`/`failed` read inside
   *  `typingGuard.defer`'s callback would close over the render that scheduled it, not the one at
   *  the moment it actually runs `TYPING_GUARD_MS` later. */
  const modeRef = useRef(mode);
  modeRef.current = mode;
  // The counts this screen mounted with (see `landing`'s doc): the two request effects below act
  // only on a count that changed after that.
  const focusAtMount = useRef(props.focusRequest);
  const arriveAtMount = useRef(props.arriveRequest);
  /** A start put off until the trust question is answered. Everything that treats a starting tab as busy
   *  (the composer queues behind it, no dashboard, no focus request) applies to it too, so `starting` stays
   *  true for it; only what the screen SAYS differs. */
  const awaitingTrust = tab.state === "awaiting_trust";
  const starting = tab.state === "starting" || awaitingTrust;
  const failed = tab.state === "failed";
  /** See `modeRef`: `f`/`H`/`L` must still fire on a `failed` tab even though `mode` reads "input"
   *  there (`onKeyDown`'s own "Once the composer is disabled (failed) there is nothing left to type
   *  into" reasoning, just below -- there is no live composer to disturb once the tab has failed). */
  const failedRef = useRef(failed);
  failedRef.current = failed;
  /** See `modeRef`: a deferred `r`/`w` runs only while this is still the dashboard, which a
   *  `starting` tab does not draw. */
  const startingRef = useRef(starting);
  startingRef.current = starting;
  // `null` for a failure this build's classifier does not recognise (or none),
  // in which case the row below draws exactly what it always did -- the raw text, and nothing more.
  const failureProblem = failed && failure !== null ? classify(failure, hello?.account ?? null) : null;
  const showDashboard = hello !== null && !starting && !failed;
  const editorLink = props.editorLink ?? null;
  const [dashCursor, setDashCursor] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  /** "The first start on a fresh Verdandi checkout also builds the sidecar." used
   *  to be shown unconditionally on every starting screen, which is irrelevant on an installed
   *  build where nothing gets built at all. It now waits 10s, per THIS tab's own starting spell
   *  (`tab.id` in the deps below, so a switch to a different starting tab restarts the wait rather
   *  than inheriting the old tab's elapsed time), and reads differently once it does. No wire
   *  change: `tab.state` already said "starting", this only delays and rewords one sentence. */
  const [stillStarting, setStillStarting] = useState(false);
  useEffect(() => {
    if (!starting || awaitingTrust) {
      setStillStarting(false);
      return;
    }
    const timer = setTimeout(() => setStillStarting(true), 10_000);
    return () => clearTimeout(timer);
  }, [starting, awaitingTrust, tab.id]);
  /** What `Composer` actually reads, mirroring `App.tsx`'s own `composerFocusRequest`
   *  -- bumped only from the guarded `focusRequest`/`keysRequest` effects below, so an overlay drawn
   *  over this screen can never have its composer autofocused out from under it. */
  const [composerFocus, setComposerFocus] = useState(0);
  /** Mirrors `App.tsx`'s own `composerCaret` -- `A` forces
   *  the caret to the end of the draft, every other route into INPUT (`i`/`o`, `Ctrl+j`, a landing)
   *  explicitly resets it to "kept" rather than leaving whatever an earlier `A` last set (the same
   *  bug `App.tsx`'s own doc comment on this state records). */
  const [composerCaret, setComposerCaret] = useState<"kept" | "end">("kept");
  const [widthPx, setWidthPx] = useState(0);
  /* The dashboard's own narrow breakpoint (360 px), measured the same way
     `StatusBand` measures its own box: jsdom lays nothing out, so `widthPx` stays 0 and `narrow`
     stays `false`, the same floor a real host shows for one frame before its first measurement
     lands. */
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (el !== null) setWidthPx(el.clientWidth);
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) if (entry.target === el) setWidthPx(entry.contentRect.width);
    });
    if (el !== null) observer.observe(el);
    return () => observer.disconnect();
  }, []);
  const narrow = widthPx > 0 && widthPx <= DASH_NARROW_PX;
  const panelTable = props.panelTable ?? EMPTY_PANEL_TABLE;
  const [ownGuard] = useState(() => new TypingGuard());
  const typingGuard = props.typingGuard ?? ownGuard;
  /* The leader/which-key engine: a scoped-down copy of
     `App.tsx`'s own `seqRef`/`seq`/`boxShown`/`applySeqStep`/`clearSequence`, kept local to this
     screen rather than lifted to `App.tsx` -- this tab already keeps its own `mode` locally (BROWSE
     here means the dashboard has the keys, not the live conversation), and `App.tsx`'s copy cannot
     see it. The reserved `g`/`z`/`[`/`]` prefixes get only their table half here
     (`pendingPrefixRef` below): `[b`/`]b` step tabs from the dashboard as from a live tab, while
     `resolveKey`'s own fixed pairs (`gg`, `[[`, ...) have no rows to act on on this screen and stay
     unimplemented -- so here every key but a table pair cancels a waiting prefix, as in a live tab. */
  const pendingPrefixRef = useRef<string | null>(null);
  const seqRef = useRef<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  const [seq, setSeq] = useState<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  const boxShownRef = useRef(false);
  const [boxShown, setBoxShown] = useState(false);
  const boxTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const seqTimeoutTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function cancelBoxTimer() {
    if (boxTimerRef.current !== null) clearTimeout(boxTimerRef.current);
    boxTimerRef.current = null;
  }
  function cancelSeqTimeoutTimer() {
    if (seqTimeoutTimerRef.current !== null) clearTimeout(seqTimeoutTimerRef.current);
    seqTimeoutTimerRef.current = null;
  }
  function showBox() {
    boxShownRef.current = true;
    setBoxShown(true);
  }
  function hideBox() {
    boxShownRef.current = false;
    setBoxShown(false);
  }
  function scheduleBoxTimer() {
    if (boxShownRef.current) return;
    cancelBoxTimer();
    boxTimerRef.current = setTimeout(() => {
      boxTimerRef.current = null;
      showBox();
    }, WHICH_KEY_DELAY_MS);
  }
  function clearSequence() {
    pendingPrefixRef.current = null;
    seqRef.current = null;
    setSeq(null);
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
    hideBox();
  }
  function applySeqStep(step: SeqStep) {
    if (step.kind === "run") {
      clearSequence();
      props.onPanelAction?.(step.binding);
      return;
    }
    if (step.kind === "cancel") {
      clearSequence();
      return;
    }
    if (step.kind === "none") return;
    seqRef.current = { typed: step.typed, ambiguous: step.ambiguous };
    setSeq(seqRef.current);
    cancelSeqTimeoutTimer();
    scheduleBoxTimer();
    if (step.ambiguous !== null && panelTable.timeout) {
      const ambiguous = step.ambiguous;
      seqTimeoutTimerRef.current = setTimeout(() => {
        seqTimeoutTimerRef.current = null;
        applySeqStep({ kind: "run", binding: ambiguous });
      }, panelTable.timeoutlen);
    }
  }
  // The same rule as `App.tsx`'s, for this local copy: a table that changes mid-sequence must never
  // let the OLD table's binding run against the new one.
  useEffect(() => {
    clearSequence();
    // This screen's own copy of the same rule: `clearSequence` drops only a pending MULTI-key
    // sequence -- a single-key table binding deferred by `typingGuard.defer` (H's default tab.prev) closes over the OLD table's `binding` at keydown time and
    // stayed armed to run it against whatever the table now says even after being replaced.
    typingGuard.cancel();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelTable]);
  // Leaving BROWSE by any route other than a key this engine itself consumed (a click into the
  // composer, `focusRequest`), or the dashboard itself disappearing out from under a pending
  // sequence (the tab starting to fail mid-sequence), must not leave a stale sequence or box armed
  // for a screen that no longer shows either.
  useEffect(() => {
    if (mode !== "browse" || starting || failed || !props.paneFocused) clearSequence();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, starting, failed, props.paneFocused]);
  // The window's own cancel routes (`dropKeysRequest`'s doc). Also run on mount, where there is
  // nothing to drop yet -- as the `[panelTable]` effect above.
  useEffect(() => {
    clearSequence();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.dropKeysRequest]);
  useEffect(() => {
    return () => {
      cancelBoxTimer();
      cancelSeqTimeoutTimer();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const box: { title: string; entries: BoxEntry[] } | null =
    seq !== null ? { title: sequenceTitle(panelTable, seq.typed), entries: boxEntries(panelTable, seq.typed, starting || failed) } : null;
  /* A request for the keys must enter INPUT as well as focus it: blur can leave BROWSE with no
     textarea, and focusing an ancestor handles no typing. Refuse where `i` is refused, and while
     an overlay is open, so autoFocus cannot steal the chooser's keys. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.focusRequest !== focusAtMount.current && !starting && !failed) {
      setMode("input");
      setComposerCaret("kept");
      setComposerFocus((n) => n + 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.focusRequest]);
  /* An `arrive` that lands here -- a launch that starts with the
     keys already in the chat -- ends INPUT the way it ends a live tab's, rather than leaving the
     fresh prompt's default composer focused. No `!starting && !failed` guard: unlike `focusRequest`,
     landing BROWSE never needs a box to type into. The cursor resets to the dashboard's first item
     (`"new"`, the New session item). Focusing the root happens BEFORE
     `setMode("browse")` -- that synchronously blurs the textarea (its own `onBlur` then sees
     `relatedTarget` already the root, not `.history-search`, and reports browse itself too), so the
     unmount a moment later never drops the keys onto `document.body`. Also guarded by
     `overlayOpen`: the launch chooser's own `Esc` sends this when Rust could not hand the keys to
     the editor, but a `chooser`/`begin_rename`/`/` still open over this screen keeps them instead. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.arriveRequest !== arriveAtMount.current) {
      rootRef.current?.focus({ preventScroll: true });
      setMode("browse");
      setDashCursor(0);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.arriveRequest]);
  /* A tab that fails while its composer has the keys disables the focused
     textarea, and WebKit's focus fix-up drops the keys on <body> -- a failed tab's `r` then depended
     on the document replay alone. The root takes the keys first, then BROWSE (the same ordering as
     `Esc` and `arrive` above). Not a copy of the live layout's `[sessionEnded]` effect, which only ends
     VISUAL and sets BROWSE and takes no focus: there the replay's target, the conversation's own root,
     handles the keys itself, where this layout's replay needed this effect to find `.empty-tab`. Only when the keys
     are in this screen's dead composer or nowhere in it (a failure while the panel does not have the
     keys focuses DOM-wise only; the next `arrive` lands here anyway), and never under an overlay. */
  const wasFailed = useRef(failed);
  useEffect(() => {
    const turnedFailed = failed && !wasFailed.current;
    wasFailed.current = failed;
    if (!turnedFailed || props.overlayOpen) return;
    const root = rootRef.current;
    if (root === null) return;
    const active = document.activeElement;
    const deadControl = active instanceof HTMLTextAreaElement && active.disabled;
    if (root.contains(active) && !deadControl) return;
    root.focus({ preventScroll: true });
    setMode("browse");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [failed]);
  /** `App`'s own `[keysRequest]` effect bumps this directly (there is no
   *  `containerRef` on this layout for it to check). A no-op when the root already contains the
   *  active element -- WebKitGTK's DOM focus across a GTK round trip may well have survived, and a
   *  `root.contains` skips reaching for anything in that case, the same guard `takeKeys` itself
   *  applies to the live conversation. */
  useEffect(() => {
    if (props.keysRequest === 0 || props.overlayOpen) return;
    const root = rootRef.current;
    if (root === null || root.contains(document.activeElement)) return;
    if (mode === "input" && !starting && !failed) setComposerFocus((n) => n + 1);
    else root.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.keysRequest]);
  /** The menu's own `Ctrl+j`/the composer's own `Ctrl+k`, echoed back once
   *  Rust's mirror has claimed one. Initialized from whatever `props.navKeyRequest` already was
   *  (the `focusAtMount`/`arriveAtMount` pattern, "requests are edges, not levels" above): a count
   *  reached before this screen mounted -- another tab's own stale request, still sitting in
   *  `App.tsx`'s state -- is not a request to this one either. `starting`/`failed` (no box to enter)
   *  and a `mode` that no longer matches the direction (stale) both report back
   *  through `onNavFallthrough` rather than transitioning, except an upward request out of a starting
   *  tab's open INPUT, which leaves it for BROWSE as `Esc` does (below). */
  const navKeySeenRef = useRef<number | null>(props.navKeyRequest?.seq ?? null);
  useEffect(() => {
    const req = props.navKeyRequest;
    if (req === null || req === undefined || req.seq === navKeySeenRef.current) return;
    navKeySeenRef.current = req.seq;
    if (props.overlayOpen) {
      props.onNavFallthrough?.(req.direction);
      return;
    }
    // A starting tab's composer is live ("it queues behind the
    // connect", this file's own `onKeyDown` comment above), so leaving an already-open INPUT must
    // behave the same way `Esc` does there -- not fall through to Rust's `move_focus` and hand the
    // keys off this screen entirely. Checked ahead of the `starting || failed` fallthrough below,
    // which still owns every other case (entering INPUT while starting/failed at all, and `failed`'s
    // own dead INPUT, never exempted here either, matching `Esc`'s own `!failed` guard). The root
    // takes focus first, as `Esc`'s own branch does: `setMode("browse")` unmounts the textarea, and
    // focusing afterwards would drop the keys onto `document.body`.
    if (req.direction === "up" && mode === "input" && !failed) {
      rootRef.current?.focus({ preventScroll: true });
      setMode("browse");
      return;
    }
    if (starting || failed) {
      props.onNavFallthrough?.(req.direction);
      return;
    }
    if (req.direction === "down" && mode === "browse") {
      setMode("input");
      setComposerCaret("kept");
      return;
    }
    props.onNavFallthrough?.(req.direction);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.navKeyRequest]);
  /* Where the keys go when this screen is landed: the two request effects above now ignore a count reached before
     mount, so a mount or a switch has to put the keys somewhere itself -- otherwise a new tab made
     from a live one (whose container just unmounted) lands with nothing focused. BROWSE takes the
     root; INPUT takes the composer, but only when a request for it exists (`focusRequest > 0`, the
     batch that made this tab), so a plain launch leaves DOM focus alone as it always has. Never
     under an overlay (the chooser, a rename, `/` hold the keys there). */
  function land(where: PanelMode, onMount: boolean) {
    if (props.overlayOpen) return;
    const root = rootRef.current;
    // On mount the keys usually sit on the window's layout root (React keeps that `div` across the
    // live/empty layouts) or on `document.body`; neither hands a key to this screen.
    if (onMount && root !== null && root.contains(document.activeElement)) return;
    if (where === "browse") root?.focus({ preventScroll: true });
    else if (props.focusRequest > 0 && !starting && !failed) setComposerFocus((n) => n + 1);
  }
  useEffect(() => {
    land(props.landing === "browse" ? "browse" : "input", true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /* A switch from one empty tab to another keeps this component mounted: land it the way a mount
     would (see `landing`). Skips the first run, which the initial state already covers. */
  const tabAtMount = useRef(tab.id);
  useEffect(() => {
    if (tab.id === tabAtMount.current) return;
    tabAtMount.current = tab.id;
    const where = props.landing === "browse" ? "browse" : "input";
    // The root first, as `arrive` does: `setMode("browse")` unmounts the focused textarea.
    land(where, false);
    setMode(where);
    setComposerCaret("kept");
    setDashCursor(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.id]);
  useEffect(() => {
    props.onModeChange?.(mode);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);

  /** What each dashboard item does -- the one place both a click (`Dashboard`'s
   *  own `onItem`) and a keyboard letter/`Enter` (`onKeyDown` below) end up. */
  function runItem(item: DashItem) {
    switch (item) {
      case "new":
        setMode("input");
        // `i`/`o`, a click and Enter keep the caret; only the `A` branch forces it to the end.
        setComposerCaret("kept");
        break;
      case "restore":
        props.onRestoreLast?.();
        break;
      case "resume": {
        const newest = hello?.resumableSessions[0];
        if (newest !== undefined) props.onResume(newest.providerSessionId);
        break;
      }
      case "project":
        props.onChooseProject?.();
        break;
      case "sessions":
        props.onChooseSessions?.();
        break;
      case "mode":
        props.onCycleMode();
        break;
      case "keys":
        props.onOpenKeymap?.();
        break;
    }
  }

  /** `gx` on this screen: the web link a failure's row carries (the install address of a not-found `claude`). The
   *  one the keys are on when they are on a link, else the row's first. The row's own links are the page's, not a
   *  reply's, so the address is sent without a pick. */
  function openFailureLink(target: EventTarget | null) {
    const anchors = Array.from(rootRef.current?.querySelectorAll<HTMLAnchorElement>(".row-problem a[href]") ?? []);
    const focused = anchors.find((anchor) => anchor === target || anchor.contains(target as Node | null));
    const url = webUrl((focused ?? anchors[0])?.getAttribute("href") ?? null);
    if (url === null) props.onFlash?.("no web link on this screen");
    else props.onOpenUrl?.(url);
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (props.answerConfirm?.(event)) return;
    // An overlay drawn over this screen (the chooser, a rename) owns the keys; one that
    // reaches this root anyway (replayed off <body>) must not act on the tab behind it.
    if (props.overlayOpen) return;
    // Every other keydown is "a key" to the typing guard, first, as `App.tsx`'s own `onKeyDown` does:
    // the leader below asks it whether it stood alone.
    const typedAt = typingGuard.timeOf(event);
    // Cancelling a waiting `f`, `H`/`L`, `r` or `w` must show that key's typing-guard reason.
    const cancelled = typingGuard.onKey(event.key, typedAt);
    if (cancelled !== null) props.onFlash?.(cancelled);
    // Every real key reads and drops a reserved prefix first; an input method's key and an
    // INPUT key drop it too.
    if (pendingPrefixRef.current !== null && isModifierKey(event.key)) return;
    const prefix = pendingPrefixRef.current;
    pendingPrefixRef.current = null;
    // An IME key is never a key of this screen's, the leader engine's below included: the same
    // `isImeKey` test App.tsx's own leader engine and the composer use, so WebKit's `keyCode` 229
    // (a key the input method consumed, reported without `isComposing`) is caught here as well.
    if (isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) return;
    // `modeKey.ts` claims Shift+Tab in document capture before this handler.
    // A failed tab has no live input: its stale "input" mode must not swallow `r`, HINT or copying.
    // A starting tab still has a live composer that queues behind its connect.
    if (mode === "input" && !failed) {
      // vim's Esc (and the live conversation's, `resolveKey`): back to the menu. The root takes the keys
      // BEFORE `setMode("browse")` unmounts the textarea, or they would fall to <body>.
      if (event.key === "Escape" && !event.nativeEvent.isComposing) {
        event.preventDefault();
        rootRef.current?.focus({ preventScroll: true });
        setMode("browse");
      }
      return;
    }
    // A reserved prefix typed a key ago (`[`) waits with no timeout; this key
    // either completes one of its table pairs (`[b` -> tab.prev, `gT` through Shift, never through
    // Ctrl/Alt: no table key is a chord) or cancels, swallowed, as vim ends an unfinished `g` --
    // never read as an ordinary key, so neither `f`/`r`/`y`/`i`, the leader, nor a focused
    // control's own Enter/Space (the line just below) runs past it. `Ctrl+c` alone still falls
    // through, to the page's own native copy, as it does over a live tab.
    if (prefix !== null) {
      if (prefix === "g" && event.key === "x" && !event.ctrlKey && !event.altKey && !event.metaKey) {
        event.preventDefault();
        openFailureLink(event.target);
        return;
      }
      const pair = event.ctrlKey || event.altKey ? null : pendingPairBinding(panelTable, prefix, event.key);
      if (pair !== null) {
        event.preventDefault();
        props.onPanelAction?.(pair);
        return;
      }
      if (!(event.ctrlKey && event.key === "c")) {
        event.preventDefault();
        return;
      }
    }
    // Any key within `TYPING_GUARD_MS` of a BROWSE `y` (below) is typing, as
    // over a live conversation (`App.tsx`): swallowed, and the band says how to type. A bare modifier
    // is not a key. Ahead of the focused-control line below, so
    // `y` then Enter on a focused dashboard control is claimed too; a lone Enter after a pause is not.
    if (!isModifierKey(event.key) && typingGuard.afterCopy(typedAt)) {
      event.preventDefault();
      if (cancelled === null) props.onFlash?.(TYPE_HINT_FLASH);
      return;
    }
    // A pending sequence owns continuations before the single-key handlers (`f`, `r`, `y`) can.
    // Otherwise HINT steals the `f` of `<leader>fn`. Keep focused controls' Enter/Space ahead
    // of the sequence engine too, so it cannot swallow their native activation.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ")) return;
    if (seqRef.current !== null) {
      // Mirrors `App.tsx`'s own guard just below -- a bare
      // modifier's own keydown is not "the next key" and must not cancel a pending sequence here
      // either, the same reasoning as `pendingPrefixRef`'s guard a few lines down.
      if (isModifierKey(event.key)) return;
      event.preventDefault();
      // Mirrors `App.tsx`'s own rule for the same chord
      // -- a real Ctrl/Alt chord is unbound here by definition (no table key carries a modifier), so
      // it must not complete a binding `advanceSequence` would otherwise match on `event.key` alone.
      applySeqStep(
        event.ctrlKey || event.altKey ? { kind: "cancel" } : advanceSequence(panelTable, seqRef.current.typed, event.key),
      );
      return;
    }
    if (event.key === "r" && failed) {
      event.preventDefault();
      props.onReset();
      return;
    }
    if (event.key === ":" && !event.ctrlKey && !event.altKey && !event.metaKey && props.onExLine !== undefined) {
      // The live conversation opens its command line on a bare `:` in BROWSE, with no typing-guard wait.
      event.preventDefault();
      props.onExLine();
      return;
    }
    if (event.key === "f") {
      event.preventDefault();
      // Defer HINT by `TYPING_GUARD_MS`, so "fix the dashboard layout⏎" does not act on its `f`.
      // Fire only if nothing else was typed and this screen still allows HINT (`modeRef`/`failedRef`).
      const waiting = typingGuard.defer(
        typedAt,
        event.repeat,
        () => {
          if (modeRef.current !== "input" || failedRef.current) props.onHint(false);
        },
        hintTypingFlash(),
      );
      if (!waiting && !event.repeat) props.onFlash?.(hintTypingFlash());
      return;
    }
    if (event.key === "y" && !event.ctrlKey && !event.altKey && !event.metaKey) {
      // Every BROWSE `y` starts a typing burst even when there is no command to copy: `yi`/`ys`
      // are operator continuations, so Enter and `j` immediately after `y` must not act on the dashboard.
      typingGuard.noteCopy(typedAt);
      if (handoff !== null) {
        event.preventDefault();
        void navigator.clipboard?.writeText(handoff.command);
        copyToPrimary(handoff.command);
        return;
      }
    }
    if (hello === null) return;
    // The leader/which-key engine runs on a `starting` or `failed` tab too: `<leader>` `mode.cycle`
    // goes through `modeKeyRoute` beside Shift+Tab on those tabs (`starting` always cycles, same as `live`; `failed`
    // flashes `modeFixedMessage` unless it is already in bypass), and returning here first left it doing nothing at
    // all there. Only the dashboard's own menu keys below stay a `not_started` tab's. The leader/which-key engine
    // (Space starts a leader sequence): tried first, mirroring `App.tsx`'s own `onKeyDown` ordering, so a table
    // binding on any key not already claimed above (Space itself, or `H`/`L`/... once the user's table has them)
    // takes it before the dashboard's own fixed `j`/`k`/`Enter`/letters below ever see it -- the reserved-key list
    // is what keeps those from ever colliding with a real table entry. A reserved prefix
    // (`g`/`z`/`[`/`]`) is armed here, and read at the top of this function, where its next key completes a table
    // pair or cancels -- as `resolveKey` does for a live tab. A bare modifier's keydown (the Shift of `gT`) waits
    // with it, as vim does.
    if (!event.ctrlKey && !event.altKey && !event.shiftKey && isPendingFirst(event.key)) {
      event.preventDefault();
      pendingPrefixRef.current = event.key;
      return;
    }
    if (!event.ctrlKey && !event.altKey) {
      const start = startSequence(panelTable, event.key, isActivatableControl(event.target));
      if (start.kind !== "none") {
        event.preventDefault();
        // In the middle of typed prose the leader is swallowed and
        // says so, as over a live conversation (`TypingGuard.mayActAfterMotion`).
        if (event.key === panelTable.leader && !typingGuard.mayActAfterMotion(typedAt, event.repeat)) {
          props.onFlash?.(leaderTypingFlash(panelTable.leaderLabel));
          return;
        }
        // A single-key table binding (`H`/`L`, or a configured letter) also needs deferral: a first
        // `L` in "Looks good, now add tests⏎" has no motion run for `mayActAfterMotion` to recognise.
        // Wait as `a`/`d`/`f` do, rather than act on prose.
        if (start.kind === "run") {
          const { binding } = start;
          const flash = tableKeyTypingFlash(event.key, binding.desc);
          const waiting = typingGuard.defer(
            typedAt,
            event.repeat,
            () => {
              if (modeRef.current !== "input" || failedRef.current) applySeqStep({ kind: "run", binding });
            },
            flash,
          );
          if (!waiting && !event.repeat) props.onFlash?.(flash);
          return;
        }
        applySeqStep(start);
        return;
      }
    }
    if (starting || failed) return;
    const items = dashItems(hello);
    // One column, as in snacks.nvim's dashboard: h/l never change the item, but they are still
    // claimed here so they never fall through to the composer or leak out as an unhandled key.
    if (event.key === "h" || event.key === "l") {
      event.preventDefault();
      return;
    }
    if (event.key === "j" || event.key === "k" || event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const delta = event.key === "j" || event.key === "ArrowDown" ? 1 : -1;
      const order = screenOrder(items.length);
      // The box is the last stop, so `j` on it walks in, by the conversation's own rule (`TypingGuard.walkIntoBox`).
      if (delta === 1 && dashCursor === order[order.length - 1]) {
        const walks = typingGuard.walkIntoBox(event, typedAt, () => {
          if (modeRef.current !== "input" && !failedRef.current && !startingRef.current) runItem("new");
        });
        if (!walks && !event.repeat && cancelled === null) props.onFlash?.(TYPE_HINT_FLASH);
        return;
      }
      setDashCursor((c) => {
        const at = order.indexOf(c);
        return order[Math.max(0, Math.min(order.length - 1, at + delta))];
      });
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      const item = items[dashCursor];
      if (item !== undefined) runItem(item);
      return;
    }
    // Bare `m` collides with prose such as "make" and "mode"; mode changes use Shift+Tab or
    // `<leader>m`. `o` aliases `i`; `A` forces the caret to the end. These INPUT entries require a
    // pure pause (`mayStartInput`), never a motion exception: "look at" must not walk on `l` and
    // open INPUT on `o`. In typed prose the keys are swallowed and say how to type.
    if ((event.key === "i" || event.key === "o" || event.key === "A") && !typingGuard.mayStartInput(typedAt, event.repeat)) {
      event.preventDefault();
      if (cancelled === null) props.onFlash?.(TYPE_HINT_FLASH);
      return;
    }
    if (event.key === "i" || event.key === "o") {
      event.preventDefault();
      runItem("new");
    } else if (event.key === "A") {
      event.preventDefault();
      runItem("new");
      setComposerCaret("end");
    } else if (
      (event.key === "r" && items.includes("resume")) ||
      (event.key === "s" && items.includes("restore")) ||
      event.key === "w" ||
      (event.key === "p" && items.includes("project"))
    ) {
      // Arrivals land in BROWSE: "run the tests" must not resume on `r`, nor "what's next" open
      // the chooser on `w` and feed it the rest. Defer by `TYPING_GUARD_MS` like `f` and `H`/`L`,
      // acting only if nothing else was typed and the dashboard remains in BROWSE.
      event.preventDefault();
      const item: DashItem =
        event.key === "r" ? "resume" : event.key === "s" ? "restore" : event.key === "p" ? "project" : "sessions";
      const flash = tableKeyTypingFlash(
        event.key,
        item === "resume" ? "resume the newest session"
          : item === "restore" ? "restore the last session"
          : item === "project" ? "open another project"
          : "all sessions",
      );
      const waiting = typingGuard.defer(
        typedAt,
        event.repeat,
        () => {
          if (modeRef.current !== "input" && !failedRef.current && !startingRef.current) runItem(item);
        },
        flash,
      );
      if (!waiting && !event.repeat) props.onFlash?.(flash);
    } else if (event.key === "?") {
      event.preventDefault();
      runItem("keys");
    }
  }

  // Check `hello !== null` beside the dashboard flag: TypeScript cannot narrow through that flag.
  const inDashboard = hello !== null && showDashboard;
  return (
    <div className="empty-tab" tabIndex={0} onKeyDown={onKeyDown} ref={rootRef}>
      {handoff !== null && <HandoffCommandCard handoff={handoff} />}
      {failed && (
        <Row kind="error" sign="✗" role="alert" problem={failureProblem}>
          <strong>{tab.failedAfterStart ? "This tab's session stopped." : "This tab's session did not start."}</strong>
          <pre>{failure === null ? "no reason was given" : failureEvidence(failure)}</pre>
          {/* Once per screen: left out when the remedy above already says it. */}
          {!remedyNamesR(failureProblem) && <div className="row-hint">Press r to start a new session here.</div>}
        </Row>
      )}
      {awaitingTrust && (
        <p className="connecting">Waiting for your answer to the trust question — nothing has started</p>
      )}
      {starting && !awaitingTrust && (
        <>
          <p className="connecting">Starting the agent backend…</p>
          {/* Irrelevant on an installed build, where nothing gets built at all --
              shown only once this tab has been starting for 10s, not on every starting screen. */}
          {stillStarting && (
            <p className="connecting">still starting — a first start from a Verdandi checkout builds the sidecar</p>
          )}
        </>
      )}
      {/* No dashboard while starting or failed. Its box is the New session row, with the same width
          and right-side key as the other actions. Keep the box at the same tree position and parent
          on every screen: sending into Starting must not remount it and lose focus, IME composition
          or its history walk. */}
      <div className={inDashboard ? "dashboard" : "empty-box-stack"}>
        {inDashboard && hello !== null && (
          <DashboardHead hello={hello} narrow={narrow} agent={isExternalAgent(tab.agent) ? agentTitle(tab.agent) : null} />
        )}
        <QueueLines items={props.queue ?? []} error={props.queueError ?? null} note={props.queueNote ?? null} />
        <div
          className={inDashboard ? "dash-input" : "empty-box"}
          data-nav-stop={inDashboard ? "dash" : undefined}
          aria-current={inDashboard && dashCursor === 0 ? "true" : undefined}
        >
          <Composer
            disabled={failed}
            sessionEnded={failed}
            closing={false}
            restoredDraft={props.restoredDraft}
            mode={mode}
            focusRequest={composerFocus}
            caretOnFocus={composerCaret}
            hintTarget={!failed}
            onModeChange={setMode}
            onSend={props.onSend}
            onRunShell={props.onRunShell}
            atPaths={props.atPaths}
            onAtQuery={props.onAtQuery}
            claudeSlashCommands={!isExternalAgent(props.tab.agent)}
            onSlashAction={(action) => {
              if (action.kind === "sessions") runItem("sessions");
              else props.onSlashAction?.(action);
            }}
            onDraftChange={props.onDraftChange}
            running={starting}
            onQueue={props.onQueue}
            onSendNow={(text) => {
              if (text.trim() !== "") props.onQueue?.(text);
            }}
            history={props.history}
            queueCount={props.queueCount}
            onTakeBackQueue={props.onTakeBackQueue}
            queueTaken={props.queueTaken}
            onHistoryPush={props.onHistoryPush}
            onEditInNvim={props.onEditInNvim}
            editingInNvim={props.editingInNvim}
            onOpenKeymap={props.onOpenKeymap}
            // A tab that has not started has no reported list yet: Claude's built-ins, nothing for another agent.
            slashCommands={completionSource(props.tab.agent, null)}
            // The window's own key for the module to the right, which is where the panel sits.
            idleHint={props.paneFocused || props.entryKey == null ? undefined : `${props.entryKey}, then i to type`}
          />
        </div>
        {inDashboard && hello !== null && (
          <DashboardItems
            hello={hello}
            mode={tab.mode}
            cursor={dashCursor}
            onItem={runItem}
            editorLink={editorLink}
            agent={isExternalAgent(tab.agent) ? agentTitle(tab.agent) : null}
            boxIsNew
            narrow={narrow}
            paneFocused={props.paneFocused}
            entryKey={props.entryKey}
          />
        )}
      </div>
      {/* The which-key box: `WHICH_KEY_DELAY_MS` after
          a leader/table sequence started above is still pending. `.empty-tab` is this screen's own
          positioned ancestor (index.css) -- there is no `.agent-ui-scroller` here for it
          to dock against the way the live conversation's copy does, so it sits against this
          centred block's own bottom edge rather than the screen's; its real placement has not yet
          been checked in a GUI pass. */}
      {boxShown && box !== null && (
        <WhichKeyBox
          title={box.title}
          entries={box.entries}
          onPick={(key) => {
            const root = rootRef.current;
            root?.focus({ preventScroll: true });
            root?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
          }}
        />
      )}
    </div>
  );
}
