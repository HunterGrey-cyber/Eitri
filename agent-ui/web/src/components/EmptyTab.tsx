import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { NavKeyDirection } from "../bridge";
import type { HandoffCommand, Hello, QueueItem, TabInfo } from "../types";
import { EMPTY_PANEL_TABLE } from "../keymap";
import type { PanelBinding, PanelMode, PanelTable } from "../keymap";
import { advanceSequence, boxEntries, isPendingFirst, pendingPairBinding, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "../leader";
import type { BoxEntry, SeqStep } from "../leader";
import { isActivatableControl } from "../nav";
import { isImeKey } from "../composerKeys";
import { Composer } from "./Composer";
import type { RestoredDraft } from "./Composer";
import { HandoffCommandCard } from "./TerminalHandoff";
import { QueueLines } from "./QueueLines";
import { Row } from "./Row";
import { Dashboard, dashItems } from "./Dashboard";
import type { DashItem } from "./Dashboard";
import { WhichKeyBox } from "./WhichKeyBox";
import { classify } from "../problems";
import { leaderTypingFlash, TypingGuard } from "../typingGuard";

/** Below this width (spec §7, "At 360 px") the dashboard's where-line cuts the cwd and drops the
 *  account. Measured the same way `StatusBand` measures its own box: a `ResizeObserver` on this
 *  screen's own root, falling back to "not narrow" at the `widthPx <= 0` a real host shows for one
 *  frame before its first measurement lands (and the only thing jsdom, which lays nothing out,
 *  ever reports). */
const DASH_NARROW_PX = 360;

/** V1 C1 (spec §3.5): what `App.tsx` bumps on each `nav_key` it has not already answered with
 *  `nav_fallthrough` itself (an overlay owning the keys, a y/n, `?`) -- `seq` for the same reason
 *  `RestoredDraft`/`QueueTaken` carry one: the same direction twice in a row must still re-fire the
 *  effect below, which a bare boolean or a repeated string would coalesce away. */
export type NavKeyRequest = { seq: number; direction: NavKeyDirection };

export type EmptyTabProps = {
  hello: Hello | null;
  tab: TabInfo;
  handoff: HandoffCommand | null;
  failure: string | null;
  paneFocused: boolean;
  focusRequest: number;
  /** Bumped by App's `arrive` handling (panel round 2, spec §8, decision 4): lands BROWSE with the
   *  cursor on the dashboard's `New session` item, the same reversal a live tab's arrival gets. */
  arriveRequest: number;
  /** Wave 3 Task 1: bumped by `App`'s own `[keysRequest]` effect when this is the empty layout --
   *  the launch-chooser investigation's defect 4 (a `pane_focus` round trip, or an overlay closing,
   *  must never leave the keys on `document.body`). Landed on the composer (INPUT) or this screen's
   *  own root (BROWSE), whichever currently has the live control. */
  keysRequest: number;
  /** Wave 3 Task 1: true while the `prefix w` chooser is drawn over this screen. Guards
   *  `focusRequest`/`arriveRequest`/`keysRequest` from stealing the keys out from under it -- the
   *  same `overlayOpen` `App.tsx` computes, threaded through since this screen has no
   *  `containerRef` of its own for `App` to check. */
  overlayOpen: boolean;
  /** V1 C1 (spec §3.5): a claimed `Ctrl+j`/`Ctrl+k` `App.tsx` has already checked against its OWN
   *  overlays (`keymapOpen`, a y/n `confirm`) -- `null`/absent, or a `seq` already seen, is not a
   *  request. This screen decides the rest itself: a `starting`/`failed` tab, or its own menu
   *  (`mode === "browse"`) vs. composer (`mode === "input"`) not matching the direction, both answer
   *  through `onNavFallthrough` rather than transitioning. */
  navKeyRequest?: NavKeyRequest | null;
  /** See `navKeyRequest`. Optional, like `onChooseSessions`, so this component's own tests need no
   *  stand-in. */
  onNavFallthrough?: (direction: NavKeyDirection) => void;
  /** Where this screen starts when it mounts, or when the window switches it to another empty tab
   *  (GUI pass 2026-09-26, r2-gui): `"browse"` after a tab switch or an arrival, `"input"` after a
   *  new tab's `enter_input` (R7) and at launch. `focusRequest` and `arriveRequest` are the
   *  window's counters and outlive any one tab, so a count already reached when this mounted is
   *  not a request to it -- only a later change is. Defaults to `"input"`, today's launch state. */
  landing?: PanelMode;
  /** This screen's own mode, for the band `App.tsx` draws under it (GUI pass 2026-09-26: the band
   *  read `INPUT` whatever this screen was in). */
  onModeChange?: (mode: PanelMode) => void;
  restoredDraft: RestoredDraft | null;
  onSend: (text: string) => void;
  onResume: (providerSessionId: string) => void;
  onCycleMode: () => void;
  onReset: () => void;
  onHint: (repeat: boolean) => void;
  /** The dashboard's `w` item / key (spec §7, "All sessions"): the same window-level `tab_verb
   *  choose` a live tab's `prefix w` posts (`App.tsx`'s `runPanelAction`, `tab.choose`). Optional,
   *  like `onOpenKeymap` below, so this component's own tests need no stand-in. */
  onChooseSessions?: () => void;
  /** Mirrors `Composer`'s own prop of the same name (session tabs Task 11, ruling 24): this tab's
   *  draft is saved and restored across a switch like any other, so an empty tab's composer needs
   *  the same hook into it. */
  onDraftChange?: (text: string) => void;
  /** The window-close prompt (ruling 7), lifted out of `App.tsx` so both layouts' `onKeyDown`
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
   *  `Composer` DOES need something wired to `onSendNow`, though (fix round 1, reviewer finding):
   *  its `submit()` clears the box on the `now` branch unconditionally, whether or not `onSendNow`
   *  did anything, so leaving it as the default no-op silently threw away whatever the user typed
   *  on Ctrl+Enter. A starting tab has no turn to send to "now" either, so it falls back to the same
   *  effect as `onQueue` below, guarded against an empty box (`queue_message` has no such guard of
   *  its own and would happily queue a blank entry). */
  onQueue?: (text: string) => void;
  history?: string[];
  queueCount?: number;
  /** The queue, shown above the composer the same way the live conversation shows it (Task 9): a
   *  `starting` tab can already have queued behind its own connect (C1). V1's editor-context line
   *  used to be shown here too, through its own `ContextLine`; panel round 2 (plan Task 10) moved
   *  it into the outer band's `context` fact instead (`App.tsx` builds that directly from its own
   *  `editorContext` state, so this component no longer needs a copy of it at all). */
  queue?: QueueItem[];
  queueError?: string | null;
  onTakeBackQueue?: () => void;
  queueTaken?: { texts: string[]; seq: number } | null;
  onHistoryPush?: (text: string) => void;
  onEditInNvim?: (text: string) => void;
  editingInNvim?: boolean;
  onOpenKeymap?: () => void;
  /** The panel's leader/which-key table (`keymapHelp.panel` in `App.tsx`, itself `keymap` envelope's
   *  `panel`), and where a completed sequence's binding goes. Fix round 1 (panel round 2 plan Task
   *  12+13, reviewer finding): the dashboard's own root never ran the leader engine that `App.tsx`'s
   *  `onKeyDown` already had for the live conversation (spec §7, "Space starts a leader sequence"),
   *  so Space did nothing here. Both optional, defaulting to `EMPTY_PANEL_TABLE` (no bindings) and a
   *  no-op, so this component's own tests need no stand-in for either -- the same reason
   *  `onChooseSessions` above is optional. `onPanelAction` is `App.tsx`'s own `runPanelAction`,
   *  reused rather than duplicated: its `mode.cycle` case already branches on whether a session has
   *  started (posting `cycle_mode` directly here, since this tab has none), and its `tab.*` cases
   *  post the same window-level `tab_verb` regardless of which tab held the keys. */
  panelTable?: PanelTable;
  onPanelAction?: (binding: PanelBinding) => void;
  /** V1 P11 (spec §10.1): the window's own prefix chord, as a person reads it (`App.tsx`'s
   *  `keymapHelp.prefix`) -- threaded straight through to `Dashboard`'s own first-run hint line,
   *  which is the only thing here that reads it. Defaults to `"Ctrl+b"`, the same stock default
   *  `keymapHelp` itself starts at before the `keymap` envelope arrives, so a caller (this
   *  component's own tests included) that never configured a prefix still gets a truthful hint. */
  prefix?: string;
  /** The v1-ui GUI pass (2026-09-27): the panel's one typing guard (`App.tsx`'s own), so the leader
   *  here starts a sequence only on a key that stands alone or ends a quick motion, as it does over
   *  a live conversation -- "set up my" typed onto this dashboard ran `<leader>m` and flipped the
   *  stored mode to bypass, the thing S2 took the bare `m` away to stop. Optional: without one this
   *  screen keeps its own, so its tests need no stand-in. */
  typingGuard?: TypingGuard;
  /** Says a refused leader in the band (`App.tsx`'s `showFlash`); optional for the same reason. */
  onFlash?: (text: string) => void;
};

/** An empty session tab: Claude Code's fresh prompt (spec §3.6, F3). The composer is live in INPUT;
 *  the first send creates the session in Rust (ruling 4). `Shift+Tab` cycles the mode. Nothing here
 *  spawns a process. */
export function EmptyTab(props: EmptyTabProps) {
  const { hello, tab, handoff, failure } = props;
  const [mode, setMode] = useState<PanelMode>(props.landing === "browse" ? "browse" : "input");
  // The counts this screen mounted with (see `landing`'s doc): the two request effects below act
  // only on a count that changed after that.
  const focusAtMount = useRef(props.focusRequest);
  const arriveAtMount = useRef(props.arriveRequest);
  const starting = tab.state === "starting";
  const failed = tab.state === "failed";
  // Spec §10.2 (P11): `null` for a failure this build's classifier does not recognise (or none),
  // in which case the row below draws exactly what it always did -- the raw text, and nothing more.
  const failureProblem = failed && failure !== null ? classify(failure, hello?.account ?? null) : null;
  const showDashboard = hello !== null && !starting && !failed;
  const prefix = props.prefix ?? "Ctrl+b";
  const [dashCursor, setDashCursor] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  /** F19 (spec §10.2): "The first start on a fresh Verdandi checkout also builds the sidecar." used
   *  to be shown unconditionally on every starting screen, which is irrelevant on an installed
   *  build where nothing gets built at all. It now waits 10s, per THIS tab's own starting spell
   *  (`tab.id` in the deps below, so a switch to a different starting tab restarts the wait rather
   *  than inheriting the old tab's elapsed time), and reads differently once it does. No wire
   *  change: `tab.state` already said "starting", this only delays and rewords one sentence. */
  const [stillStarting, setStillStarting] = useState(false);
  useEffect(() => {
    if (!starting) {
      setStillStarting(false);
      return;
    }
    const timer = setTimeout(() => setStillStarting(true), 10_000);
    return () => clearTimeout(timer);
  }, [starting, tab.id]);
  /** Wave 3 Task 1: what `Composer` actually reads, mirroring `App.tsx`'s own `composerFocusRequest`
   *  -- bumped only from the guarded `focusRequest`/`keysRequest` effects below, so an overlay drawn
   *  over this screen can never have its composer autofocused out from under it. */
  const [composerFocus, setComposerFocus] = useState(0);
  const [widthPx, setWidthPx] = useState(0);
  /* The dashboard's own narrow breakpoint (spec §7, "At 360 px"), measured the same way
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
  /* The leader/which-key engine (fix round 1, panel round 2 plan Task 12+13): a scoped-down copy of
     `App.tsx`'s own `seqRef`/`seq`/`boxShown`/`applySeqStep`/`clearSequence`, kept local to this
     screen rather than lifted to `App.tsx` -- this tab already keeps its own `mode` locally (BROWSE
     here means the dashboard has the keys, not the live conversation), and `App.tsx`'s copy cannot
     see it. The reserved `g`/`z`/`[`/`]` prefixes get only their table half here
     (`pendingPrefixRef` below): `[b`/`]b` step tabs from the dashboard as from a live tab (spec §4,
     the whole-branch review), while `resolveKey`'s own fixed pairs (`gg`, `[[`, ...) have no rows
     to act on on this screen and stay unimplemented. */
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
  // Review Focus 1, reproduced for this local copy: a table that changes mid-sequence must never
  // let the OLD table's binding run against the new one.
  useEffect(() => {
    clearSequence();
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
  useEffect(() => {
    return () => {
      cancelBoxTimer();
      cancelSeqTimeoutTimer();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const box: { title: string; entries: BoxEntry[] } | null =
    seq !== null ? { title: sequenceTitle(panelTable, seq.typed), entries: boxEntries(panelTable, seq.typed, starting || failed) } : null;
  /* A request for the keys (`enter_input`, a chooser closing) is a request for INPUT, not only for
     focus: once anything took focus off the textarea (the chooser, a HINT, a click), `Composer`'s
     blur left this tab in BROWSE with no textarea to focus, and the keys landed on an ancestor that
     handles none (GUI pass, 2026-09-25). Refused where `i` is. Wave 3 Task 1: also refused while an
     overlay (the launch chooser) is drawn over this screen -- its own `autoFocus` must not steal the
     keys from a chooser opened over the empty tab. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.focusRequest !== focusAtMount.current && !starting && !failed) {
      setMode("input");
      setComposerFocus((n) => n + 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.focusRequest]);
  /* Panel round 2 (spec §8, decision 4): an `arrive` that lands here -- a launch that starts with the
     keys already in the chat -- ends INPUT the way it ends a live tab's, rather than leaving the
     fresh prompt's default composer focused. No `!starting && !failed` guard: unlike `focusRequest`,
     landing BROWSE never needs a box to type into. The cursor resets to the dashboard's first item
     (`"new"`, spec §8: "the New session item"). Wave 3 Task 1: focusing the root happens BEFORE
     `setMode("browse")` -- that synchronously blurs the textarea (its own `onBlur` then sees
     `relatedTarget` already the root, not `.history-search`, and reports browse itself too), so the
     unmount a moment later never drops the keys onto `document.body` (defect 4). Also guarded by
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
  /** Wave 3 Task 1: `App`'s own `[keysRequest]` effect bumps this directly (there is no
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
  /** V1 C1 (spec §3.1, §3.5): the menu's own `Ctrl+j`/the composer's own `Ctrl+k`, echoed back once
   *  Rust's mirror has claimed one. Initialized from whatever `props.navKeyRequest` already was
   *  (the `focusAtMount`/`arriveAtMount` pattern, "requests are edges, not levels" above): a count
   *  reached before this screen mounted -- another tab's own stale request, still sitting in
   *  `App.tsx`'s state -- is not a request to this one either. `starting`/`failed` (no box to enter)
   *  and a `mode` that no longer matches the direction (stale, Review Focus 2) both report back
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
    // Fix round 1 (reviewer finding): a starting tab's composer is live ("C1: it queues behind the
    // connect", this file's own `onKeyDown` comment above), so leaving an already-open INPUT must
    // behave the same way `Esc` does there -- not fall through to Rust's `move_focus` and hand the
    // keys off this screen entirely. Checked ahead of the `starting || failed` fallthrough below,
    // which still owns every other case (entering INPUT while starting/failed at all, and `failed`'s
    // own dead INPUT, never exempted here either, matching `Esc`'s own `!failed` guard). The root
    // takes focus first, as `Esc`'s own branch does: `setMode("browse")` unmounts the textarea, and
    // focusing afterwards would drop the keys onto `document.body` (wave 3 Task 1).
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
      return;
    }
    props.onNavFallthrough?.(req.direction);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.navKeyRequest]);
  /* Where the keys go when this screen is landed (integration of wave 3's single focus route with
     r2-gui's edge-only requests): the two request effects above now ignore a count reached before
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
    setDashCursor(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.id]);
  useEffect(() => {
    props.onModeChange?.(mode);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);

  /** What each dashboard item does (spec §7's table) -- the one place both a click (`Dashboard`'s
   *  own `onItem`) and a keyboard letter/`Enter` (`onKeyDown` below) end up. */
  function runItem(item: DashItem) {
    switch (item) {
      case "new":
        setMode("input");
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
    // Every other keydown is "a key" to the typing guard, first, as `App.tsx`'s own `onKeyDown` does
    // (v1 S1): the leader below asks it whether it stood alone.
    const typedAt = event.timeStamp > 0 ? event.timeStamp : performance.now();
    typingGuard.onKey(event.key, typedAt);
    // An IME key is never a key of this screen's, the leader engine's below included: the same
    // `isImeKey` test App.tsx's own leader engine and the composer use, so WebKit's `keyCode` 229
    // (a key the input method consumed, reported without `isComposing`) is caught here as well.
    if (isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) return;
    // Wave 4 Task 1: Shift+Tab used to be claimed here directly; it is now caught by App.tsx's
    // document-capture router (`modeKey.ts`) before it ever reaches this handler.
    // Once the composer is disabled (failed) there is nothing left to type into, so `mode` staying
    // "input" must not swallow the keys the disabled screen still offers -- `r` on a failed tab, `f`
    // for HINT, `y` to copy a handoff command -- the same reasoning `Composer`'s own doc comment
    // gives for a dead session's INPUT being an empty mode. A `starting` tab's composer is live now
    // (C1: it queues behind the connect), so it is no longer exempted here.
    if (mode === "input" && !failed) {
      // vim's Esc (and the live conversation's, `resolveKey`): back to the menu. The root takes the keys
      // BEFORE `setMode("browse")` unmounts the textarea, or they would fall to <body> (wave 3 Task 1, defect 4).
      if (event.key === "Escape" && !event.nativeEvent.isComposing) {
        event.preventDefault();
        rootRef.current?.focus({ preventScroll: true });
        setMode("browse");
      }
      return;
    }
    if (event.key === "r" && failed) {
      event.preventDefault();
      props.onReset();
      return;
    }
    if (event.key === "f") {
      event.preventDefault();
      props.onHint(event.repeat);
      return;
    }
    if (event.key === "y" && handoff !== null) {
      event.preventDefault();
      void navigator.clipboard?.writeText(handoff.command);
      return;
    }
    if (hello === null) return;
    // The leader/which-key engine runs on a `starting` or `failed` tab too (whole-branch review):
    // R4 names `<leader>` `mode.cycle` beside Shift+Tab as flashing `mode is fixed ...` on those
    // tabs, and returning here first left it doing nothing at all there. Only the dashboard's own
    // menu keys below stay a `not_started` tab's.
    // The leader/which-key engine (fix round 1, panel round 2 plan Task 12+13; spec §7, "Space
    // starts a leader sequence"): tried first, mirroring `App.tsx`'s own `onKeyDown` ordering, so a
    // table binding on any key not already claimed above (Space itself, or `H`/`L`/... once the
    // owner's table has them) takes it before the dashboard's own fixed `j`/`k`/`Enter`/letters
    // below ever see it -- the reserved-key list (Global Constraint) is what keeps those from ever
    // colliding with a real table entry.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ")) return;
    if (seqRef.current !== null) {
      event.preventDefault();
      applySeqStep(advanceSequence(panelTable, seqRef.current.typed, event.key));
      return;
    }
    // A reserved prefix typed a key ago (`[`): its table pair runs (`[b` -> tab.prev); anything
    // else drops the prefix and is read as an ordinary key, as vim drops an unfinished `g` and as
    // `resolveKey` does for a live tab.
    const prefix = pendingPrefixRef.current;
    pendingPrefixRef.current = null;
    if (prefix !== null && !event.ctrlKey && !event.altKey && !event.shiftKey) {
      const pair = pendingPairBinding(panelTable, prefix, event.key);
      if (pair !== null) {
        event.preventDefault();
        props.onPanelAction?.(pair);
        return;
      }
    }
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
        applySeqStep(start);
        return;
      }
    }
    if (starting || failed) return;
    const items = dashItems(hello);
    // R3 (snacks.nvim's dashboard: one column): h/l never change the item, but they are still
    // claimed here so they never fall through to the composer or leak out as an unhandled key.
    if (event.key === "h" || event.key === "l") {
      event.preventDefault();
      return;
    }
    if (event.key === "j" || event.key === "k" || event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const delta = event.key === "j" || event.key === "ArrowDown" ? 1 : -1;
      setDashCursor((c) => Math.max(0, Math.min(items.length - 1, c + delta)));
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      const item = items[dashCursor];
      if (item !== undefined) runItem(item);
      return;
    }
    // V1 S2 (spec §2.4): `m` used to run `runItem("mode")` here. It collided with ordinary typed
    // prose starting with `m` (e.g. "make", "mode") landing on the dashboard before `i` was
    // pressed, so the bare letter is gone -- the item stays reachable by `j`/`k` + Enter (above),
    // Shift+Tab (`modeKey.ts`'s document-capture router, which runs ahead of this handler and is
    // untouched here) and `<leader>m` (the leader engine above, `DASH_ITEM_KEY.mode` is display
    // only). A stray `m` (or the `m` in "make") now falls through this whole chain and does
    // nothing, exactly like any other unbound letter.
    if (event.key === "i") {
      event.preventDefault();
      runItem("new");
    } else if (event.key === "r" && items.includes("resume")) {
      event.preventDefault();
      runItem("resume");
    } else if (event.key === "w") {
      event.preventDefault();
      runItem("sessions");
    } else if (event.key === "?") {
      event.preventDefault();
      runItem("keys");
    }
  }

  return (
    <div className="empty-tab" tabIndex={0} onKeyDown={onKeyDown} ref={rootRef}>
      {handoff !== null && <HandoffCommandCard handoff={handoff} />}
      {failed && (
        <Row kind="error" sign="✗" role="alert" problem={failureProblem}>
          <strong>This tab's session did not start.</strong>
          <pre>{failure ?? "no reason was given"}</pre>
          <div className="row-hint">Press r to start a new session here.</div>
        </Row>
      )}
      {starting && (
        <>
          <p className="connecting">Starting the agent backend…</p>
          {/* F19 (spec §10.2): irrelevant on an installed build, where nothing gets built at all --
              shown only once this tab has been starting for 10s, not on every starting screen. */}
          {stillStarting && (
            <p className="connecting">still starting — a first start from a Verdandi checkout builds the sidecar</p>
          )}
        </>
      )}
      {/* The empty tab's dashboard (spec §7): replaces the eight resume rows that used to sit below
          the composer. Never drawn while starting or failed (those screens keep their own).
          `hello !== null` (not just `showDashboard`, which TS cannot narrow through) is what lets
          `Dashboard`'s `hello: Hello` prop take it without a non-null assertion. */}
      {hello !== null && showDashboard && (
        <Dashboard hello={hello} mode={tab.mode} cursor={dashCursor} narrow={narrow} onItem={runItem} prefix={prefix} />
      )}
      <QueueLines items={props.queue ?? []} error={props.queueError ?? null} />
      <Composer
        disabled={failed}
        sessionEnded={failed}
        closing={false}
        restoredDraft={props.restoredDraft}
        mode={mode}
        focusRequest={composerFocus}
        hintTarget={!failed}
        onModeChange={setMode}
        onSend={props.onSend}
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
      />
      {/* The which-key box (fix round 1, panel round 2 plan Task 12+13): `WHICH_KEY_DELAY_MS` after
          a leader/table sequence started above is still pending. `.empty-tab` is this screen's own
          positioned ancestor (index.css) -- there is no `.agent-ui-scroller` here (ruling 7) for it
          to dock against the way the live conversation's copy does, so it sits against this
          centred block's own bottom edge rather than the screen's; a GUI pass owes the real
          placement (Task 13's checklist, "Panel round 2"). */}
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
