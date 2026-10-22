import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { NavKeyDirection } from "../bridge";
import type { EditorLink, HandoffCommand, Hello, QueueItem, TabInfo } from "../types";
import { EMPTY_PANEL_TABLE } from "../keymap";
import type { PanelBinding, PanelMode, PanelTable } from "../keymap";
import { advanceSequence, boxEntries, isPendingFirst, pendingPairBinding, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "../leader";
import type { BoxEntry, SeqStep } from "../leader";
import { isActivatableControl } from "../nav";
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
import { isExternalAgent } from "../tabs";
import { hintTypingFlash, isModifierKey, leaderTypingFlash, tableKeyTypingFlash, TYPE_HINT_FLASH, TypingGuard } from "../typingGuard";

/** Below this width the dashboard's where-line cuts the cwd and drops the
 *  account. Measured the same way `StatusBand` measures its own box: a `ResizeObserver` on this
 *  screen's own root, falling back to "not narrow" at the `widthPx <= 0` a real host shows for one
 *  frame before its first measurement lands (and the only thing jsdom, which lays nothing out,
 *  ever reports). */
const DASH_NARROW_PX = 360;

/** V1 C1: what `App.tsx` bumps on each `nav_key` it has not already answered with
 *  `nav_fallthrough` itself (an overlay owning the keys, a y/n, `?`) -- `seq` for the same reason
 *  `RestoredDraft`/`QueueTaken` carry one: the same direction twice in a row must still re-fire the
 *  effect below, which a bare boolean or a repeated string would coalesce away. */
export type NavKeyRequest = { seq: number; direction: NavKeyDirection };

/** The dashboard's cursor indices in the order the screen draws them, top to bottom: the box (index 0, the
 *  New session item) sits at the foot under the other rows, so `j` walks the rows and then reaches it. */
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
  /** Bumped by `App`'s `[keysRequest]` effect for the empty layout. Restore focus to the composer
   *  in INPUT or the screen root in BROWSE after a pane-focus return or overlay close, keeping
   *  keys off `document.body`. */
  keysRequest: number;
  /** Bumped by `App.tsx`'s `dropPendingKeys()` when cancellation does not reach this screen as a
   *  key, including Shift+Tab, overlays, hints, tab switches and navigation envelopes. Drop the
   *  local waiting prefix and leader sequence so they cannot swallow a later key. Optional so
   *  component tests need no stand-in. */
  dropKeysRequest?: number;
  /** True while the chooser covers this screen. Guard `focusRequest`, `arriveRequest` and
   *  `keysRequest` so they cannot steal focus from it. Pass `App.tsx`'s `overlayOpen` because
   *  this layout has no conversation `containerRef` to check. */
  overlayOpen: boolean;
  /** V1 C1: a claimed `Ctrl+j`/`Ctrl+k` `App.tsx` has already checked against its OWN
   *  overlays (`keymapOpen`, a y/n `confirm`) -- `null`/absent, or a `seq` already seen, is not a
   *  request. This screen decides the rest itself: a `starting`/`failed` tab, or its own menu
   *  (`mode === "browse"`) vs. composer (`mode === "input"`) not matching the direction, both answer
   *  through `onNavFallthrough` rather than transitioning. */
  navKeyRequest?: NavKeyRequest | null;
  /** See `navKeyRequest`. Optional, like `onChooseSessions`, so this component's own tests need no
   *  stand-in. */
  onNavFallthrough?: (direction: NavKeyDirection) => void;
  /** The initial mode on mount or a switch to another empty tab: browse after a tab switch or
   *  arrival, input after `enter_input` or at launch. `focusRequest` and `arriveRequest` outlive
   *  individual tabs, so only later counter changes are requests to this instance. Defaults to
   *  input. */
  landing?: PanelMode;
  /** This screen's own mode, for the band `App.tsx` draws under it (GUI pass 2026-09-26: the band
   *  read `INPUT` whatever this screen was in). */
  onModeChange?: (mode: PanelMode) => void;
  restoredDraft: RestoredDraft | null;
  onSend: (text: string) => void;
  onSlashAction?: (action: SlashAction) => void;
  onResume: (providerSessionId: string) => void;
  /** The dashboard's `s` item / key: bring back the tabs the last window had open. Present with the
   *  item (`hello.restore`); optional so this component's own tests need no stand-in. */
  onRestoreLast?: () => void;
  onCycleMode: () => void;
  onReset: () => void;
  onHint: (repeat: boolean) => void;
  /** The dashboard's `w` item / key: the same window-level `tab_verb
   *  choose` a live tab's `prefix w` posts (`App.tsx`'s `runPanelAction`, `tab.choose`). Optional,
   *  like `onOpenKeymap` below, so this component's own tests need no stand-in. */
  onChooseSessions?: () => void;
  /** Mirrors `Composer`'s own prop of the same name: this tab's
   *  draft is saved and restored across a switch like any other, so an empty tab's composer needs
   *  the same hook into it. */
  onDraftChange?: (text: string) => void;
  /** The window-close prompt, lifted out of `App.tsx` so both layouts' `onKeyDown`
   *  agree: called first, and if it returns `true` this component's own key handling stops there.
   *  Optional so a caller with no window (this component's own tests) needs no stand-in. */
  answerConfirm?: (event: KeyboardEvent<HTMLDivElement>) => boolean;
  /** Phase 3: this tab's own queue and prompt-history plumbing, threaded through to `Composer` --
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
  /** The queue appears above the composer, as in a live conversation. A starting tab can already
   *  have messages queued behind connection. Editor context lives in the outer band's context fact,
   *  built by `App.tsx` from `editorContext`, so this component needs no copy. */
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
  /** The panel leader/which-key table and callback for completed bindings. Both are optional,
   *  defaulting to `EMPTY_PANEL_TABLE` and a no-op for component tests. Share `App.tsx`'s
   *  `runPanelAction`: it handles mode cycling without a started session and posts window-level
   *  `tab_verb` actions from either layout. */
  panelTable?: PanelTable;
  onPanelAction?: (binding: PanelBinding) => void;
  /** Companion mode (`App.tsx`'s `editorLink`): passed through to `Dashboard`, whose words change when
   *  the editor is another window. `null` (the default) is the one-window mode. */
  editorLink?: EditorLink | null;
  /** Share `App.tsx`'s typing guard so the dashboard leader starts only for a key pressed alone or
   *  at the end of a quick motion. A space inside typed text must not arm a mode-changing sequence.
   *  If omitted, this screen keeps its own guard. */
  typingGuard?: TypingGuard;
  /** Says a refused leader in the band (`App.tsx`'s `showFlash`); optional for the same reason. */
  onFlash?: (text: string) => void;
};

/** An empty session tab: Claude Code's fresh prompt. The composer is live in INPUT;
 *  the first send creates the session in Rust. `Shift+Tab` toggles the mode (O2 a; entering
 *  bypass asks first). Nothing here
 *  spawns a process. */
export function EmptyTab(props: EmptyTabProps) {
  const { hello, tab, handoff, failure } = props;
  const [mode, setMode] = useState<PanelMode>(props.landing === "browse" ? "browse" : "input");
  /** Deferred `f` and single-key table bindings read the current mode at fire time. Reading
   *  `mode` or `failed` directly in `typingGuard.defer` would capture the render that scheduled
   *  the callback, before `TYPING_GUARD_MS` elapsed. */
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
  // `null` means there is no failure or the classifier does not recognise it. The row then shows
  // only the raw text.
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
  /** Focus counter read by `Composer`, mirroring `App.tsx`'s `composerFocusRequest`. Only the
   *  guarded `focusRequest` and `keysRequest` effects bump it, so the composer cannot take
   *  focus from an overlay. */
  const [composerFocus, setComposerFocus] = useState(0);
  /** Mirrors `App.tsx`'s `composerCaret`: `A` forces the caret to the end of the draft. Every
   *  other route into INPUT (`i`/`o`, `Ctrl+j`, a landing) resets it to "kept" so an earlier `A`
   *  cannot affect the next entry. */
  const [composerCaret, setComposerCaret] = useState<"kept" | "end">("kept");
  const [widthPx, setWidthPx] = useState(0);
  /* The dashboard's own narrow breakpoint, measured the same way
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
  /* The leader/which-key engine stays local because this tab keeps its own `mode`, which
     `App.tsx` cannot see. BROWSE here gives the dashboard the keys. Reserved `g`/`z`/`[`/`]`
     prefixes support table pairs such as `[b`/`]b` for stepping tabs. Fixed `resolveKey` pairs
     such as `gg` and `[[` have no rows to act on here. Any key that does not complete a table
     pair cancels a waiting prefix. */
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
  // Clear a pending sequence when the table changes so a binding from the old table cannot run
  // against the new one.
  useEffect(() => {
    clearSequence();
    // `clearSequence` drops only a pending multi-key sequence. A single-key binding deferred by
    // `typingGuard.defer` closes over the old table binding at keydown, so cancel it too when
    // the table changes to avoid running a stale binding.
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
  // `dropKeysRequest` clears pending sequences through the window cancellation routes.
  // This also runs on mount, when there is nothing to clear.
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
  /* A request for the keys (`enter_input`, a chooser closing) must enter INPUT as well as
     request focus. Blurring `Composer` leaves this tab in BROWSE with no textarea to focus.
     Apply the same guards as `i`, and skip the request while an overlay is open so composer
     autofocus cannot take its keys. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.focusRequest !== focusAtMount.current && !starting && !failed) {
      setMode("input");
      setComposerCaret("kept");
      setComposerFocus((n) => n + 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.focusRequest]);
  /* An `arrive` request ends INPUT and resets the cursor to the dashboard's first item, "new".
     BROWSE needs no composer, so this also works while starting or failed. Focus the root before
     `setMode("browse")` unmounts the textarea so focus does not fall to `document.body`. Skip
     this while `overlayOpen` so a chooser, rename, or search keeps the keys. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.arriveRequest !== arriveAtMount.current) {
      rootRef.current?.focus({ preventScroll: true });
      setMode("browse");
      setDashCursor(0);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.arriveRequest]);
  /* Failure disables the focused composer, which can move focus to <body> and leave retry
     keys dependent on document replay. Focus the root before entering BROWSE so this screen
     can handle them. The live layout only ends VISUAL and sets BROWSE because its conversation
     root already handles replayed keys. Move focus only if it is in the disabled composer or
     outside this screen, and never while an overlay is open. */
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
  /** `App`'s `[keysRequest]` effect bumps this directly because this layout has no
   *  `containerRef`. If the root already contains the active element, DOM focus survived the GTK
   *  focus transfer and needs no change, just as in the live conversation's `takeKeys` guard. */
  useEffect(() => {
    if (props.keysRequest === 0 || props.overlayOpen) return;
    const root = rootRef.current;
    if (root === null || root.contains(document.activeElement)) return;
    if (mode === "input" && !starting && !failed) setComposerFocus((n) => n + 1);
    else root.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.keysRequest]);
  /** V1 C1: the menu's own `Ctrl+j`/the composer's own `Ctrl+k`, echoed back once
   *  Rust's mirror has claimed one. Initialized from whatever `props.navKeyRequest` already was
   *  (the `focusAtMount`/`arriveAtMount` pattern, "requests are edges, not levels" above): a count
   *  reached before this screen mounted -- another tab's own stale request, still sitting in
   *  `App.tsx`'s state -- is not a request to this one either. `starting`/`failed` (no box to enter)
   *  and a `mode` that no longer matches the direction both report back
   *  through `onNavFallthrough` rather than transitioning. */
  const navKeySeenRef = useRef<number | null>(props.navKeyRequest?.seq ?? null);
  useEffect(() => {
    const req = props.navKeyRequest;
    if (req === null || req === undefined || req.seq === navKeySeenRef.current) return;
    navKeySeenRef.current = req.seq;
    if (props.overlayOpen) {
      props.onNavFallthrough?.(req.direction);
      return;
    }
    // A starting tab has a live composer that queues behind the connection. Leaving INPUT must
    // behave like `Esc`, keeping the keys on this screen. Handle that before the `starting ||
    // failed` fallthrough, which still handles entry into INPUT and failed tabs. Focus the root
    // before `setMode("browse")` unmounts the textarea so focus does not fall to `document.body`.
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
  /* The request effects ignore counts reached before mount, so mounting or switching must
     place focus explicitly. Otherwise a new tab can inherit no focused element after the live
     container unmounts. BROWSE focuses the root; INPUT focuses the composer only when
     `focusRequest > 0`, so a plain launch leaves DOM focus alone. An open overlay keeps focus. */
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
        // C1a's own default: `i`/`o` (and a click, and `Enter` on this item) place the caret where
        // it was left, never forced to the end -- only the `A` branch below overrides this after
        // calling `runItem("new")` itself.
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

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (props.answerConfirm?.(event)) return;
    // An overlay such as a chooser or rename owns the keys. A key replayed from <body> that reaches
    // this root must not act on the tab behind the overlay.
    if (props.overlayOpen) return;
    // Pass every other keydown to the typing guard first, as `App.tsx` does.
    // The leader handler below asks the guard whether the key stood alone.
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    // A key that cancelled a waiting `f`, `H`/`L`, `r` or `w` says so, with that key's own words --
    // this used to be dropped, so the dashboard said nothing at all.
    const cancelled = typingGuard.onKey(event.key, typedAt);
    if (cancelled !== null) props.onFlash?.(cancelled);
    // Each non-modifier key reads and clears the reserved prefix first.
    // An input method key or an INPUT key clears it too.
    if (pendingPrefixRef.current !== null && isModifierKey(event.key)) return;
    const prefix = pendingPrefixRef.current;
    pendingPrefixRef.current = null;
    // An IME key is never a key of this screen's, the leader engine's below included: the same
    // `isImeKey` test App.tsx's own leader engine and the composer use, so WebKit's `keyCode` 229
    // (a key the input method consumed, reported without `isComposing`) is caught here as well.
    if (isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) return;
    // Shift+Tab is caught by App.tsx's document-capture router (`modeKey.ts`) before this handler.
    // A failed tab has no live composer, so an "input" mode must still allow `r`, HINT `f`, and
    // handoff-copy `y`. A starting tab has a live composer that queues behind the connection, so
    // INPUT still claims its keys.
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
    // A reserved prefix waits without a timeout. The next key completes a table pair or is
    // swallowed as cancellation before ordinary keys, the leader, or control activation can run.
    // Shift can form pairs such as `gT`; Ctrl/Alt cannot because table keys are not chords.
    // `Ctrl+c` still falls through to native copy, as it does over a live tab.
    if (prefix !== null) {
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
    // Any non-modifier key within `TYPING_GUARD_MS` of a BROWSE `y` is swallowed as typing, and the
    // band explains how to type. Check before focused controls so `y` then Enter is claimed too. A
    // lone Enter after a pause is unaffected.
    if (!isModifierKey(event.key) && typingGuard.afterCopy(typedAt)) {
      event.preventDefault();
      if (cancelled === null) props.onFlash?.(TYPE_HINT_FLASH);
      return;
    }
    // v1 hardening, codex-release-p1 #5: a pending sequence's own continuation must run BEFORE the
    // unconditional single-key handlers below, matching `App.tsx`'s own ordering (leader engine
    // before other single-key handling: `f` there reaches HINT only through `resolveKey`, well
    // after this same check). Without this, `f` alone unconditionally intercepted every `f`
    // keydown -- including the second key of an armed `<leader>fn` -- so that binding could never
    // complete here. `isActivatableControl`'s Enter/Space guard moves with it: both exist to keep
    // the sequence engine from swallowing keys it must not (a focused button's own Enter/Space, or a
    // continuation key `r`/`f`/`y` also happen to claim on their own below).
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ")) return;
    if (seqRef.current !== null) {
      // A modifier keydown is not the next sequence key, so it must leave the sequence pending.
      // This matches the guards in `App.tsx` and for `pendingPrefixRef` below.
      if (isModifierKey(event.key)) return;
      event.preventDefault();
      // No table key carries a modifier, so a Ctrl/Alt chord cancels the sequence. Matching only
      // `event.key` in `advanceSequence` could otherwise complete a binding.
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
      // Defer `f` for `TYPING_GUARD_MS` so prose starting with that letter cannot open HINT. Fire
      // only if no more keys were typed and the current `modeRef`/`failedRef` still allow `f` to
      // act.
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
      // Every BROWSE `y` starts the typing guard window, even without a handoff command to copy.
      // Otherwise `y` then Enter could run the selected action and `y` then `j` could move the
      // cursor while typing.
      typingGuard.noteCopy(typedAt);
      if (handoff !== null) {
        event.preventDefault();
        void navigator.clipboard?.writeText(handoff.command);
        return;
      }
    }
    if (hello === null) return;
    // The leader/which-key engine also runs on starting and failed tabs so `mode.cycle` reaches
    // `modeKeyRoute`. Starting tabs cycle like live tabs; failed tabs flash `modeFixedMessage`
    // unless already in bypass. Only the dashboard menu keys below require a `not_started` tab. Try
    // the leader/which-key engine before the dashboard's fixed `j`/`k`/`Enter`/letter handlers,
    // matching `App.tsx`. Table bindings take keys not already claimed above; the reserved-key list
    // prevents collisions with fixed bindings. Arm reserved prefixes (`g`/`z`/`[`/`]`) here; the
    // next key completes a table pair or cancels at the top of this handler. A bare modifier
    // keydown, such as Shift before `gT`, keeps the prefix waiting.
    if (!event.ctrlKey && !event.altKey && !event.shiftKey && isPendingFirst(event.key)) {
      event.preventDefault();
      pendingPrefixRef.current = event.key;
      return;
    }
    if (!event.ctrlKey && !event.altKey) {
      const start = startSequence(panelTable, event.key, isActivatableControl(event.target));
      if (start.kind !== "none") {
        event.preventDefault();
        // The v1-ui GUI pass (2026-09-27): in the middle of typed prose the leader is swallowed and
        // says so, as over a live conversation (`TypingGuard.mayActAfterMotion`).
        if (event.key === panelTable.leader && !typingGuard.mayActAfterMotion(typedAt, event.repeat)) {
          props.onFlash?.(leaderTypingFlash(panelTable.leaderLabel));
          return;
        }
        // Defer single-key table bindings such as `H`/`L`, just like `a`/`d`/`f`, so prose starting
        // with one cannot trigger an action. `mayActAfterMotion` cannot protect the first key
        // because no motion run precedes it.
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
    // The dashboard has one column, so h/l do not change the item. Claim them here so they
    // do not fall through to the composer or leave as unhandled keys.
    if (event.key === "h" || event.key === "l") {
      event.preventDefault();
      return;
    }
    if (event.key === "j" || event.key === "k" || event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const delta = event.key === "j" || event.key === "ArrowDown" ? 1 : -1;
      setDashCursor((c) => {
        const order = screenOrder(items.length);
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
    // Bare `m` is unbound so prose such as "make" cannot change the mode. The mode item remains
    // reachable with `j`/`k` then Enter, Shift+Tab through `modeKey.ts`, or `<leader>m`;
    // `DASH_ITEM_KEY.mode` is display only. `o` aliases `i`, while `A` opens the composer with the
    // caret at the draft's end. `i`/`o`/`A` require a standalone key through `mayStartInput`, with
    // a pause and no motion exception. Inside typed prose they are swallowed and flash a hint on
    // how to type.
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
      event.key === "w"
    ) {
      // Arrival puts the dashboard in BROWSE, where prose can accidentally trigger actions such as
      // `r` to resume or `w` to open the chooser. Defer these actions like `f` and `H`/`L` until
      // `TYPING_GUARD_MS` has passed without more typing and the dashboard is still in BROWSE.
      event.preventDefault();
      const item: DashItem = event.key === "r" ? "resume" : event.key === "s" ? "restore" : "sessions";
      const flash = tableKeyTypingFlash(
        event.key,
        item === "resume" ? "resume the newest session" : item === "restore" ? "restore the last session" : "all sessions",
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

  // `hello !== null` beside `showDashboard`, which TS cannot narrow through, so the dashboard's parts take
  // `hello` without a non-null assertion.
  const inDashboard = hello !== null && showDashboard;
  return (
    <div className="empty-tab" tabIndex={0} onKeyDown={onKeyDown} ref={rootRef}>
      {handoff !== null && <HandoffCommandCard handoff={handoff} />}
      {failed && (
        <Row kind="error" sign="✗" role="alert" problem={failureProblem}>
          <strong>{tab.failedAfterStart ? "This tab's session stopped." : "This tab's session did not start."}</strong>
          <pre>{failure === null ? "no reason was given" : failureEvidence(failure)}</pre>
          {/* Omit the retry hint when the remedy already names the same key. */}
          {!remedyNamesR(failureProblem) && <div className="row-hint">Press r to start a new session here.</div>}
        </Row>
      )}
      {awaitingTrust && (
        <p className="connecting">Waiting for your answer to the trust question — nothing has started</p>
      )}
      {starting && !awaitingTrust && (
        <>
          <p className="connecting">Starting the agent backend…</p>
          {/* irrelevant on an installed build, where nothing gets built at all --
              shown only once this tab has been starting for 10s, not on every starting screen. */}
          {stillStarting && (
            <p className="connecting">still starting — a first start from a Verdandi checkout builds the sidecar</p>
          )}
        </>
      )}
      {/* The empty tab's dashboard: replaces the eight resume rows that used to sit below
          the composer. Never drawn while starting or failed (those screens keep their own).

          On the dashboard the box you type in IS the New session row (2026-10-15): it sits between the
          name and the other rows, as wide as they are, its key at the right where theirs are, and the
          cursor's first stop. The box keeps one place in the tree on every screen -- the same parent
          and the same position, whether the dashboard is drawn around it or not -- so going from the
          dashboard to "Starting…" when a message is sent never remounts it (its focus, an input
          method's composition and its history walk all live in it). */}
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
          />
        )}
      </div>
      {/* The which-key box: `WHICH_KEY_DELAY_MS` after
          a leader/table sequence started above is still pending. `.empty-tab` is this screen's own
          positioned ancestor (index.css) -- there is no `.agent-ui-scroller` here for it
          to dock against the way the live conversation's copy does, so it sits against this
          centred block's own bottom edge rather than the screen's; a GUI pass owes the real
          placement. */}
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
