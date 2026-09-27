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

export type EmptyTabProps = {
  hello: Hello | null;
  tab: TabInfo;
  handoff: HandoffCommand | null;
  failure: string | null;
  paneFocused: boolean;
  focusRequest: number;
  /** Bumped by App's `arrive` handling: lands BROWSE with the
   *  cursor on the dashboard's `New session` item, the same reversal a live tab's arrival gets. */
  arriveRequest: number;
  /** Bumped by `App`'s `[keysRequest]` effect for the empty layout. Restore focus to the composer
   *  in INPUT or the screen root in BROWSE after a pane-focus return or overlay close, keeping
   *  keys off `document.body`. */
  keysRequest: number;
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
  onResume: (providerSessionId: string) => void;
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
  /** The window's readable prefix chord from `App.tsx`'s `keymapHelp.prefix`, passed to
   *  `Dashboard`'s key hint. Defaults to Ctrl+b, matching `keymapHelp` before the keymap envelope
   *  arrives, so callers without a configured prefix still get the correct hint. */
  prefix?: string;
};

/** An empty session tab: Claude Code's fresh prompt. The composer is live in INPUT;
 *  the first send creates the session in Rust. `Shift+Tab` cycles the mode. Nothing here
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
  const showDashboard = hello !== null && !starting && !failed;
  const prefix = props.prefix ?? "Ctrl+b";
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
    if (!starting) {
      setStillStarting(false);
      return;
    }
    const timer = setTimeout(() => setStillStarting(true), 10_000);
    return () => clearTimeout(timer);
  }, [starting, tab.id]);
  /** Focus counter read by `Composer`, mirroring `App.tsx`'s `composerFocusRequest`. Only the
   *  guarded `focusRequest` and `keysRequest` effects bump it, so the composer cannot take
   *  focus from an overlay. */
  const [composerFocus, setComposerFocus] = useState(0);
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
  /* The leader/which-key engine stays local because this tab keeps its own `mode`, which
     `App.tsx` cannot see. BROWSE here gives the dashboard the keys. Reserved `g`/`z`/`[`/`]`
     prefixes support table pairs such as `[b`/`]b` for stepping tabs. Fixed `resolveKey` pairs
     such as `gg` and `[[` have no rows to act on here. */
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
  /* A request for the keys (`enter_input`, a chooser closing) must enter INPUT as well as
     request focus. Blurring `Composer` leaves this tab in BROWSE with no textarea to focus.
     Apply the same guards as `i`, and skip the request while an overlay is open so composer
     autofocus cannot take its keys. */
  useEffect(() => {
    if (props.overlayOpen) return;
    if (props.focusRequest !== focusAtMount.current && !starting && !failed) {
      setMode("input");
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
    // The leader/which-key engine also runs on starting and failed tabs so `<leader>` `mode.cycle`
    // can flash `mode is fixed ...` there. Only the dashboard menu keys below require a
    // `not_started` tab. Try the leader/which-key engine before the dashboard's fixed
    // `j`/`k`/`Enter`/letter handlers, matching `App.tsx`. Table bindings take keys not already
    // claimed above; the reserved-key list prevents collisions with fixed bindings.
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
      setDashCursor((c) => Math.max(0, Math.min(items.length - 1, c + delta)));
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      const item = items[dashCursor];
      if (item !== undefined) runItem(item);
      return;
    }
    // Bare `m` is unbound so prose such as "make" does not cycle the mode before entering INPUT.
    // The Mode item remains reachable by `j`/`k` then Enter, Shift+Tab through document capture,
    // and `<leader>m`. `DASH_ITEM_KEY.mode` is display only.
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
        <Row kind="error" sign="✗" role="alert">
          <strong>This tab's session did not start.</strong>
          <pre>{failure ?? "no reason was given"}</pre>
          <div className="row-hint">Press r to start a new session here.</div>
        </Row>
      )}
      {starting && (
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
