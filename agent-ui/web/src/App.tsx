import { SessionEndedBanner } from "./components/SessionEndedBanner";
import { TrustOverlay } from "./components/TrustOverlay";
import {
  useClampCursorToTimeline,
  usePreserveCursorRow,
  useRevealCursorRow,
  useClampCursorOnScroll,
  useRideLastRow,
  useWaitForCtrlYTarget,
  useEnterInput,
  useWatchFrozenSelection,
} from "./app/panelEffects";
import { statusWarning, contextFact } from "./app/status";
import { TOOL_RESULT_SCROLL_STEP_PX, HINT_PENDING_TIMEOUT_MS, accumulateMotionCount, jumpTarget } from "./app/motion";
import {
  TYPING_FLASH,
  COUNT_ANSWER_FLASH,
  MODIFIED_ENTER_FLASH,
  ENTER_LANDING_FLASH,
  ENTER_ELSEWHERE_FLASH,
  NO_CARD_FLASH,
  CARD_ELSEWHERE_FLASH,
  NO_WAITING_CARD_FLASH,
  STOP_TYPING_FLASH,
  CTRL_Y_TYPING_FLASH,
  CTRL_Y_TARGET_FLASH,
  CTRL_Y_YANK_FLASH,
  CTRL_Y_HELD_FLASH,
  CTRL_Y_EDITOR_CARD_FLASH,
  NOT_APPROVABLE_FLASH,
  ESC_DOES_NOT_INTERRUPT_FLASH,
  NOTHING_IN_THE_BACKGROUND,
  CLAUDE_REPORTS_NO_BACKGROUND,
  nothingToStopWords,
  stopBackgroundQuestion,
} from "./app/flashes";
import { isEditableElement, isComposerBox, isKillKey, CANCELS_WAITING_ANSWER, caretRectMissing } from "./app/elements";
import type { SlashPickerState, SettingsRequest, SettingsAnswer } from "./app/elements";
import {
  scrollVisibleRowBox,
  rowScrollStep,
  scrollKeymapOverlay,
  revealRow,
  stepWithinRow,
  clampCursorToView,
  oldestPendingTool,
  applyViewScroll,
} from "./app/rowScroll";
import type { FrozenSnapshot, ArrivalPark } from "./app/rowScroll";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { KeyboardEvent, SetStateAction } from "react";
import { ProjectDirContext } from "./projectPath";
import { initialPageModel, pageModel, withoutHint } from "./page_model";
import type { CommandRecord, PageModel } from "./page_model";
import { pruneCardViews, pruneTabEntries } from "./pageLookups";
import type { CardView } from "./pageLookups";
import { installDispatch, installRemoteDesk, postToRust, nextMirrorId, nextRequestId } from "./bridge";
import type { OutboundMessage, PanelKeysMode, PermissionDecision } from "./bridge";
import { noteUserScroll, resumeFollowing, watchUserScroll } from "./follow";
import {
  computedLineHeight,
  rowTextElement,
  settleListScroll,
} from "./jkScroll";
import { cancelHold, isHolding, keepHoldAlive, releaseHold, startHold } from "./jkHold";
import { isPlainAnswerKey, panelEntryKey, pluginKeysHelp, resolveKey } from "./keymap";
import type { KeyLike, PanelBinding, PanelMode, PendingPrefix, PluginKeysHelp } from "./keymap";
import { advanceSequence, boxEntries, FIXED_PENDING_ENTRIES, sequenceTitle, startSequence, WHICH_KEY_DELAY_MS } from "./leader";
import type { BoxEntry, SeqStep } from "./leader";
import { isImeKey } from "./composerKeys";
import { afterPaint } from "./afterPaint";
import { bypassYesCounts, isModeCycleKey, isShiftTab, KeyClock, modeFixedMessage, modeKeyRoute } from "./modeKey";
import { heldKeyName, hintTypingFlash, isModifierKey, leaderTypingFlash, tableKeyTypingFlash, TYPE_HINT_FLASH, TYPING_GUARD_MS, TypingGuard } from "./typingGuard";
import {
  caretModelOf,
  compareCarets,
  copySelectionText,
  entrySelectableCaret,
  firstSelectableCaret,
  rebuildSelection,
  repeatMotion,
  revealCaret,
  selectionMatchesBuild,
  swapEnds,
} from "./visual";
import type { BuiltSelection, Caret, SelectionLike, VisualModel } from "./visual";
import { appendQuote, formatQuote } from "./quote";
import { installCtrlBracketAsEscape } from "./ctrlBracket";
import { installHeldSuperTracking } from "./heldSuper";
import { WhichKeyBox } from "./components/WhichKeyBox";
import { TurnChanges } from "./components/TurnChanges";
import { allCards, buildTimeline, oldestPendingPermission, oldestWaitingPermission, promptIndex, waitingCardAfter } from "./timeline";
import { buildDisplay, indexOfKey, runKeyOf } from "./display";
import { outputText, primaryText } from "./copyText";
import { codeBlockText, renderedText } from "./markdown";
import { countCodePoints } from "./toolRegistry";
import { findMatch } from "./search";
import { SearchBar } from "./components/SearchBar";
import {
  assignLabels,
  controlsOf,
  conversationRows,
  countedStop,
  currentStop,
  hintTargets,
  isActivatableControl,
  linkClickNeedsLook,
  linkLook,
  LINK_LOOK_MAX_MS,
  linkOpensAtOnce,
  nextControl,
  permissionTarget,
  rowIndexOf,
  rowOf,
  stopOf,
  webLinks,
  HINT_ALPHABET,
} from "./nav";
import type { ShownLink } from "./nav";
import type { AnswerableItem, HintTarget } from "./nav";
import type { TimelineItem } from "./timeline";
import { parsePath, pathsIn, pathsInText, viewText } from "./paths";
import { pathAtCaret, pathRanges, pathSelectionOffsets } from "./pathTargets";
import type { PathTarget } from "./pathTargets";
import { proposalOf } from "./proposal";
import type { PathRef } from "./paths";
import { PathPick } from "./components/PathPick";
import { LinkPick } from "./components/LinkPick";
import { isExternalTool, agentTitle } from "./externalTools";
import { acceptsEnvelope, activeTabInfo, agentWord, countedTabTarget, exportView, forgetClosed, importView, isAcpAgent, isExternalAgent, saveView, tabPermissionModes, takeView } from "./tabs";
import type { TabViewState } from "./tabs";
import { EmptyTab } from "./components/EmptyTab";
import type { NavKeyRequest } from "./components/EmptyTab";
import { QuestionDock } from "./components/QuestionDock";
import type { QuestionDockHandle } from "./components/QuestionDock";
import { Composer } from "./components/Composer";
import type { RestoredDraft } from "./components/Composer";
import { TabBar } from "./components/TabBar";
import { MessageList } from "./components/MessageList";
import { cardApprovable, PermissionCardContext } from "./components/PermissionCard";
import { classify } from "./problems";
import { revealedPathText, revealedText } from "./revealHidden";
import { lookOnScreen } from "./lookOnScreen";
import { cliPhaseText } from "./turnPhase";
import { ActivityLine } from "./components/ActivityLine";
import { StatusBand } from "./components/StatusBand";
import { QueueLines } from "./components/QueueLines";
import { heldCount } from "./queueLines";
import { DetailPopover } from "./components/DetailPopover";
import { ContinueInTerminal } from "./components/TerminalHandoff";
import { HintLayer } from "./components/HintLayer";
import type { ShownHint } from "./components/HintLayer";
import { KeymapOverlay } from "./components/KeymapOverlay";
import { ReviewOverlay } from "./components/ReviewOverlay";
import { TasksOverlay, stopTaskWords, type TasksOverlayHandle } from "./components/TasksOverlay";
import {
  parseExCommand,
  resolveTrustKey,
  scrollTarget,
  TRUST_BAND_PROMPT,
  TRUST_BAND_PROMPT_N_ONLY,
  TRUST_WAIT_FLASH,
  trustFooter,
} from "./trust";
import type { TrustKeyEvent } from "./trust";
import {
  applyReviewKey,
  boxUnderCursor,
  openReview,
  resolveReviewKey,
  scrollBoxFirst,
  typeComment,
} from "./review";
import type { ReviewEffect } from "./review";
import { THINKING_DISPLAYS } from "./types";
import { Chooser } from "./components/Chooser";
import { SlashPicker } from "./components/SlashPicker";
import type { SlashPickerKind } from "./components/SlashPicker";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import { RemoteEnroll } from "./RemoteEnroll";
import { DeviceList } from "./DeviceList";
import { RemoteDesk } from "./remoteDesk";
import type { SlashAction } from "./slashCommands";
import { barePickerCommand, completionSource, settingCommand } from "./slashCommands";
import { EFFORT_FROM_SETTINGS, effortPicker, modelPicker, parseEffortReply, parseModelReply, settingsChangedWords } from "./slashPicker";
import { DRAFT_MIRROR_DELAY_MS, modePill, REFUSED_ECHO_MS, showTabBar } from "./tabs";
import { cardSummary, liveTasks, shortModel, usageSegment } from "./band";
import { NOTHING_TO_STOP, STOP_TURN_QUESTION, stopAskFor, stopTurnAnswer, workToStop, type StopWork } from "./stopAsk";
import { latestTurnEnding } from "./turnEnding";
import type { ApproveFact } from "./band";
import type { DetailRow, TabId, TabsEnvelope } from "./types";
import { applyTheme } from "./theme";
import { applyEditorTyping } from "./typingCadence";
import { copyToPrimary } from "./primaryCopy";

/** The band's transient line (`showFlash`). */
type FlashLine = { text: string; seq: number; look?: string; path?: string; toPrompt?: boolean };

export default function App() {
  /** Everything the envelope router decides (`./page_model`). `pageRef` is the latest model, written
   *  synchronously with every change, so envelopes later in the same dispatch batch -- and the handlers that
   *  read the model between renders -- never see the previous render's value. */
  const [page, setPage] = useState(initialPageModel);
  const pageRef = useRef(page);
  const setPageField = useCallback(<K extends keyof PageModel,>(field: K, value: SetStateAction<PageModel[K]>) => {
    const current = pageRef.current;
    const nextValue = typeof value === "function"
      ? (value as (previous: PageModel[K]) => PageModel[K])(current[field]) : value;
    if (Object.is(current[field], nextValue)) return;
    const next = { ...current, [field]: nextValue };
    pageRef.current = next;
    setPage(next);
  }, []);
  const {
    state, hello, tabs, fatalError, handoff, handoffRequests, commandNotice, rootNote, turnClock, turnDurations, thinkingDisplay, paneFocused,
    queue, queueError, queueNote, queueSteers, shellRunning, history, ruleOffers, editorContext, editorLink, scratchEditing,
    review, reviewHints, reviewRecovery, reviewSettings, chooser,
    inputRequest, arriveRequest, permissionRequest, navKey, keymapHelp, pages, heldTrust, confirm, renaming, queueTaken,
  } = page;
  const {
    setFatalError, setHandoffRequests, setCommandNotice, setRootNote, setReview, setReviewHints, setChooser,
    setInputRequest, setArriveRequest, setPermissionRequest, setHeldTrust, setConfirm, setRenaming,
  } = useMemo(() => ({
    setFatalError: (value: SetStateAction<PageModel["fatalError"]>) => setPageField("fatalError", value),
    setHandoffRequests: (value: SetStateAction<PageModel["handoffRequests"]>) => setPageField("handoffRequests", value),
    setCommandNotice: (value: SetStateAction<PageModel["commandNotice"]>) => setPageField("commandNotice", value),
    setRootNote: (value: SetStateAction<PageModel["rootNote"]>) => setPageField("rootNote", value),
    setReview: (value: SetStateAction<PageModel["review"]>) => setPageField("review", value),
    setReviewHints: (value: SetStateAction<PageModel["reviewHints"]>) => setPageField("reviewHints", value),
    setChooser: (value: SetStateAction<PageModel["chooser"]>) => setPageField("chooser", value),
    setInputRequest: (value: SetStateAction<PageModel["inputRequest"]>) => setPageField("inputRequest", value),
    setArriveRequest: (value: SetStateAction<PageModel["arriveRequest"]>) => setPageField("arriveRequest", value),
    setPermissionRequest: (value: SetStateAction<PageModel["permissionRequest"]>) => setPageField("permissionRequest", value),
    setHeldTrust: (value: SetStateAction<PageModel["heldTrust"]>) => setPageField("heldTrust", value),
    setConfirm: (value: SetStateAction<PageModel["confirm"]>) => setPageField("confirm", value),
    setRenaming: (value: SetStateAction<PageModel["renaming"]>) => setPageField("renaming", value),
  }), [setPageField]);
  /** Whether `interaction` still waits for an answer in `tab`, judged from the model as the dispatch left it (ahead
   *  of the render the dock was drawn in). A closed tab's questions are gone with it. A tab switched away from is no
   *  longer this page's state, so its question is taken to wait: the host keeps the draft only if it does. */
  function questionStillWaiting(tab: TabId, interaction: string): () => boolean {
    return () => {
      const page = pageRef.current;
      if (!page.tabs?.tabs.some((entry) => entry.id === tab)) return false;
      if (page.tabs.active !== tab) return true;
      return page.state.questions.some((q) => q.interactionId === interaction && q.state.kind !== "settled");
    };
  }
  /** Records an outgoing request in the model without a render: nothing draws `inFlight`, and a render of the
   *  whole panel per outgoing command would be pure cost. The next change of the model carries it along, since
   *  every change starts from `pageRef`. */
  function trackRequest(id: string, record: CommandRecord) {
    const current = pageRef.current;
    pageRef.current = { ...current, inFlight: new Map(current.inFlight).set(id, record) };
  }

  /** The active tab for the dispatch handler, which is installed once: set synchronously inside the handler, so
   *  the snapshot that follows a `tabs` envelope in the same batch is accepted (`tabs` from the render would
   *  still hold the previous render's value until React re-renders). */
  const activeTabRef = useRef<TabId | null>(null);
  /** What each tab's reader had on screen (cursor, expansions, scroll, draft), keyed by tab id and never sent to
   *  Rust. Saved just before a switch resets the render state, and handed back through `restoreRef` once the new
   *  tab's own `snapshot` arrives. */
  const viewStore = useRef(new Map<TabId, TabViewState>());
  /** An arriving tab keeps its moved cursor even when its scroll follows the end. */
  const arrivedTabsRef = useRef(new Set<TabId>());
  /** The view to restore once the switch's `snapshot` is applied; read and cleared by the `snapshot` arm and the
   *  scroll-restoring layout effect. */
  const restoreRef = useRef<TabViewState | null>(null);
  /** See `ArrivalPark`. Taken by the `pane_focus` arm when the keys leave, used and cleared by the next
   *  `arrive`, dropped by anything that means the reader chose a place themselves while away (a wheel or touch on
   *  the list, a press in the panel) or by a route that lands elsewhere (a switch, `enter_input`,
   *  `focus_permission`). */
  const arrivalParkRef = useRef<ArrivalPark | null>(null);
  /** The typing guard's key count when an `arrive` or `focus_permission` envelope reached this page, held until
   *  the landing it asked for has run. Both landings happen in effects, so only after React has rendered the
   *  envelope; `shell` moves GTK focus here and sends the envelope at the same moment, so the user's first key can
   *  be handled in between, against the panel as it is drawn. A landing that ran over that key afterwards undid
   *  it -- the first `v` after `Ctrl+l` started CARET and the landing ended it again before it was ever painted,
   *  so only a second `v` seemed to work. A landing whose count no longer matches yields: the key that got there
   *  first already decided. */
  const landingAtKeyRef = useRef<number | null>(null);
  /** Set by the `tabs` arm whenever the active tab changes (the first `tabs` included), and read and cleared by
   *  the `snapshot` arm right after: landing on a tab already holding a card puts the keys on it, the same as
   *  `Ctrl+l` does within one tab. */
  const switchedRef = useRef(false);
  const activeTab = activeTabInfo(tabs);
  /** The conversation for handlers that run between renders; dispatch writes it with the model. */
  const stateRef = useRef(state);
  stateRef.current = state;
  /** True once the active tab actually holds a session, live or already ended: a `starting` tab, or a `failed`
   *  one with no kept conversation, renders the empty tab. Derived, not state -- Rust's `tabs` envelope is the
   *  only source of a tab's `state`. */
  const sessionStarted =
    activeTab !== null &&
    (activeTab.state === "live" ||
      activeTab.state === "ended" ||
      (activeTab.state === "failed" && page.keptStates.has(activeTab.id)));
  /** `sessionStarted` for the bridge listener, which is registered once and would otherwise read
   *  its first render's value forever. Assigned during render, so it is current by the time any
   *  later envelope arrives. */
  const sessionStartedRef = useRef(false);
  /** Whether `<leader>` `mode.cycle` (and, by the same rule, Shift+Tab) would flash rather than post
   *  -- `modeKeyRoute`'s own routing (the `SetPermissionMode` capability this used to
   *  read is gone; only an `ended`/`failed` tab already in bypass still cycles, to leave it). */
  const modeFixed =
    modeKeyRoute({
      confirmOpen: false,
      chooserOpen: false,
      tabState: activeTab?.state ?? null,
      tabMode: activeTab?.mode ?? null,
    }) === "fixed";
  sessionStartedRef.current = sessionStarted;
  /** Text to put back in the composer after a refused send. See `RestoredDraft`. */
  const [restoredDraft, setRestoredDraft] = useState<RestoredDraft | null>(null);
  /** Monotonic, so two refusals of the same text are two distinct restores. */
  const restoreSeq = useRef(0);
  /** Bumped only when the `snapshot` arm actually applies a saved view's cursor and expansions. The
   *  scroll-restoring layout effect keys on THIS, not on `state`: `state` also changes on the switch's own reset,
   *  in the same render as `restoreRef.current` is first set, and a `[state]` dependency fired there too --
   *  before the new tab's real snapshot ever arrived -- consuming `restoreRef.current` on an empty transcript and
   *  leaving nothing for the real restore. The "keeps each tab's cursor across a switch" test fails with that
   *  dependency. */
  const [restoreTick, setRestoreTick] = useState(0);
  // Spinner-only, per-requestId in-flight tracking -- never read to answer "is a turn in
  // progress" or "is this permission still pending" (those come only from canonical
  // state.activeTurnId / state.pendingPermissions).
  const [, setPendingCommands] = useState<Set<string>>(new Set());
  /** When the payload carrying a turn's first assistant text was RECEIVED, for the render trace.
   *  A ref, not state: writing it must not itself cause a render, which would be the thing being
   *  measured. Null except in the window between that payload arriving and its frame being drawn. */
  const firstTextReceivedAt = useRef<number | null>(null);
  /** Guards the render report to one per turn. Without it the effect below re-arms on every one of
   *  a reply's ~400 deltas. */
  const renderReportSent = useRef(false);
  /** Which timeline rows show their full tool result rather than the folded placeholder, keyed by
   *  the timeline `key` -- never the cursor index, because a resolved permission removes a card and
   *  shifts every later index, which would silently move an expansion onto a different row.
   *  Toggled by `Enter` on the row under the cursor, in `onKeyDown` below. */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  /** `Ctrl+o`: every result shown in full, wider cuts, no collapsed runs. Per tab, like
   *  `expanded` above -- saved and restored across a switch in `TabViewState.detailed`, reset to
   *  `false` for a tab with no saved view (a fresh tab never starts detailed). */
  const [detailed, setDetailed] = useState(false);
  /** BROWSE (read, move the cursor, act on "this item") / INPUT (typing into the composer) / HINT
   *  (declared for the status line's mode block -- nothing reaches it yet). See
   *  `./keymap`'s own doc comment on `PanelMode`. */
  const [mode, setMode] = useState<PanelMode>("browse");
  const questionDock = useRef<QuestionDockHandle>(null);
  const waitingQuestions = state.questions.filter((q) => q.state.kind !== "settled").sort((a, b) => a.seq - b.seq);
  const waitingQuestion = waitingQuestions[0];
  const questionIdentity = waitingQuestion ? `${activeTab?.id}:${waitingQuestion.interactionId}` : null;
  const questionArrival = useRef({ identity: null as string | null, at: 0, held: new Set<string>() });
  const questionHeld = useRef(new Set<string>());
  /** Whether the key being handled belongs to an unbroken run that began at an arrival (`noteArrival`): each key
   *  of it pressed within `TYPING_GUARD_MS` of the key or arrival before it. The question dock answers from keys
   *  this page's typing guard never sees, so it asks this instead. Judged for every keydown by the document-capture
   *  listener that dates keys (`onModeKey`); a pointer press ends it, so a click is judged by the dock's own guard. */
  const arrivalRunRef = useRef(false);
  useEffect(() => {
    const down = (e: globalThis.KeyboardEvent) => { questionHeld.current.add(e.code || e.key); };
    const up = (e: globalThis.KeyboardEvent) => {
      questionHeld.current.delete(e.code || e.key);
      questionArrival.current.held.delete(e.code || e.key);
    };
    const blur = () => { questionHeld.current.clear(); questionArrival.current.held.clear(); };
    const pointer = () => {
      arrivalRunRef.current = false;
      if (editReturnRef.current?.resumed) editReturnRef.current = null;
    };
    window.addEventListener("keydown", down, true);
    window.addEventListener("keyup", up, true);
    window.addEventListener("blur", blur);
    window.addEventListener("pointerdown", pointer, true);
    return () => {
      window.removeEventListener("keydown", down, true);
      window.removeEventListener("keyup", up, true);
      window.removeEventListener("blur", blur);
      window.removeEventListener("pointerdown", pointer, true);
    };
  }, []);
  useLayoutEffect(() => {
    const composer = containerRef.current?.querySelector<HTMLElement>(".composer");
    if (composer) composer.hidden = questionIdentity !== null;
    const previous = questionArrival.current.identity;
    if (previous === questionIdentity) return;
    questionArrival.current = { identity: questionIdentity, at: performance.now(), held: new Set(questionHeld.current) };
    if (questionIdentity !== null && mode === "input") questionDock.current?.focus();
    else if (previous !== null && questionIdentity === null && mode === "input") setComposerFocusRequest((n) => n + 1);
  }, [questionIdentity, sessionStarted]);
  function questionBlocked() {
    return performance.now() - questionArrival.current.at < 250 || questionArrival.current.held.size > 0 || arrivalRunRef.current;
  }
  /** A held `j`/`k`'s glide (`./jkHold`): the last repeat's time on the keydowns' clock and on
   *  `performance.now()`'s, to date its release for the typing guard, and which way it goes. */
  const holdClockRef = useRef({ typed: 0, perf: 0 });
  const holdDirectionRef = useRef<1 | -1>(1);
  /** `paneFocused` for the `snapshot` dispatch arm, which cannot read render state directly -- the same reason
   *  `sessionStartedRef` exists. A switch landing on a card must check whether the keys are even in this window
   *  right now: landing them silently in a background window would be a surprise nobody asked for. */
  const paneFocusedRef = useRef(false);
  paneFocusedRef.current = paneFocused;
  /** What actually reaches `EmptyTab`'s own `navKeyRequest` prop: only a `navKey` the effect below
   *  found no overlay (`overlayOpen`, `?`, a y/n) already owning the keys for -- EmptyTab has no
   *  visibility into `keymapOpen`/`confirm` at all (they are this component's own state, drawn over
   *  its layout rather than passed down), so forwarding `navKey` itself unfiltered would let it act
   *  on a request that has already been answered with `nav_fallthrough` above it. */
  const [emptyNavKey, setEmptyNavKey] = useState<NavKeyRequest | null>(null);
  /** Where an empty tab lands when it mounts or is switched to (`EmptyTab`'s `landing`; GUI pass
   *  2026-09-26): the last of `enter_input` (INPUT), `arrive` or a tab switch (BROWSE).
   *  Launch starts INPUT, as it always has. */
  const [emptyLanding, setEmptyLanding] = useState<PanelMode>("input");
  /** Bumped by `dropPendingKeys()`, so a cancel route drops the empty tab's own
   *  waiting prefix and leader sequence too (`EmptyTab`'s `dropKeysRequest`), not only this
   *  component's refs. A counter for the reason `arriveRequest` is one. */
  const [emptyDropKeys, setEmptyDropKeys] = useState(0);
  /** The empty tab's own mode, reported by `EmptyTab`, for the band drawn under it. */
  const [emptyMode, setEmptyMode] = useState<PanelMode>("input");
  /** Whether keyboard focus is on a stop that is NOT a conversation row -- a banner's Dismiss, the
   *  status line's Stop, the handoff button (`./nav`). While it is, the row cursor is drawn hollow,
   *  the same way it is when another pane has the keys: it is where `k` brings you back to, and it
   *  is not what the keys act on now. Derived from DOM focus (`onFocus` on the root), never set by
   *  the key handler alone, so a click or a `Tab` onto one of those buttons agrees with `j`/`k`. */
  const [edgeFocused, setEdgeFocused] = useState(false);
  /** The index into `timeline` that `j`/`k` move and `Enter`/`y` act on. */
  const [cursor, setCursor] = useState(0);
  /** `cursor`/`mode`/`expanded`/the composer's draft, mirrored into refs for the `tabs` dispatch
   *  handler (installed once), which cannot read render state directly --
   *  the same reason `sessionStartedRef` exists above. Assigned during render, so a switch always
   *  saves what the CURRENT render actually shows, not a stale one from before the last update. */
  const cursorRef = useRef(cursor);
  cursorRef.current = cursor;
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const expandedRef = useRef(expanded);
  expandedRef.current = expanded;
  const detailedRef = useRef(detailed);
  detailedRef.current = detailed;
  /** What the reader has on screen now, from the live refs: what a tab switch parks and what leaving
   *  the pane parks, one capture for both. `atBottom` is the tab switch's own formula. */
  function captureView(list: HTMLElement | null): TabViewState {
    const atBottom = list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1;
    return {
      cursor: cursorRef.current,
      mode: modeRef.current,
      expanded: expandedRef.current,
      scrollTop: list?.scrollTop ?? 0,
      atBottom,
      detailed: detailedRef.current,
      // At the bottom, following is what a restore should resume (see the `snapshot` arm's restore
      // block), so no threshold is worth remembering -- and `null` there matches `MessageList`'s own
      // "while following" reset.
      unseenAfterSeq: atBottom ? null : unseenAfterSeqRef.current,
    };
  }
  /** The model VISUAL/V-LINE keep between keys (the two caret ends, the
   *  linewise flag, the goal column), and the native selection's own snapshot from the last
   *  rebuild (the "still the one VISUAL built" check). `null` outside VISUAL/V-LINE -- `mode`
   *  itself is what the rest of this file reads to know whether it is on. */
  const visualModelRef = useRef<VisualModel | null>(null);
  const visualBuiltRef = useRef<BuiltSelection | null>(null);
  /** The count VISUAL/V-LINE's own digits accumulate, applied to the next motion and reset by
   *  it -- `countRef`'s own twin, kept separate because BROWSE's `onKeyDown` (which owns `countRef`)
   *  never runs for a VISUAL key (the capture-phase handler stops its propagation), so sharing
   *  one ref between the two would leave whichever mode did not just run holding a stale value. */
  const visualCountRef = useRef<number | null>(null);
  /** The region's local pending `g` (`gg`/`G`, and the reserved `gv`) --
   *  `visualCountRef`'s own twin, kept separate from BROWSE's `pendingRef` for the identical reason:
   *  the capture-phase region handler never lets a key reach `onKeyDown`, which owns `pendingRef`.
   *  Reuses `PendingPrefix`'s `"g"` value purely as a carrier through `resolveKey`'s `ctx.pending`;
   *  the region never arms `"z"`/`"["`/`"]"`, which stay BROWSE-only. */
  const regionPendingGRef = useRef<PendingPrefix | null>(null);
  /** The props `MessageList` is frozen to while VISUAL is on -- see `FrozenSnapshot`'s own doc
   *  comment. State, not a ref: `MessageList`'s own render depends on it. */
  const [frozenSnapshot, setFrozenSnapshot] = useState<FrozenSnapshot | null>(null);
  /** `frozenSnapshot` as of the last render, i.e. whether the DOM on screen right now is the frozen
   *  list. Read by the handlers and effects that turn the LIVE
   *  cursor into a DOM row -- the `[cursor]` reveal, the scroll clamp, `focus_permission`'s own
   *  reveal -- none of which may do so while the list on screen is the frozen one: the live cursor
   *  indexes the live timeline, and the two differ once a row arrives (a queued prompt sent at a
   *  turn's end, a linked card). Assigned during render, like `cursorRefForScroll`, so it describes
   *  the DOM the commit that follows puts on screen -- `exitRegion`'s own `setFrozenSnapshot(null)`
   *  does not clear it until the thawed list has actually rendered. */
  const frozenSnapshotRef = useRef<FrozenSnapshot | null>(null);
  frozenSnapshotRef.current = frozenSnapshot;
  /** A `focus_permission` reveal waiting for the thawed list (see its layout effect, below the
   *  `permissionRequest` effect). */
  const revealAfterThawRef = useRef<number | null>(null);
  /** The composer's own unsent text, kept only so a tab switch can save it -- never
   *  read to render anything itself; `restoredDraft` is what actually reaches `Composer`. Updated
   *  from `Composer`/`EmptyTab`'s `onDraftChange`, which fires on every keystroke and once more
   *  with `""` after a send. */
  const draftRef = useRef("");
  /** The cards this tab has answered from this panel, by `permissionId`,
   *  whichever way -- a card's own button or `a`/`d`. This is the card's old `answered` guard lifted
   *  out of `PermissionCard`, because `a`/`d` no longer press the card's button (a DOM query for it
   *  could find a button a model reply drew): they call `answerPermission` with the card's id, and
   *  the double-answer protection has to hold on that path too. `answeredRef` is the same set,
   *  current between renders, for the check `answerPermission` makes and for a wait `a`/`d` are
   *  still in. Tab-scoped like `ruleOffers`: reset on a switch, and an id leaves it once its card
   *  is no longer pending (the effect after `answerableItems`). */
  const [answeredPermissions, setAnsweredPermissions] = useState<ReadonlySet<string>>(() => new Set());
  const answeredRef = useRef<ReadonlySet<string>>(answeredPermissions);
  /** The card INPUT's `Ctrl+y` would approve, as `<tab>:<permission id>`,
   *  and whether it has been that card for `TYPING_GUARD_MS` (the layout effect after
   *  `ctrlYTargetKey`). */
  const ctrlYTargetRef = useRef<{ key: string | null; settled: boolean }>({ key: null, settled: false });
  /** Whether the last non-modifier key the panel saw was a kill in the
   *  composer (`isKillKey`), so a `Ctrl+y` after it is readline's yank whatever the pause. */
  const lastKeyWasKillRef = useRef(false);
  /** Whether the composer's own textarea has focus -- what the band's
   *  `Ctrl+y` segment and `approveOldestByKey` both require. Followed through `focusin`/`focusout`. */
  const [composerBoxFocused, setComposerBoxFocused] = useState(false);
  useEffect(() => {
    const onIn = (event: FocusEvent) => setComposerBoxFocused(isComposerBox(event.target) || (event.target instanceof Element && event.target.closest(".question-dock") !== null));
    const onOut = (event: FocusEvent) => setComposerBoxFocused(isComposerBox(event.relatedTarget) || (event.relatedTarget instanceof Element && event.relatedTarget.closest(".question-dock") !== null));
    document.addEventListener("focusin", onIn);
    document.addEventListener("focusout", onOut);
    setComposerBoxFocused(isComposerBox(document.activeElement));
    return () => {
      document.removeEventListener("focusin", onIn);
      document.removeEventListener("focusout", onOut);
    };
  }, []);
  /** The reason typed so far into each pending card's box, reported by the card as it changes, so
   *  `d` sends it exactly as the card's own Deny does. A ref, not state: typing in the
   *  box must not re-render the whole panel. */
  const permissionReasons = useRef(new Map<string, string>());
  // Companion mode: where the panel stands with the editor beside it. `null` is the one-window mode,
  // where Rust never sends one.
  /** The unread pill, reported up by `MessageList` rather than floated over
   *  the last line by a `NewPill` this component owned itself -- see `MessageList`'s own
   *  `onUnreadChange` doc comment. `jump` starts as a no-op so the band never has to guard a click
   *  that lands before the first `updatePill` (mount, before any scroll).
   *
   *  Only the label is state: `MessageList` reports on every scroll event and every update, and a
   *  fresh `jump` closure each time made every report a state change, so the whole panel rendered
   *  again on each scroll frame with nothing on screen changed. The latest `jump` is kept in a ref. */
  const [unreadLabel, setUnreadLabelState] = useState<string | null>(null);
  /** The label last set, compared before setting: a state setter handed its current value still
   *  renders the panel once more when it is called from a layout effect, as `MessageList`'s is on
   *  every update. */
  const unreadLabelRef = useRef<string | null>(null);
  const setUnreadLabel = (label: string | null) => {
    if (label === unreadLabelRef.current) return;
    unreadLabelRef.current = label;
    setUnreadLabelState(label);
  };
  const unreadJumpRef = useRef<() => void>(() => {});
  /** The unread pill's current `seq` threshold, mirrored from `MessageList`'s `onUnreadChange` third
   *  argument -- read (not `state`) when a switch saves the departing tab's view, so
   *  a row that arrives while a tab is away is still counted once the reader comes back to it. A ref,
   *  not state: nothing here ever renders on it, the same reason `cursorRef`/`modeRef`/etc. above are
   *  refs. */
  const unseenAfterSeqRef = useRef<number | null>(null);
  /** What to seed `MessageList`'s own threshold with on a restore: `tick` changes on
   *  every restore that has something to seed, so a second switch back to the same threshold still
   *  re-fires `MessageList`'s seed effect -- see that prop's own doc comment. `null` between restores
   *  and for a tab that was left following or never had a saved threshold. */
  const [unseenSeed, setUnseenSeed] = useState<{ afterSeq: number; tick: number } | null>(null);
  const unseenSeedTick = useRef(0);
  const [tasksOpen, setTasksOpen] = useState(false);
  const [tasksWorkflows, setTasksWorkflows] = useState(false);
  const [tasksInitialId, setTasksInitialId] = useState<string | null>(null);
  const tasksRef = useRef<TasksOverlayHandle>(null);
  const taskRequests = useRef(new Map<string, TabId>());
  const [atPathsByTab, setAtPathsByTab] = useState<Map<TabId, { items: string[]; seq: number }>>(() => new Map());
  const atPaths = activeTab === null ? null : atPathsByTab.get(activeTab.id) ?? null;
  const atLastAsked = useRef(new Map<TabId, number>());
  const settingsRequests = useRef(new Map<string, SettingsRequest>());
  /** How many times `Ctrl+g` has viewed each broker card's proposal, by card id: the next press shows the next file. */
  const cardViews = useRef(new Map<string, CardView>());
  const taskScratchTab = useRef<TabId | null>(null);
  // Only a locally requested draft/answer edit resumes typing. Row views and later landings keep their modes.
  const editReturnRef = useRef<{
    tab: TabId;
    requestId: string;
    question?: { identity: string; index: number };
    started: boolean;
    finished: boolean;
    resumed?: boolean;
    landingHandled?: boolean;
  } | null>(null);
  const [emptyEditReturnRequest, setEmptyEditReturnRequest] = useState(0);
  const stopTasksSignature = useRef("");
  /** The footer's transient line. `seq` so the same text twice is two flashes. `look`: the part the band must show
   *  whole before the action it names may follow (`BandFacts.messageLook`); `path`: the path it ends with, as drawn
   *  (`BandFacts.messagePath`); `toPrompt`: the trust question's own answer to a key, drawn after its prompt. */
  const [flash, setFlash] = useState<FlashLine | null>(null);
  const flashSeq = useRef(0);
  /** The reply link whose address the last click showed instead of opening (`linkClickNeedsLook`), armed: set only
   *  while the band measures its host on screen in the flash `heldLinkRef` names, its look starting when it last
   *  came into view; cleared the moment it is measured hidden or that flash changes or goes. */
  const shownLinkRef = useRef<ShownLink | null>(null);
  /** The link whose address the band is showing: the link, the flash's text and instance (`seq`), and the part that
   *  must be seen (`look`, the host). Set by the first click, kept while that flash is up, armed or not. */
  const heldLinkRef = useRef<{ link: Omit<ShownLink, "at">; text: string; seq: number; look: string } | null>(null);
  /** The flash on screen now, for the measurement callbacks and the click, which run outside a render. */
  const flashRef = useRef<FlashLine | null>(null);
  flashRef.current = flash;
  /** The element the band draws its flash in (on its row or above it), for the check at the moment of opening. */
  const bandMessageRef = useRef<HTMLElement | null>(null);
  function showFlash(text: string, opts: { look?: string; path?: string; toPrompt?: boolean } = {}): number {
    flashSeq.current += 1;
    setFlash({ text, seq: flashSeq.current, ...opts });
    return flashSeq.current;
  }
  /** `StatusBand` measured the link flash: on screen, the second click on that link may open it, from a look that
   *  starts now if it was not armed already; hidden (resized, covered, scrolled, clipped), it is disarmed, and the look
   *  starts over when it is next measured on screen. Any other flash arms nothing. */
  const onLookMeasured = (text: string, visible: boolean) => {
    const held = heldLinkRef.current;
    if (held === null || held.text !== text || flashRef.current?.seq !== held.seq) return;
    if (!visible) shownLinkRef.current = null;
    else if (shownLinkRef.current === null) shownLinkRef.current = { ...held.link, at: Date.now() };
  };
  // A link is armed only by the flash instance that showed its address: that flash replaced or gone disarms it.
  useEffect(() => {
    const held = heldLinkRef.current;
    if (held !== null && (flash === null || flash.seq !== held.seq)) {
      heldLinkRef.current = null;
      shownLinkRef.current = null;
    }
  }, [flash]);
  useEffect(() => {
    if (state.notification?.content) showFlash(revealedText(state.notification.content));
  }, [activeTab?.id, state.notification]);
  useEffect(() => { setTasksOpen(false); }, [activeTab?.id]);
  // Whether Super is held, from its own keydown/keyup: WebKitGTK reports it on no other key's event,
  // and `isPlainAnswerKey` reads this for `a`/`d`/`D` and the bypass `y` (heldSuper.ts). The first
  // effect in this component, so its window-capture listener runs before HINT's, which stops keys.
  // `Ctrl+[` is Esc everywhere in the panel (ctrlBracket.ts). Its document-capture listener is
  // registered here too, so it runs ahead of `onModeKey`'s below and every handler reads a plain
  // Escape; a window-capture listener still runs before it, so HINT's own swallow wins while pending.
  useEffect(() => {
    const removeHeldSuper = installHeldSuperTracking(document, window);
    const removeCtrlBracket = installCtrlBracketAsEscape(document);
    return () => {
      removeHeldSuper();
      removeCtrlBracket();
    };
  }, []);
  // A newer flash replaces an older one (`showFlash` above); this only ever clears the flash that
  // is STILL the current one when its own two seconds are up, so an older flash's timer firing late
  // cannot erase a newer flash that has since taken its place.
  useEffect(() => {
    if (flash === null) return;
    const seq = flash.seq;
    // A link's address stays as long as its second click may come (`LINK_LOOK_MAX_MS`): the click opens only while
    // the flash that showed the address is still up.
    const timer = setTimeout(() => setFlash((f) => (f?.seq === seq ? null : f)), flash.look !== undefined ? LINK_LOOK_MAX_MS : 2000);
    return () => clearTimeout(timer);
  }, [flash]);
  // Where the envelopes above are read: `queue`, `history`, `scratchEditing` and `queueTaken` by the
  // composer and the queue, `queueError` (the queue's own error line), `editorContext` (the
  // editor-context line) and `flash` (the footer's transient line) by the band. `ruleOffers` (a
  // card's third button) is read by `MessageList` -- see its own prop just below.

  /** The row just yanked flashes for 400 ms (codecompanion's yank flash). */
  const [yanked, setYanked] = useState<{ key: string; seq: number } | null>(null);
  const yankSeq = useRef(0);
  useEffect(() => {
    if (yanked === null) return;
    const seq = yanked.seq;
    const timer = setTimeout(() => setYanked((y) => (y?.seq === seq ? null : y)), 400);
    return () => clearTimeout(timer);
  }, [yanked]);
  function copied(text: string, key: string | undefined) {
    void navigator.clipboard?.writeText(text);
    copyToPrimary(text);
    showFlash(`copied ${countCodePoints(text)} chars`);
    if (key !== undefined) {
      yankSeq.current += 1;
      setYanked({ key, seq: yankSeq.current });
    }
  }

  /** The box's text reaches Rust 300 ms after the last change, and at once when the tab
   *  stops being active. `pendingDraftRef` holds what has not been posted yet and for which tab. */
  const pendingDraftRef = useRef<{ tab: TabId; text: string } | null>(null);
  /** A refused send this page has put back into the box itself (`tab`, its `requestId`, and when). The refusal
   *  of a first message put off by the trust question is followed by Rust's `draft` naming that send
   *  (`refused`), its copy of the box with the message in front; that echo must not replace a box that already
   *  holds the message and what was typed after it. Forgotten once the mirror it goes with is delivered. */
  const refusedEchoRef = useRef<{ tab: TabId; requestId: string; at: number } | null>(null);
  const draftTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The last copy of the box Rust gave this page, and for which tab: a `late` draft replaces the box only
   *  while it still holds exactly this, since anything typed since is newer than the late copy. */
  const givenDraftRef = useRef<{ tab: TabId; text: string } | null>(null);
  function flushDraft() {
    refusedEchoRef.current = null;
    if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    draftTimerRef.current = null;
    const pending = pendingDraftRef.current;
    pendingDraftRef.current = null;
    if (pending !== null) postToRust({ type: "draft", request_id: nextMirrorId(), tab: pending.tab, text: pending.text });
  }
  /** Whether a `draft` for `tab` is the echo of the refusal just recorded: Rust names the refused send in it
   *  (`refused`), which only the trust question's put-off does. Taken once; an echo that never comes expires
   *  (`REFUSED_ECHO_MS`, or when the mirror it follows goes out). */
  function takeRefusedEcho(tab: TabId, refused: string | undefined): boolean {
    const echo = refusedEchoRef.current;
    refusedEchoRef.current = null;
    return (
      echo !== null &&
      refused !== undefined &&
      echo.tab === tab &&
      echo.requestId === refused &&
      performance.now() - echo.at < REFUSED_ECHO_MS
    );
  }
  function mirrorDraft(text: string) {
    draftRef.current = text;
    const tab = activeTabRef.current;
    if (tab === null) return;
    pendingDraftRef.current = { tab, text };
    if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    draftTimerRef.current = setTimeout(flushDraft, DRAFT_MIRROR_DELAY_MS);
  }
  useEffect(
    () => () => {
      if (draftTimerRef.current !== null) clearTimeout(draftTimerRef.current);
    },
    [],
  );
  /** How the NEXT cursor change should bring its row on screen (`revealRow`): +1/-1 when `j`/`k`
   *  moved it (a tall row shows its top or its bottom), `"keep"` when the key already put the view
   *  exactly where it belongs (`Ctrl+d`/`Ctrl+u` re-homing, `G`, `gg`), 0 otherwise. A ref, read
   *  and reset by the cursor-follow effect, and only ever set when the cursor really changes -- an
   *  unchanged cursor runs no effect, and a stale value would misplace some later, unrelated move. */
  const landingRef = useRef<1 | -1 | 0 | "keep">(0);
  /** Whether the view eases to the next landing (`revealRow`'s `animate`): set only by a lone `j`/`k`,
   *  read and reset by the cursor-follow effect with `landingRef`. */
  const landingAnimateRef = useRef(false);
  /** Whether the next landing enters a row taller than the view at its head (`revealRow`'s `head`): set by
   *  the `focus_permission` landing, read and reset by the cursor-follow effect with `landingRef`. */
  const landingHeadRef = useRef(false);
  /** Whether the last `j` moved, so the first repeat of a held `j` that then stops at the last
   *  stop still flashes once (seen in a GUI pass, 2026-09-27) -- see the move branch of `onKeyDown`. */
  const heldMoveRef = useRef(false);
  /** The open `/` prompt and where the cursor was when it opened; `lastSearchRef` is what `n`/`N` repeat. */
  const [search, setSearch] = useState<{ query: string; origin: number } | null>(null);
  const lastSearchRef = useRef("");
  /** The open `:` command line's text, or `null` while it is closed. It runs no
   *  command: it exists so `:ls⏎`-style keys land in a box instead of on a card. Closed wherever the
   *  `/` prompt is, and for the same reasons -- an overlay that hands the keys to the root when it
   *  closes (the chooser, a tab rename) included -- and the two are one command
   *  line: opening either closes the other. */
  const [exLine, setExLine] = useState<string | null>(null);
  /** Bumped by every `/`, `:` and `panel.search`, and read by both lines'
   *  `SearchBar` as its `focusRequest`, so a line already open when its key comes again -- the keys
   *  had gone elsewhere, a click took them -- takes them back instead of only being wiped (`:`, a
   *  click, `:`, `l`, Enter walked onto Approve and pressed it). A line just opened takes them as it
   *  mounts anyway. The two are never open at once, so one counter serves both. */
  const [lineFocusRequest, setLineFocusRequest] = useState(0);
  function moveCursorTo(index: number) {
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    noteUserScroll(list, index < cursor ? "up" : "down");
    setCursor(index);
  }
  /** The first key of a two-key BROWSE sequence (`resolveKey`'s `{kind:"pending"}`: `g`, `z`, `[`, `]`
   *  or `Ctrl+w`), or `null` between sequences. Cleared on EVERY key
   *  `onKeyDown` sees, so anything but the matching second half cancels it, and on every `pane_focus`
   *  envelope, so a pane switch the WebView never saw as a key cancels it too; handed to `resolveKey`
   *  in its context so the key table itself keeps no memory. (Formerly `pendingGRef`, a plain
   *  boolean, before `[[`/`]]` gave BROWSE a second prefix.) */
  const pendingRef = useRef<PendingPrefix | null>(null);
  /** The count accumulated from `1`-`9` then `0`-`9`, applied to the next `j`/`k`/`[[`/`]]` --
   *  and to `G`, `gg`, `gt` and `gT` (a prefix hands it on to its second
   *  key) -- and reset by it (or by anything else that runs). `null` when no digit has been pressed
   *  yet. A ref for the same reason `pendingRef` is one: read once by `onKeyDown`, never rendered. */
  const countRef = useRef<number | null>(null);
  /** A leader/table sequence in progress (`./leader`'s `startSequence`/`advanceSequence`), or
   *  `null` between sequences -- the engine's OWN pending state, distinct from `pendingRef` above,
   *  which is `resolveKey`'s five reserved two-key prefixes (`g`/`z`/`[`/`]`/`Ctrl+w`) and predates this
   *  plan. A ref for `onKeyDown` to read synchronously (the same reason `pendingRef` is one) mirrored
   *  into `seq` (state) so the box can render what it names. */
  const seqRef = useRef<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  const [seq, setSeq] = useState<{ typed: string[]; ambiguous: PanelBinding | null } | null>(null);
  /** Whether the which-key box is actually drawn, `WHICH_KEY_DELAY_MS` after whichever became
   *  pending first -- a leader/table sequence (`seqRef`) or one of `resolveKey`'s own reserved
   *  prefixes (`pendingRef`). A ref alongside the state for the same reason `boxShown`'s own timer
   *  needs to read "is it already showing" synchronously, before the state update that would set it
   *  has committed (`applySeqStep`'s and the `"pending"` case's own "only if not already shown"). */
  const boxShownRef = useRef(false);
  const [boxShown, setBoxShown] = useState(false);
  const boxTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The `timeoutlen` timer that runs an ambiguous node's own action once it elapses with no further
   *  key (as vim's `timeoutlen` does) -- armed only while `panelTable.timeout` is true and the pending node both has
   *  a continuation AND is itself a binding (`SeqStep`'s `ambiguous`). */
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
  /** (Re)starts the box's `WHICH_KEY_DELAY_MS` timer -- only while it is not already showing (a
   *  step deeper into an already-drawn box must not blink it off and back on), matching
   *  `applySeqStep`'s own doc comment. Shared by the leader engine's own `"pending"` arm and by
   *  `resolveKey`'s reserved two-key prefixes below, since the two can never both be pending at
   *  once (starting either clears the other, `clearSequence`'s own doc comment). */
  function scheduleBoxTimer() {
    if (boxShownRef.current) return;
    cancelBoxTimer();
    boxTimerRef.current = setTimeout(() => {
      boxTimerRef.current = null;
      showBox();
    }, WHICH_KEY_DELAY_MS);
  }
  /** Cancels a leader/table sequence in progress and hides the box, with no side effect beyond
   *  that -- never runs an action. Added to every place `pendingRef`
   *  itself is already cleared (a pane switch the WebView never saw as a key, a HINT starting
   *  elsewhere, every key `onKeyDown` sees) and to the `keymap` envelope arm, so a table that
   *  changes mid-sequence can never run a binding from the one it replaced. */
  function clearSequence() {
    seqRef.current = null;
    setSeq(null);
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
    hideBox();
  }
  /** A prefix, a count and a leader sequence are one thing in progress; every cancel route
   *  (pane_focus, arrive, an overlay, a tab switch, Shift+Tab) drops all three together -- the
   *  empty tab's own prefix and sequence included, which live in `EmptyTab` (`g`,
   *  Shift+Tab, `i` used to swallow the `i` there). Only refs and state setters, so the listeners installed
   *  once, with the first render's closure, may call it. */
  function dropPendingKeys() {
    pendingRef.current = null;
    countRef.current = null;
    clearSequence();
    setEmptyDropKeys((n) => n + 1);
    // A `gx` pick waiting for its letter is a pending key too -- left up across a
    // cancel route, the next letter typed would open a link and be swallowed.
    setLinkPick(null);
    setPathPick(null);
  }
  useEffect(() => {
    cancelBoxTimer();
    cancelSeqTimeoutTimer();
  }, []);
  /** One step of the sequence engine: `run` clears the sequence and performs the binding;
   *  `pending` records the new node and, only if the box is not already showing, (re)arms its
   *  `WHICH_KEY_DELAY_MS` timer -- plus, when this node is itself a binding (ambiguous) AND
   *  `panelTable.timeout` is on, a `panelTable.timeoutlen` timer that runs the ambiguous binding
   *  with no further key; `cancel` clears with nothing run. */
  function applySeqStep(step: SeqStep) {
    if (step.kind === "run") {
      clearSequence();
      runPanelAction(step.binding);
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
  /** Runs a table binding reached through the leader engine or a reserved-prefix pair (`[b`).
   *  Tab navigation posts the window-level `tab_verb` through `postToRust` directly
   *  (never `post()`, which would add a `tab` this message has no field for -- Rust's own
   *  `InboundMessage::TabVerb` names none, since whichever tab is under the keys decides). */
  function runPanelAction(binding: PanelBinding) {
    switch (binding.action) {
      case "tab.new":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "new" });
        break;
      case "tab.next":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "next" });
        break;
      case "tab.prev":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "prev" });
        break;
      case "tab.last":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "last" });
        break;
      case "tab.close":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "close" });
        break;
      case "tab.close-others":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "close_others" });
        break;
      case "tab.choose":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" });
        break;
      case "tab.info":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "info" });
        break;
      case "panel.search":
        // One command line, as in vim -- the `/` prompt replaces an open `:` line.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
        break;
      case "panel.keymap":
        keymapOpening.current = null;
        keymapClosedByKey.current = false;
        setKeymapOpen(true);
        break;
      case "panel.handoff": {
        // Only a Claude session can be taken into a terminal `claude`; another agent's tab says so
        // instead of opening a confirmation that could not be carried out.
        const other = agentWord(activeTabInfo(tabsRef.current)?.agent);
        if (other !== null) {
          showFlash(`Continuing a ${other} session in a terminal is not available yet`);
          break;
        }
        // `<leader>t`: opens `ContinueInTerminal`'s confirmation directly, since the
        // band leaves no permanently visible button for a click to reach first any more. Opening
        // unconditionally is deliberate -- if the handoff is blocked (no session id yet, a turn
        // running), that component shows why instead of the confirm dialog, rather than this key
        // doing nothing with no explanation.
        setHandoffOpen(true);
        break;
      }
      case "model.pick":
      case "effort.pick": {
        const thing = binding.action === "model.pick" ? "model" : "effort";
        const other = agentWord(activeTabInfo(tabsRef.current)?.agent);
        if (other !== null && !isAcpAgent(activeTabInfo(tabsRef.current)?.agent)) {
          showFlash(`Choosing the ${thing} of a ${other} session is not available`);
          break;
        }
        // The empty-tab render returns before bandPicks is initialized, so check for a live session first.
        if (activeTab?.state !== "live" || !bandPicks) {
          showFlash(`no running session to choose the ${thing} of`);
          break;
        }
        askBandPicker(thing === "model" ? "/model" : "/effort");
        break;
      }
      case "tab.follow": {
        const tab = activeTabInfo(tabsRef.current);
        if (tab === null) break;
        post({ type: "toggle_following" });
        showFlash(tab.following ? "not following" : "following the agent in the editor");
        break;
      }
      case "tab.stop-background":
        askStopBackgroundOfActiveTab();
        break;
      case "remote.devices":
        if (!remoteDesk.openList()) showFlash("remote control is off in this window");
        break;
      case "mode.cycle":
        // Disabled in the box once the tab is fixed (`modeFixed`, `leader.ts`'s own `boxEntries` doc
        // comment): cycling makes no sense any more, so this flashes rather than posting. Routed
        // through `modeKeyRoute`, the same rule Shift+Tab uses. Every route but
        // `ended`/`failed`-not-already-bypass now always posts -- there is no capability left to
        // gate on, and a move into bypass gets its own y/n from Rust rather than being refused or
        // applied silently here.
        switch (
          modeKeyRoute({
            confirmOpen: false,
            chooserOpen: false,
            tabState: activeTab?.state ?? null,
            tabMode: activeTab?.mode ?? null,
          })
        ) {
          case "cycle":
            post({ type: "cycle_mode" });
            break;
          case "fixed":
            // "r" (not `keymapHelp.newTabChord`): reaching "fixed" means the tab has already ended
            // or failed, and `r` -- not a rebindable panel-table action -- is what restarts it in
            // place on both layouts (`EmptyTab`'s own `r`, BROWSE's `restart` case).
            showFlash(modeFixedMessage("r"));
            break;
        }
        break;
    }
  }
  /** Whether the `?` keymap overlay is open. Toggled by `resolveKey`'s `{kind:"keymap"}`
   *  (opening only -- see `onKeyDown`'s dedicated branch below for why closing never goes through
   *  `resolveKey` at all) and by the handful of places that must force it shut: the session ending,
   *  the start screen coming back, and a HINT starting elsewhere in the window. */
  const [keymapOpen, setKeymapOpen] = useState(false);
  /** The number `prefix ?` gave the opening that is showing (`open_keymap`'s `opening`), or `null` when the
   *  panel's own `?` opened it: only the former reports its close to Rust, which may give the keys back. */
  const keymapOpening = useRef<number | null>(null);
  /** Counts the shell's `open_keymap` envelopes, so one that arrives while the sheet is already up still
   *  re-renders it and the sheet can move to where that opening starts. */
  const [keymapRequest, setKeymapRequest] = useState(0);
  /** Whether the showing opening is being closed by the overlay's own keys or a click outside it. Every
   *  other close leaves it false, so a forced close (`:`, a review, a new session) gives nothing back. */
  const keymapClosedByKey = useRef(false);
  function closeKeymapByKey() {
    keymapClosedByKey.current = true;
    setKeymapOpen(false);
  }
  useEffect(() => {
    if (keymapOpen) return;
    const opening = keymapOpening.current;
    if (opening !== null) {
      postToRust({ type: "keymap_closed", request_id: nextRequestId(), opening, return_keys: keymapClosedByKey.current });
    }
    keymapOpening.current = null;
    keymapClosedByKey.current = false;
  }, [keymapOpen]);
  const keymapPendingG = useRef(false);
  useEffect(() => { keymapPendingG.current = false; }, [keymapOpen]);

  function onKeymapKey(event: KeyboardEvent) {
    event.preventDefault();
    event.stopPropagation();
    if (isModifierKey(event.key)) return;
    const pendingG = keymapPendingG.current;
    keymapPendingG.current = false;
    if (event.altKey || event.metaKey || event.nativeEvent.isComposing) return;
    if (event.ctrlKey && event.shiftKey) return;
    if (!event.ctrlKey && (event.key === "?" || event.key === "Escape" || event.key === "q")) {
      closeKeymapByKey();
    } else if (!event.ctrlKey && event.key === "g") {
      if (pendingG) scrollKeymapOverlay(keymapOverlayRef.current, "gg", false);
      else keymapPendingG.current = true;
    } else {
      scrollKeymapOverlay(keymapOverlayRef.current, event.key, event.ctrlKey);
    }
  }
  /** The plugin page's section `prefix ?` sent with `open_keymap`, drawn first while the overlay is open. It
   *  belongs to that one opening: once the overlay closes it is dropped, so the panel's own `?` later shows no
   *  page's keys. */
  const [keymapPlugin, setKeymapPlugin] = useState<PluginKeysHelp | null>(null);
  useEffect(() => {
    if (!keymapOpen) setKeymapPlugin(null);
  }, [keymapOpen]);
  /** The review overlay's own root, so `j`/`k` can scroll an open hunk's box before they move the cursor. */
  const reviewOverlayRef = useRef<HTMLDivElement>(null);
  /** The detail popover's rows, or `null` when it is closed. Set by a
   *  `tab_detail` envelope (the reply to `open_detail`, sent by the band and by `prefix i`);
   *  closed the same places `keymapOpen` is forced shut, since the two are mutually exclusive
   *  overlays over the same conversation area. */
  const [detail, setDetail] = useState<DetailRow[] | null>(null);
  /** The row `j`/`k`/`y` act on inside the popover, reset to 0 every time it opens. */
  const [detailCursor, setDetailCursor] = useState(0);
  /** Whether `ContinueInTerminal`'s confirmation is open.
   *  Lifted here from that component's own local state -- which used to gate on a permanently
   *  visible button's click -- because the band leaves no permanently visible button any more: the
   *  only entry points now are `<leader>t` (`"panel.handoff"`, `onKeyDown` below) and the detail
   *  popover's own trailing row. Closed everywhere the other conversation-area overlays are (a
   *  pane-focus change, a tab switch, `enter_input`/`arrive`, `focus_permission`, `hint_collect`,
   *  a rename/close-prompt/chooser opening, an `error`/`handoff` whole-state reset) and by `Esc`. */
  const [handoffOpen, setHandoffOpen] = useState(false);
  /** A `gf` pick owns the keys until its full label is answered or it is cancelled.
   *  Row keys and text offsets survive DOM replacements; placement decides which paths need the footer. */
  const [pathPick, setPathPick] = useState<{
    refs: PathRef[]; labels: string[]; targets: (PathTarget | null)[]; shown: string[]; typed: string;
  } | null>(null);

  const pathPickOpen = pathPick !== null;
  useEffect(() => {
    if (!pathPickOpen) return;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    if (list) return watchUserScroll(list, () => setPathPick(null), () => {
      // A follow snap or layout re-anchor keeps the pick and commits so HintLayer measures the moved text.
      setPathPick((pick) => pick === null ? null : { ...pick });
    });
  }, [pathPickOpen]);

  function beginPathPick(refs: PathRef[], rows: HTMLElement[] = [], selection?: Range) {
    const targets: (PathTarget | null)[] = refs.map(() => null);
    for (const row of rows) {
      const rowKey = row.getAttribute("data-row-key");
      if (rowKey === null) continue;
      const limit = selection === undefined ? undefined : pathSelectionOffsets(row, selection);
      pathRanges(row, refs, limit).forEach((offsets, i) => {
        if (offsets === null || targets[i] !== null) return;
        const ref = refs[i];
        targets[i] = { kind: "path", rowKey, ...offsets, text: ref.path + (ref.line === null ? "" : `:${ref.line}`) };
      });
    }
    setPathPick({ refs, labels: assignLabels(refs.length), targets, shown: [], typed: "" });
  }
  const pathLabelsPlaced = useCallback((shown: string[]) => {
    setPathPick((pick) => pick === null || (pick.shown.length === shown.length && pick.shown.every((label, i) => label === shown[i]))
      ? pick : { ...pick, shown });
  }, []);
  /** `gx` with several links -- or a titled one, or one nobody can see -- waiting for
   *  its letter: the full normalized addresses it lists, in order, or `null` between picks. Like `pathPick`
   *  it owns every key while set (`onKeyDown`, ahead of the key table), but it is also dropped by every route
   *  away (`dropPendingKeys`, the tab switch): a pick left up would open a link on a later letter, over a
   *  conversation that no longer holds it. */
  const [linkPick, setLinkPick] = useState<string[] | null>(null);
  /** `open_path` and `view_in_editor` share `"editor"` in-flight bookkeeping with
   *  `edit_draft` (see `inFlight`'s own doc comment). A suffix lookup may answer with `path_choices`;
   *  direct opens answer with a command result, whose refusal flashes in the band. */
  function openPath(ref: PathRef) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_path", request_id: requestId, path: ref.path, ...(ref.line === null ? {} : { line: ref.line }) });
  }
  function runSlashAction(action: SlashAction) {
    switch (action.kind) {
      case "tasks":
        openTasksOverlay(action.workflows);
        break;
      case "keymap":
        keymapOpening.current = null;
        keymapClosedByKey.current = false;
        setKeymapOpen(true);
        break;
      case "tab-info":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "info" });
        break;
      case "sessions":
        postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" });
        break;
      case "open-path":
        openPath({ path: action.path, line: null });
        break;
      case "pointer":
        showFlash(action.text);
        break;
    }
  }
  function openTasksOverlay(workflows = false, taskId: string | null = null) {
    endKeyPrompts();
    dropPendingKeys();
    exitRegion();
    closeTransientOverlays();
    setReview(null);
    setChooser(null);
    setSearch(null);
    setExLine(null);
    setSlashPicker(null);
    setMode("browse");
    setTasksWorkflows(workflows);
    setTasksInitialId(taskId);
    setTasksOpen(true);
  }
  function closeTasksOverlay() {
    taskScratchTab.current = null;
    setTasksOpen(false);
    (containerRef.current ?? startScreenRef.current)?.focus();
  }
  function onTasksKey(event: KeyboardEvent<HTMLDivElement>) {
    event.preventDefault();
    event.stopPropagation();
    tasksRef.current?.key(event.nativeEvent);
  }
  function tasksOverlay() {
    return tasksOpen && <TasksOverlay ref={tasksRef} state={state} workflows={tasksWorkflows} initialTaskId={tasksInitialId}
      onClose={closeTasksOverlay}
      onRequest={(kind, taskId, requestId, offset) => {
        const tab = activeTabRef.current;
        if (tab === null || taskRequests.current.size >= 64) {
          tasksRef.current?.reply({ requestId, error: "too many task commands are waiting" });
          return;
        }
        taskRequests.current.set(requestId, tab);
        postToRust(kind === "task_output" && offset !== undefined
          ? { type: kind, request_id: requestId, tab, task_id: taskId, offset }
          : { type: kind, request_id: requestId, tab, task_id: taskId });
      }}
      onOpen={(title, text) => {
        const requestId = nextRequestId();
        taskScratchTab.current = activeTabRef.current;
        trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
        postToRust({ type: "view_in_editor", request_id: requestId, title, text });
      }} />;
  }
  /** `c` in BROWSE: opens the review overlay on this tab's latest turn and asks the shell for it. The
   *  overlay is drawn at once, saying it is loading, and filled in by the `review` envelope that answers
   *  this request id (or told why not by the refusal). Recorded as `"review"`, so a refusal is the
   *  overlay's own header and not the conversation's banner. */
  function openReviewOverlay(tab: TabId) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "review", tab });
    setReview(openReview(tab, requestId, reviewRecovery));
    postToRust({ type: "review_request", request_id: requestId, tab, turn: "latest", scope: "turn" });
  }
  /** What a key in the review overlay asked for beyond changing its own state. `tab` is the tab the overlay
   *  was opened on, never "the active one": a request names its tab. */
  function runReviewEffect(effect: ReviewEffect, tab: TabId) {
    switch (effect.kind) {
      case "close":
        setReview(null);
        return;
      case "request":
        trackRequest(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope });
        return;
      case "request-diff":
        trackRequest(effect.requestId, { kind: "review", tab });
        postToRust({ type: "review_diff_request", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path });
        return;
      case "open": {
        // The editor opens it at the hunk's first line when a patch has been loaded, else at the top. It
        // is the shell that knows whether the turn's hunks are drawn there too, so it is asked, by turn.
        const requestId = nextRequestId();
        trackRequest(requestId, { kind: "editor", tab, review: true });
        postToRust({
          type: "open_in_editor",
          request_id: requestId,
          tab,
          turn: effect.turn,
          scope: effect.scope,
          path: effect.path,
          ...(effect.line === null ? {} : { line: effect.line }),
        });
        return;
      }
      case "revert":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_revert", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, target: effect.target });
        return;
      case "undo":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_undo", request_id: effect.requestId, tab });
        return;
      case "comment-add":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_add", request_id: effect.requestId, tab, turn: effect.turn, scope: effect.scope, path: effect.path, from: effect.from, to: effect.to, text: effect.text });
        return;
      case "comment-remove":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_comment_remove", request_id: effect.requestId, tab, id: effect.id });
        return;
      case "send":
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_send", request_id: effect.requestId, tab, confirm: effect.confirm });
        return;
      case "recover":
        // Window-level: the journal is the project's, so this names no tab.
        trackRequest(effect.requestId, { kind: "review-command", tab });
        postToRust({ type: "review_recover", request_id: effect.requestId, entry: effect.entry, answer: effect.answer });
        return;
      case "copy":
        void navigator.clipboard?.writeText(effect.text);
        copyToPrimary(effect.text);
        showFlash(`copied ${effect.text}`);
        return;
    }
  }
  /** `gx`: hands one address -- always a `webLinks` `url`, the normalized `href` the reader was shown -- to
   *  Rust, which re-checks it (`web_url`) and opens it in the system browser. Recorded as `"link"`, so a
   *  refusal is a footer flash (see `inFlight`). */
  function openUrl(url: string) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "link", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "open_url", request_id: requestId, url });
  }
  /** The popover's own scrollable root -- unused today (it has nothing to scroll into view yet),
   *  kept for parity with `keymapOverlayRef` and because a forwarded ref is part of the component's
   *  contract. */
  const detailRef = useRef<HTMLDivElement>(null);
  /** The picker a bare `/model`/`/effort` reply opens, or `null`
   *  when closed. Opened purely client-side (no Rust envelope, unlike `chooser`) by the `events`
   *  handler below the moment a `turn_completed` following one of those bare sends parses -- see
   *  `pendingSlashPickerRef` and `../slashPicker`. */
  const [slashPicker, setSlashPicker] = useState<SlashPickerState | null>(null);
  /** A parsed `/model`/`/effort` reply on its
   *  way to `slashPicker`, held for one render so the effect below can see every overlay as that
   *  render left it -- the `events` handler is installed once and cannot. A reply that finds another
   *  overlay open (the chooser, a rename, the `/` prompt, `?`, the detail popover, the handoff
   *  confirm, a `gf` pick, a y/n prompt) opens no picker: it stays what it already is in the
   *  transcript, the CLI's own text. The picker used to open on top of whichever it was, and after a
   *  focus round trip `takeKeys` gave the keys to the chooser underneath, so Enter answered the
   *  chooser instead of choosing a model. */
  const [slashReply, setSlashReply] = useState<SlashPickerState | null>(null);
  /** Set by `sendMessage` the moment it sends a BARE `/model`/`/effort` (`barePickerCommand`), read
   *  and cleared by the very next `turn_completed` this tab sees, whether or not that reply parses
   *  -- a ref, not state, because it is read and written from inside the `events` handler's own
   *  closure (installed once, in the mount effect) rather than from render. Only the immediately
   *  following turn is ever a candidate: a picker choice itself always carries an argument
   *  (`onChoose` below), so it can never re-arm this, and a follow-up queued behind a running turn
   *  is out of this feature's scope (an edge case `sendMessage`'s own "not running" path does not
   *  reach) -- worst case there, no picker opens and the reply shows as ordinary text, never a
   *  broken one.
   *
   *  Also a single flag rather than a per-tab one: the tab-switch
   *  reset block below clears it the moment the active tab changes, so a switch away before the
   *  reply lands drops the candidate instead of leaving it to be checked against whatever
   *  `turn_completed` the NEW active tab sees next -- the same reasoning `queueMessage`/`send_now`
   *  below never arming this at all is built on. Both `queueMessage` (queued behind a running turn)
   *  and `send_now`'s own post (an immediate send that may itself queue if its interrupt is
   *  refused) can leave more than one turn between the send and its own reply, so "the very next
   *  `turn_completed`" would not reliably mean "this command's own reply" the way it does for
   *  `sendMessage`'s own not-running path -- arming there risks a picker opening against an
   *  unrelated turn's text, worse than today's safe fallback (plain text, no picker). Left
   *  unfixed, deliberately, rather than reaching for a heuristic that would trade a missed picker
   *  for a wrong one. */
  const pendingSlashPickerRef = useRef<SlashPickerKind | null>(null);
  /** True while an
   *  overlay drawn OVER the conversation or the empty tab owns the keys -- the chooser, a tab
   *  rename, or the `/` search prompt (the live layout's only, hence `sessionStarted`: `search`
   *  can outlive the tab it opened on, and a stale value under the empty layout, where `SearchBar`
   *  never renders, would otherwise strand the keys for good). `?`, the detail popover and the
   *  handoff confirm are NOT here: `pane_focus`/`arrive`
   *  already close those three, so nothing needs to re-check them here. Read by every effect below
   *  that could otherwise steal the keys out from under one of these three overlays. `slashPicker`
   *  sits alongside `chooser` for the same reason -- it is drawn over the conversation and must keep
   *  the keys the same way -- and so does the `:` command line
   *  (`exLine`), which is drawn and gated exactly like the `/` prompt. */
  const overlayOpen =
    tasksOpen ||
    chooser !== null ||
    slashPicker !== null ||
    renaming !== null ||
    (search !== null && sessionStarted) ||
    exLine !== null ||
    review !== null;
  useEffect(() => {
    if (waitingQuestion && mode === "input" && paneFocused && !overlayOpen && confirm === null) {
      const focused = document.activeElement;
      if (!focused?.closest(".question-dock, .permission-card")) questionDock.current?.focus();
    }
  }, [questionIdentity, mode, paneFocused, overlayOpen, confirm]);
  /** For the document replay (installed once): see its note on the empty layout. */
  const overlayOpenRef = useRef(overlayOpen);
  overlayOpenRef.current = overlayOpen;
  /** Counters `takeKeys` (below) bumps to ask a specific overlay/composer to re-focus itself,
   *  mirroring the existing `inputRequest`/`arriveRequest` convention: a plain number so a second
   *  request while the first is still pending is never silently coalesced away by React (an
   *  unchanged boolean would be). */
  const [chooserFocusRequest, setChooserFocusRequest] = useState(0);
  /** Bumped by each `arrive` that carries `intent: "stop"`: the tab was brought forward to be stopped, and the
   *  question is asked once that envelope is drawn (the effect after the arrival's landing). */
  const [stopAskRequest, setStopAskRequest] = useState(0);
  /** Hands an open picker the keys again: `takeKeys` used to have no `slashPicker` branch, so a GTK
   *  focus round trip while the picker was open handed the keys to the container root instead,
   *  leaving `j`/`k`/`Enter`/`Escape` dead. Same convention as `chooserFocusRequest`, read by `SlashPicker`'s
   *  own `focusRequest` prop. */
  const [slashPickerFocusRequest, setSlashPickerFocusRequest] = useState(0);
  const [tabBarFocusRequest, setTabBarFocusRequest] = useState(0);
  const [emptyKeysRequest, setEmptyKeysRequest] = useState(0);
  /** What `Composer` (the live conversation's) actually reads, instead of the raw `inputRequest`
   *  counter it used to take directly. `Composer`'s own `[focusRequest, mode]` effect focuses the
   *  textarea whenever `mode === "input"`, with no way to gate it from outside -- so a raw
   *  `inputRequest` bump under an open overlay would autofocus a composer drawn underneath it. Bumped from the `inputRequest` effect's own pass branch, below, so ordinary
   *  behaviour (no overlay open) is unchanged; and from `resolveKey`'s own `i`/`o`/`A` --
   *  `mode` alone already re-mounts the textarea (BROWSE's stand-in unmounts, INPUT's own mounts),
   *  but this is what makes `Composer`'s effect actually run `focusAtCaret` rather than leaving the
   *  caret to whatever a bare `autoFocus` on a non-empty value happens to land on (`i` after an `Esc` re-mounted the box at position 0). */
  const [composerFocusRequest, setComposerFocusRequest] = useState(0);
  /** Which end `composerFocusRequest`'s NEXT bump should place the caret at --
   *  `"kept"` for `i`/`o` (wherever it was left, `Composer`'s own `caretRef`), `"end"` for `A`.
   *  Read once, alongside the bump, by `Composer`'s own effect; not itself a `PanelMode`-changing
   *  request the same way `composerFocusRequest` is, so it carries no `seq` of its own. */
  const [composerCaret, setComposerCaret] = useState<"kept" | "end">("kept");
  const [keysRequest, setKeysRequest] = useState(0);
  /* The window's remote-control questions and its device list, kept here so a switch between the empty tab and the
     conversation (each draws them) loses none. While another overlay owns the keys they take none: a `y` typed into
     the chooser's filter is the filter's. */
  const [remoteDesk] = useState(() => new RemoteDesk());
  const remoteOn = useSyncExternalStore(remoteDesk.subscribe, () => remoteDesk.snapshot().on);
  /** The panel's own which-key table, read off `keymapHelp`; the device list's key only while remote control is on
   *  in this window. */
  const panelTable = useMemo(
    () =>
      remoteOn
        ? keymapHelp.panel
        : { ...keymapHelp.panel, bindings: keymapHelp.panel.bindings.filter((b) => b.action !== "remote.devices") },
    [keymapHelp.panel, remoteOn],
  );
  /** Mirrors of state the document-capture Shift+Tab router (below, `onModeKey`)
   *  must read live -- refs so that effect, installed once with an empty dependency array, always
   *  sees this render's values rather than the one from when it was installed. Assigned here in the
   *  render body, the same pattern `sessionStartedRef` above already uses. */
  const confirmOpenRef = useRef(false);
  confirmOpenRef.current = confirm !== null;
  /* The keys MOVING into a text field after a bypass or restore question appeared (a click on the composer, a
     rename field) end the question: what is typed there is text, and the `y` of "you should" must not give a tab
     bypass just because the question was still up. Only a move counts -- a field that already had the keys when
     the question opened (Shift+Tab pressed in the composer) is where an INPUT user answers, so its `y` still does. */
  const confirmEntersBypass =
    confirm?.kind === "bypass" || confirm?.kind === "restore" || confirm?.kind === "stop_background" || confirm?.kind === "stop_turn";
  useEffect(() => {
    if (!confirmEntersBypass) return;
    function onFocusIn(event: FocusEvent) {
      if (!isEditableElement(event.target)) return;
      setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" || c?.kind === "stop_background" || c?.kind === "stop_turn" ? null : c));
    }
    document.addEventListener("focusin", onFocusIn, true);
    return () => document.removeEventListener("focusin", onFocusIn, true);
  }, [confirmEntersBypass]);
  const chooserOpenRef = useRef(false);
  chooserOpenRef.current = chooser !== null;
  const tabsRef = useRef<TabsEnvelope | null>(null);
  tabsRef.current = tabs;
  /** When the y/n question on screen was painted, `Infinity` until it has been: the moment a `y` starts counting from,
   *  so one pressed while the question was still being drawn answers nothing. Taken for each asking in a layout
   *  effect, before any key can be handled against it. */
  const answerableFromRef = useRef(Infinity);
  const askedAt = confirm !== null && "openedAt" in confirm ? confirm.openedAt : null;
  useLayoutEffect(() => {
    answerableFromRef.current = Infinity;
    if (askedAt === null) return;
    return afterPaint(() => {
      answerableFromRef.current = performance.now();
    });
  }, [askedAt]);
  /** What the answer guard needs of the key being handled, whose press the document-capture listener has recorded. */
  const answerClock = () => ({
    now: currentKeyAtRef.current,
    openedAt: keyClockRef.current.turned ? Infinity : answerableFromRef.current,
    lastKeyAt: lastKeyAtRef.current,
  });
  /** The last non-modifier keydown seen ANYWHERE in the panel BEFORE the one currently being
   *  processed, as pressed (`KeyClock`) -- read by `answerConfirm`'s `bypassYesCounts` check. Deliberately one keystroke
   *  BEHIND `currentKeyAtRef` (below): the document-capture effect that maintains both runs in the
   *  CAPTURE phase, which always fires before the bubble-phase `onKeyDown` that calls
   *  `answerConfirm` for the very same event -- so if this ref held the CURRENT keystroke's own
   *  time by the time that check ran, a lone `y`/`Y` would always see itself as "another key within
   *  the guard" and bypass could never be entered at all. Starts at `-Infinity` so a `y`/`Y` with no
   *  other key at all since mount is judged purely on how long the prompt itself has been on screen
   *  (`bypassYesCounts`'s `openedAt` half), never cancelled by a keystroke that never happened. */
  const lastKeyAtRef = useRef(-Infinity);
  /** The running "most recent non-modifier keydown" clock `lastKeyAtRef` lags one keystroke behind;
   *  private to the document-capture effect below. */
  const currentKeyAtRef = useRef(-Infinity);
  const keyClockRef = useRef(new KeyClock());
  /** A reveal gave this page the keys (`arrive`, `focus_permission`), from a key or click made somewhere else
   *  while the person may still be typing: the arrival counts as a key, now, for every guard that answers. The
   *  typing guard records it on the keydowns' clock (card keys, `Ctrl+y`, the leader, INPUT entry); the y/n
   *  questions' guard (`bypassYesCounts`) takes it as the last key; the question dock starts a run from it
   *  (`arrivalRunRef`). It only refuses: whatever waits keeps waiting for a key on its own. */
  function noteArrival() {
    typingGuard.noteArrival(typingGuard.clockNow());
    currentKeyAtRef.current = Math.max(currentKeyAtRef.current, performance.now());
    arrivalRunRef.current = true;
  }
  /** The key `answerConfirm` last consumed from a y/n
   *  prompt -- answered or cancelled, either way -- so its own auto-repeats can be swallowed rather
   *  than falling through as an ordinary key once `confirm` is null again. Mirrors
   *  `shell::hint::Held` (`shell/src/hint.rs`): remember the held
   *  key, not a timer, and stop remembering it the moment its keyup arrives (the effect right
   *  below) -- never on a fresh, different key, so an ordinary later hold of the SAME key (nothing
   *  to do with any prompt) is never mistakenly swallowed just because this key answered a prompt
   *  once, earlier in the session. `null` when nothing consumed by a prompt is still physically held. */
  const promptSwallowKeyRef = useRef<string | null>(null);
  /** The trust question's own scroll box, and whether the key before this one was the first `g` of `gg`. */
  const trustOverlayRef = useRef<HTMLDivElement>(null);
  const trustPendingGRef = useRef(false);
  /* A trust question opens only once no other prompt, field or picker holds the keys. A close, bypass or restore
     prompt answers the next `y`, and the rename field, the chooser, the `:` line and the `gf` picker take typed
     letters; opening over any of them would make the key meant for it an answer to this question, and a `y` here
     loads a repository's hooks. So it waits, and opens with its own on-screen wait once they are gone. A route
     away drops it (`cancelBypassConfirm`, `endKeyPrompts`); Rust asks again afterwards. Another trust question
     on screen is replaced, with its own fingerprint and digest. */
  const keysTakenElsewhere =
    (confirm !== null && confirm.kind !== "trust") ||
    renaming !== null ||
    chooser !== null ||
    exLine !== null ||
    pathPick !== null;
  useLayoutEffect(() => {
    if (heldTrust === null || keysTakenElsewhere) return;
    const payload = heldTrust;
    setHeldTrust(null);
    // An overlay that takes the keys ends a pending sequence, as every confirm kind does.
    dropPendingKeys();
    exitRegion();
    setDetail(null);
    setHandoffOpen(false);
    setKeymapOpen(false);
    // The keys must be somewhere that is not a text field: a first send leaves them in the composer, and
    // a key typed into a field is never an answer.
    (containerRef.current ?? startScreenRef.current)?.focus({ preventScroll: true });
    trustPendingGRef.current = false;
    setConfirm({
      kind: "trust",
      tab: payload.tab,
      nonce: payload.nonce,
      view: payload,
      lines: [payload.state === "too_large" ? TRUST_BAND_PROMPT_N_ONLY : TRUST_BAND_PROMPT],
      openedAt: performance.now(),
      flashAfter: flashSeq.current,
    });
  }, [heldTrust, keysTakenElsewhere]);
  /* The question took the keys to the layout's root (a field must not hold them while it is up). On the start
     screen that root is not what handles keys -- `.empty-tab` inside it is -- so whichever way the question
     ended, the keys are asked for back, through the one path every other overlay's close uses. Reads this
     render's overlays, so a prompt, picker or rename that opened in its place keeps them. */
  const trustOpen = confirm?.kind === "trust";
  const trustWasOpen = useRef(false);
  useEffect(() => {
    if (trustWasOpen.current && !trustOpen) setKeysRequest((n) => n + 1);
    trustWasOpen.current = trustOpen;
  }, [trustOpen]);
  useEffect(() => {
    function clearPromptSwallow(event: globalThis.KeyboardEvent) {
      if (event.key === promptSwallowKeyRef.current) promptSwallowKeyRef.current = null;
    }
    // Capture phase on the window, the same tier `swallowWhilePending` (HINT) uses below, so a
    // keyup that some other handler stops from bubbling still clears this.
    window.addEventListener("keyup", clearPromptSwallow, true);
    return () => window.removeEventListener("keyup", clearPromptSwallow, true);
  }, []);
  /** The overlay's own scrollable root, so `j`/`k` typed while it is open can scroll IT rather than
   *  the conversation underneath (`onKeyDown`'s `keymapOpen` branch). */
  const keymapOverlayRef = useRef<HTMLDivElement>(null);
  /** One ordered view of the conversation, kept in step with the cursor/expand keys below. See
   *  `MessageList`'s own copy of this memo for why it is keyed on `state` as a whole; `expanded` and
   *  `detailed` join it here for the same reason collapsed runs do -- `buildDisplay` folds or
   *  unfolds rows by both, so a change to either has to recompute what the cursor actually indexes. */
  const timeline = useMemo(
    () =>
      buildDisplay(buildTimeline(state), {
        expanded,
        detailed,
        turnRunning: state.activeTurnId !== null,
        thinking: thinkingDisplay,
      }),
    [state, expanded, detailed, thinkingDisplay],
  );
  /** For the dispatch handler (installed once), as `cursorRef`: the park names the cursor's row by key. */
  const timelineRef = useRef(timeline);
  timelineRef.current = timeline;
  /** The SAME pure computation as `timeline` above, over `frozenSnapshot` instead of the live
   *  render -- functionally identical to what `MessageList` recomputes internally once it is handed
   *  the frozen props, so a row found in the frozen DOM can be placed at this array's own index
   *  (`rowIndexOf`) and its `key` read back out of it, without indexing the DOM by the live cursor
   *  while VISUAL is on. `null` outside VISUAL/V-LINE. */
  const frozenTimeline = useMemo(
    () =>
      frozenSnapshot === null
        ? null
        : buildDisplay(buildTimeline(frozenSnapshot.state), {
            expanded: frozenSnapshot.expanded,
            detailed: frozenSnapshot.detailed,
            turnRunning: frozenSnapshot.state.activeTurnId !== null,
            thinking: thinkingDisplay,
          }),
    [frozenSnapshot, thinkingDisplay],
  );
  /** Held while what a card reads stands: a new value makes React walk every row of the conversation looking for the
   *  cards under this provider, on each streamed batch. */
  const viewScriptRef = useRef<(text: string) => void>(() => {});
  viewScriptRef.current = (text) => {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
    postToRust({ type: "view_in_editor", request_id: requestId, title: "Workflow script", text });
  };
  const { toolCalls: cardCalls, backgroundTasks: cardTasks } = frozenSnapshot?.state ?? state;
  const cardContext = useMemo(
    () => ({ state: { toolCalls: cardCalls, backgroundTasks: cardTasks }, onViewScript: (text: string) => viewScriptRef.current(text) }),
    [cardCalls, cardTasks],
  );
  const answerableItems = useMemo(
    (): AnswerableItem[] =>
      timeline.map((item) =>
        item.kind === "permission"
          ? { kind: "permission", toolUseId: item.request.toolUseId }
          : item.kind === "tool"
            ? { kind: "tool", toolUseId: item.call.toolUseId }
            : { kind: "other" },
      ),
    [timeline],
  );
  /* An answered card leaves `answeredPermissions` (and its typed reason `permissionReasons`) once it
     is no longer pending: nothing can answer it any more, and an id the provider hands out again
     later names a new card. `state` is always the active tab's (a switch resets it, and both of
     these, in the same dispatch), so this never drops another tab's entry. */
  const { pendingPermissions: heldCards, editorCards: heldEditorCards } = state;
  useEffect(() => {
    const pending = new Set(allCards({ pendingPermissions: heldCards, editorCards: heldEditorCards }).map((p) => p.permissionId));
    for (const id of permissionReasons.current.keys()) if (!pending.has(id)) permissionReasons.current.delete(id);
    const kept = [...answeredRef.current].filter((id) => pending.has(id));
    if (kept.length === answeredRef.current.size) return;
    answeredRef.current = new Set(kept);
    setAnsweredPermissions(answeredRef.current);
  }, [heldCards, heldEditorCards]);
  /** The typing guard (`./typingGuard`): every keydown `onKeyDown` sees is fed to it first; `a`/`d`/
   *  `D` wait `TYPING_GUARD_MS` through it, and Enter on a card button asks it. One per panel, kept
   *  across renders (the dispatch below is installed once and reads this same instance). */
  const [typingGuard] = useState(() => new TypingGuard());
  /* An overlay drawn over the conversation takes the keys from the card a waiting answer was aimed
     at, whichever route opened it -- a key, an envelope, a click. */
  useEffect(() => {
    if (keymapOpen || detail !== null || handoffOpen || pathPick !== null || linkPick !== null || confirm !== null || overlayOpen) {
      typingGuard.cancel();
      // A held `j`/`k`'s glide stops where it is: the keys are the overlay's now, and its repeats must
      // reach the overlay instead of moving the conversation behind it.
      const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
      if (list !== null) cancelHold(list);
    }
  }, [typingGuard, keymapOpen, detail, handoffOpen, pathPick, linkPick, confirm, overlayOpen]);
  useEffect(() => () => void typingGuard.cancel(), [typingGuard]);
  // Only the connected list: the questions' own changes do not render the whole panel.
  const connectedPhones = useSyncExternalStore(remoteDesk.subscribe, () => remoteDesk.snapshot().connected);
  const remoteSandbox = useSyncExternalStore(remoteDesk.subscribe, () => remoteDesk.snapshot().sandbox);
  useEffect(() => {
    installRemoteDesk(remoteDesk.receive);
    return () => installRemoteDesk(null);
  }, [remoteDesk]);
  const remoteCovered =
    overlayOpen || keymapOpen || detail !== null || handoffOpen || pathPick !== null || linkPick !== null || confirm !== null;
  // Releases, for an answer that waits for its key to be let go (`approveOldestByKey`). Capture
  // phase on the window, so a keyup another handler stops from bubbling still arrives.
  useEffect(() => {
    function released(event: globalThis.KeyboardEvent) {
      // A key held too long to count as a press is refused at its release, with the flash its wait named.
      // `showFlash` reads only a ref and a state setter, so this listener's first copy of it stays right.
      const refused = typingGuard.onKeyUp(heldKeyName(event), typingGuard.timeOfRelease(event));
      if (refused !== null) showFlash(refused);
    }
    window.addEventListener("keyup", released, true);
    return () => window.removeEventListener("keyup", released, true);
  }, [typingGuard]);
  /* The review overlay ends wherever something else takes the conversation area or the keys, whichever route
     brought it: another overlay or prompt opening (the envelopes that open them also close it directly), a
     click into the composer (INPUT is not BROWSE, and the overlay would swallow what is typed there), a
     switch to another tab (it shows this tab's turns). */
  const reviewCovered =
    keymapOpen ||
    detail !== null ||
    handoffOpen ||
    chooser !== null ||
    slashPicker !== null ||
    renaming !== null ||
    search !== null ||
    exLine !== null ||
    pathPick !== null ||
    linkPick !== null ||
    confirm !== null ||
    mode !== "browse";
  const pathChoicesCoveredRef = useRef(true);
  pathChoicesCoveredRef.current = reviewCovered || overlayOpen;
  useEffect(() => {
    if (reviewCovered) setReview(null);
  }, [reviewCovered]);
  const activeTabId = tabs?.active ?? null;
  useEffect(() => {
    setReview((current) => (current !== null && current.tab !== activeTabId ? null : current));
  }, [activeTabId]);
  /* A new turn replaces the finished one the band's pointer was about. */
  useEffect(() => {
    if (state.activeTurnId === null) return;
    const tab = activeTabRef.current;
    if (tab !== null) setReviewHints((hints) => withoutHint(hints, tab));
  }, [state.activeTurnId]);
  /* A `gx` pick owns every key ahead of the key table, so whatever
     else takes the keys must end it, whichever route it came by -- a click into the composer (its
     `onFocus` makes INPUT), the `?` overlay from `prefix ?`, the chooser, a `/` or `:` line, the rename
     box -- or the next letter typed there would open a link and never reach what has the keys. Every
     `pathPick`-style overlay above is read here for the same reason the typing guard reads them. The
     handler's own `isEditableElement` check (below, in `onKeyDown`) covers a text field that takes the keys
     with no state of ours to say so (a card's reason box). */
  useEffect(() => {
    if (linkPick === null) return;
    if (mode !== "browse" || keymapOpen || detail !== null || handoffOpen || pathPick !== null || confirm !== null || overlayOpen) {
      setLinkPick(null);
    }
  }, [linkPick, mode, keymapOpen, detail, handoffOpen, pathPick, confirm, overlayOpen]);
  /* `slashReply` becomes the picker only with no other overlay open
     (see `slashReply`'s own doc); either way it is used up here.
     The region (CARET/VISUAL/V-LINE) is exactly this
     kind of overlay too, and used to be missing here -- `SlashPicker` opened and focused itself
     (`slashPickerFocusRequest`), and the root's own `onFocus` (focus landing on anything
     but the root ends the region) then called `exitRegion()`, silently, on a reply that has
     nothing to do with what the user was doing. `isRegionMode(modeRef.current)` joins the same
     disjunction the chooser/`?`/`/`-prompt cases already use: the reply stays plain transcript
     text, and neither the picker nor the focus effect ever runs. */
  useEffect(() => {
    if (slashReply === null) return;
    setSlashReply(null);
    const covered =
      overlayOpen ||
      keymapOpen ||
      detail !== null ||
      handoffOpen ||
      pathPick !== null ||
      linkPick !== null ||
      confirm !== null ||
      isRegionMode(modeRef.current);
    if (covered) {
      // A reply stays what it is in the transcript. A direct list has nowhere else to be, so it says why nothing opened.
      if (slashReply.direct) showFlash(`close what is open to choose the ${slashReply.kind}`);
      return;
    }
    // The picker is a cancel route, as the chooser is. Its keys return
    // from `onKeyDown` ahead of the read-and-reset of `pendingRef`/`countRef`, so a `g`/`z`/`[`/`]`,
    // a count or a leader sequence typed while the reply was on its way outlived it, drawn box and
    // all, and took the first key after it (`i` opened nothing; Space then `m` cycled the mode).
    dropPendingKeys();
    setSlashPicker(slashReply);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [slashReply]);
  /** Kept focused so BROWSE's keydown handler actually receives keys: a keydown bubbles from
   *  whatever DOM node has real focus, which is a browser fact, not a React one. `Escape` leaving
   *  INPUT removes the composer's textarea from the DOM, which drops focus onto whatever the
   *  browser picks next (typically `document.body`) rather than back onto this element -- so
   *  without refocusing here, `j`/`k`/`Enter`/`y`/`r` would stop working after the very first trip
   *  into INPUT and back.
   *
   *  Gated on the same condition as `onKeyDown` below: if something else in this subtree -- a
   *  permission card's deny-reason box, today -- legitimately holds focus, this must NOT yank it
   *  back. Found in review: clicking that box blurs the composer's textarea, `onModeChange`
   *  reports BROWSE, and an ungated version of this effect then stole focus away from the box the
   *  click had just placed it in, so the click landed nowhere. */
  const containerRef = useRef<HTMLDivElement>(null);
  /** The one place every keyboard arrival (`pane_focus` regaining focus, `arrive`,
   *  `enter_input`, a landing on a permission card) goes to ask for the keys back, instead of each
   *  reaching for `containerRef`/a composer directly and risking one of them winning over an open
   *  overlay. Reads the CURRENT render's `chooser`/
   *  `renaming`/`overlayOpen`/`sessionStarted`, so it must only ever be called from an effect (which
   *  runs after commit, with this render's closure) -- never during render itself. Order: whichever
   *  overlay is drawn topmost first (chooser, then a tab rename, then the `/` prompt or the `:`
   *  line), else the live conversation, else the empty tab's own dashboard. */
  function takeKeys() {
    if (chooser !== null) {
      setChooserFocusRequest((n) => n + 1);
      return;
    }
    // The same reasoning as the chooser branch just above -- an
    // open picker is drawn over the conversation and must keep the keys through a GTK focus round
    // trip, `arrive`, or a HINT landing, exactly the way the chooser already does.
    if (slashPicker !== null) {
      setSlashPickerFocusRequest((n) => n + 1);
      return;
    }
    if (renaming !== null) {
      setTabBarFocusRequest((n) => n + 1);
      return;
    }
    if (search !== null && sessionStarted) {
      containerRef.current?.querySelector<HTMLInputElement>(".search-bar input")?.focus();
      return;
    }
    // The `:` command line, the same way (it is a `SearchBar` too).
    if (exLine !== null) {
      (containerRef.current ?? startScreenRef.current)?.querySelector<HTMLInputElement>(".search-bar input")?.focus();
      return;
    }
    // An open review comment input (a `SearchBar` too) keeps the keys through a window focus round trip, which
    // the overlay now survives: the conversation root would leave what is typed to the overlay's key table.
    if (review?.prompt?.kind === "comment") {
      containerRef.current?.querySelector<HTMLInputElement>(".review-prompt .search-bar input")?.focus();
      return;
    }

    if (containerRef.current !== null) {
      // A `pane_focus true` that follows a click on Approve, or a HINT landing on a control, must
      // not steal the keys back from it -- `document.activeElement` already being inside the root
      // (a permission card's own deny-reason box included) means somewhere in here legitimately
      // holds them already (regression guard, test h).
      const root = containerRef.current;
      const active = document.activeElement;
      if (active === null || active === document.body || !root.contains(active)) {
        root.focus({ preventScroll: true });
      }
      return;
    }
    setEmptyKeysRequest((n) => n + 1);
  }
  useEffect(() => {
    if (keysRequest === 0) return;
    takeKeys();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keysRequest]);
  useEffect(() => {
    if (mode !== "browse" || overlayOpen) return;
    const landed = landedControlRef.current;
    if (landed !== null && landed.isConnected) {
      landed.focus();
      return;
    }
    if (isEditableElement(document.activeElement)) return;
    containerRef.current?.focus();
  }, [mode, sessionStarted]);
  /* Shift+Tab is Claude Code's mode key everywhere in the chat, and never focus
     navigation: the mode must switch from anywhere in the agent pane. Capture phase on `document`: ahead
     of every React handler (EmptyTab's, Composer's, Chooser's, this component's own `onKeyDown`) and
     of WebKit's own default backward-focus-navigation handling for Shift+Tab, which is what used to
     hand the keys to GTK's window `move-focus` once WebKit ran out of focusable elements. */
  useEffect(() => {
    function onModeKey(event: globalThis.KeyboardEvent) {
      if (editReturnRef.current?.resumed) editReturnRef.current = null;
      // Recorded for EVERY non-modifier keydown, before `isModeCycleKey` returns early for
      // everything but Shift+Tab -- this is the guard's only source of "another key landed
      // recently", and it must see a key whether or not this effect goes on to act on it itself.
      // `lastKeyAtRef` is set to the PRIOR value on purpose (see its own doc comment): this always
      // runs before the bubble-phase `answerConfirm` that would read it for THIS SAME keydown.
      if (!isModifierKey(event.key)) {
        lastKeyAtRef.current = currentKeyAtRef.current;
        currentKeyAtRef.current = keyClockRef.current.pressedAt(event, performance.now());
        arrivalRunRef.current = arrivalRunRef.current && currentKeyAtRef.current - lastKeyAtRef.current < TYPING_GUARD_MS;
      }
      if (!isModeCycleKey(event)) return;
      if (event.target instanceof Element && event.target.closest(".question-dock")) return;
      event.preventDefault();
      // Shift+Tab does nothing while CARET/VISUAL/V-LINE is on -- its y/n
      // bypass prompt would take the next `y`, meant as VISUAL's own copy. `Esc` first, same as
      // every other overlay this table would otherwise open over it.
      if (isRegionMode(modeRef.current)) {
        event.stopPropagation();
        const cancelled = typingGuard.onKey("Tab", typingGuard.timeOf(event));
        showFlash(cancelled ?? `Esc first: Shift+Tab does not act in ${regionModeName(modeRef.current)}`);
        return;
      }
      const activeInfo = activeTabInfo(tabsRef.current);
      const route = modeKeyRoute({
        confirmOpen: confirmOpenRef.current,
        chooserOpen: chooserOpenRef.current,
        tabState: activeInfo?.state ?? null,
        tabMode: activeInfo?.mode ?? null,
      });
      if (route === "overlay") return;
      // Any key within `TYPING_GUARD_MS` of a BROWSE `y` is
      // typing -- and this capture-phase router runs ahead of `onKeyDown`'s own check of that, so
      // `y` then Shift+Tab used to post `cycle_mode` (flipping auto and bypass) right after a copy. The
      // key is claimed and recorded like any other (so a wait it cancels says so), and only a Shift+Tab
      // after a pause cycles the mode.
      const modeKeyAt = typingGuard.timeOf(event);
      if (modeRef.current === "browse" && typingGuard.afterCopy(modeKeyAt)) {
        event.stopPropagation();
        showFlash(typingGuard.onKey("Tab", modeKeyAt) ?? TYPE_HINT_FLASH);
        return;
      }
      // Shift+Tab is a key after a waiting `g`/`z`/`[`/`]`, a count or a leader sequence, and
      // the `stopPropagation` below keeps it from `onKeyDown`, which would otherwise have dropped
      // them -- so both routes drop them here. Only refs and state setters: safe from a listener
      // installed once, with the first render's closure.
      dropPendingKeys();
      event.stopPropagation();
      // This capture-phase handler runs AHEAD of the bubble-phase `onKeyDown`
      // that would otherwise feed Shift+Tab to `typingGuard.onKey` (the same mechanism
      // `answerConfirm` uses for a `confirm_bypass` prompt, this file's own `answerConfirm`) -- and
      // the `stopPropagation` just above means that bubble handler never runs at all for either route
      // reached from here, "cycle" or "fixed". Without recording it here directly, a card answer
      // `a`/`d` deferred moments earlier stayed armed and fired `TYPING_GUARD_MS` later even though
      // the user had already moved on by pressing Shift+Tab. `EmptyTab` has no permission cards (it
      // only mounts for a `not_started`/`starting` tab), so this class of bug does not reach it; its
      // own doc comment on this same key (`EmptyTab.tsx`'s `onKeyDown`) already says this handler
      // claims Shift+Tab before EmptyTab's bubble handler ever sees it.
      // `typingGuard.onKey("Tab", ...)`, not a bare
      // `cancel()` -- `cancel()` only drops a pending deferred `a`/`d` and never updates the guard's
      // own "last key" bookkeeping, so a typed key immediately before this route's Shift+Tab kept
      // ending the run, but Shift+Tab itself stayed invisible to it: `a` pressed moments AFTER a
      // Shift+Tab (the guard's "before" half) used to answer at once instead of refusing, exactly the
      // asymmetry `answerConfirm` (below) does not have, since it already calls `onKey` for this same
      // key. `onKey`'s own return is the flash for whatever it cancelled (mirroring the bubble-phase
      // `onKeyDown`'s own `typingGuard.onKey` call), so a deferred `f`/`L` cancelled by a Shift+Tab
      // now says so instead of leaving the band silent about a swallowed `a`. Dating the key with
      // `typingGuard.timeOf` matches every other `onKey`/`defer` call site in this file (`onKeyDown`,
      // `answerConfirm`), which reads `event.timeStamp` first -- the two clocks are NOT interchangeable under a test's
      // faked timers (`event.timeStamp` tracks the faked `Date`; `performance.now()` does not), so
      // using the wrong one here would silently compare a real timestamp against a faked one and
      // always read as "long ago".
      const cancelled = typingGuard.onKey("Tab", typingGuard.timeOf(event));
      if (cancelled !== null) showFlash(cancelled);
      if (route === "cycle") post({ type: "cycle_mode" });
      // "r" (not a rebindable panel-table action, so nothing on the wire names it): reaching
      // "fixed" always means the tab has already ended or failed.
      else if (route === "fixed") showFlash(modeFixedMessage("r"));
    }
    document.addEventListener("keydown", onModeKey, true);
    return () => document.removeEventListener("keydown", onModeKey, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /* Keys that land on <body> are handed back to the panel. That happens whenever the focused
     control stops existing or stops being focusable: a Dismiss that removes its own banner, an
     Approve that goes disabled once answered. A browser moves focus to <body> in both cases without
     reliably firing `blur` (a removed node fires nothing), and a disabled button receives no key
     events, so the next key would sail past `onKeyDown` and the panel would stop answering its own
     keys. The key is REPLAYED on the root rather than dropped, so the keystroke that noticed the
     problem still does what it says. The replay targets the root, so it never re-enters here.
     On the empty layout the root that handles keys is `.empty-tab`, not
     `.agent-ui-root` -- its parent, whose only handler is the `?`/confirm capture, so a key replayed
     there never reached `EmptyTab`'s own `onKeyDown` (a failed tab's `r` was dead), and focusing it
     stranded every later key there too (`returnKeysToRoot`'s own trap, the same answer). */
  useEffect(() => {
    function onDocumentKeyDown(event: globalThis.KeyboardEvent) {
      if (event.target !== document.body && event.target !== document.documentElement) return;
      const start = startScreenRef.current;
      // Under an overlay (the `prefix w` chooser, a rename) the empty layout is modal, as
      // the conversation's is -- the key is not replayed onto the empty tab behind it (a failed tab's
      // `r` would reset it under the chooser); the keys go back to the overlay instead.
      if (containerRef.current === null && overlayOpenRef.current) {
        event.preventDefault();
        setKeysRequest((n) => n + 1);
        return;
      }
      const root =
        containerRef.current ?? start?.querySelector<HTMLElement>(".empty-tab[tabindex]") ?? start;
      if (root === null) return;
      root.focus({ preventScroll: true });
      const replay = new globalThis.KeyboardEvent("keydown", {
        key: event.key,
        code: event.code,
        ctrlKey: event.ctrlKey,
        shiftKey: event.shiftKey,
        altKey: event.altKey,
        metaKey: event.metaKey,
        bubbles: true,
        cancelable: true,
      });
      if (!root.dispatchEvent(replay)) event.preventDefault();
    }
    document.addEventListener("keydown", onDocumentKeyDown);
    return () => document.removeEventListener("keydown", onDocumentKeyDown);
  }, []);
  // The same removal leaves `edgeFocused` claiming a banner that is gone, since no focus event
  // announced its disappearance. Any render after it corrects that, so the row cursor goes solid
  // again as soon as the banner is gone rather than on the next keypress.
  useEffect(() => {
    if (edgeFocused && !containerRef.current?.contains(document.activeElement)) setEdgeFocused(false);
  });

  useClampCursorToTimeline(setCursor, timeline);
  const cursorKeyRef = useRef<string | null>(null);
  const prevTimelineRef = useRef<TimelineItem[] | null>(null);
  const prevCursorRef = useRef(cursor);
  usePreserveCursorRow(prevTimelineRef, timeline, prevCursorRef, cursor, cursorKeyRef, landingRef, setCursor);
  useRevealCursorRow(landingRef, landingAnimateRef, landingHeadRef, frozenSnapshotRef, containerRef, cursor);
  const cursorRefForScroll = useRef(cursor);
  cursorRefForScroll.current = cursor;
  useClampCursorOnScroll(
    containerRef,
    frozenSnapshotRef,
    cursorRefForScroll,
    landingRef,
    setCursor,
    arrivalParkRef,
    sessionStarted,
  );
  const lastLengthRef = useRef(0);
  const landOnPromptRef = useRef(false);
  /** Set by a switch's (or a mount's) first snapshot with no saved view: see the `snapshot` arm. */
  const landOnLastRef = useRef(false);
  useRideLastRow(lastLengthRef, timeline, landOnLastRef, cursor, landOnPromptRef, landingRef, setCursor, containerRef);
  /** The scroll half of a tab switch's view restore: cursor and mode are set
   *  directly in the `snapshot` handler above, but the row the cursor lands on must exist in the DOM
   *  first, so the scroll position waits for the render that `setState` there causes. A layout
   *  effect, not the ordinary one, so it runs before the browser paints the restored state at the
   *  wrong scroll position for even one frame. `noteUserScroll` marks it the same way a HINT landing
   *  or `focus_permission` does, so `MessageList`'s own follow-the-newest-message effect does not
   *  fight it or snap back on the very next delta.
   *
   *  Keyed on `restoreTick`, not on `state` directly (see that ref's own doc comment): `state` also
   *  changes on the switch's OWN reset, one render before the real snapshot this is meant to act on,
   *  and firing there would consume `restoreRef.current` against an empty transcript. */
  useLayoutEffect(() => {
    const view = restoreRef.current;
    if (view === null) return;
    restoreRef.current = null;
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list");
    if (!list) return;
    // A tab left while following comes back following (a GUI pass, 2026-09-25):
    // said outright, as a send does, rather than as a scroll to the end the list reads a frame later
    // inside a steering window -- where any other movement above the end counted as the user
    // scrolling up. A tab left parked comes back exactly where it was.
    applyViewScroll(list, view, view.atBottom);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [restoreTick]);
  /** The session is gone (lost or closed). Read before the start-screen branch below, because the
   *  effect under it is a hook and cannot live after a conditional return. */
  const sessionEnded = state.status.kind === "unavailable" || state.status.kind === "closed";
  useEffect(() => {
    const edit = editReturnRef.current;
    if (edit === null) return;
    if (edit.tab !== activeTabRef.current || overlayOpen || keymapOpen || detail !== null || handoffOpen || confirm !== null ||
        (sessionStarted && sessionEnded) || activeTab?.state === "failed" ||
        (edit.question !== undefined && edit.question.identity !== questionIdentity)) {
      editReturnRef.current = null;
      return;
    }
    // The host returns the text and clears scratch before returning the pane's keys. :q! has no text envelope.
    if (!edit.finished || !paneFocused || edit.resumed) return;
    // A card waiting is the host's to land on (`focus_permission`, which a question counts as for a composer edit): the
    // return gives up rather than putting the keys in a box for the moment before that landing, where a fresh Enter
    // would send the draft or the answer.
    const cards = stateRef.current;
    if (cards.pendingPermissions.length > 0 || cards.editorCards.length > 0 ||
        (edit.question === undefined && waitingQuestion !== undefined)) {
      editReturnRef.current = null;
      return;
    }
    // EditorDone also sends a landing after pane_focus. Keep that one tail from undoing the resumed input.
    edit.resumed = true;
    // The keys are back from now, before the host's landing envelope arrives: the arrival counts from here, so a key
    // pressed between the two (an Enter in the answer field this focuses) answers nothing, as after any arrival.
    noteArrival();
    if (edit.landingHandled) editReturnRef.current = null;
    if (edit.question !== undefined) {
      questionDock.current?.focusEditedInput(edit.question.index);
    } else if (!sessionStarted) {
      setEmptyEditReturnRequest((n) => n + 1);
    } else if (questionIdentity === null) {
      setMode("input");
      setComposerCaret("end");
      setComposerFocusRequest((n) => n + 1);
    }
  }, [scratchEditing, paneFocused, overlayOpen, keymapOpen, detail, handoffOpen, confirm,
    sessionStarted, sessionEnded, activeTab?.state, questionIdentity]);
  /** The card INPUT's `Ctrl+y` would approve right now -- the active tab's oldest
   *  waiting card (`approveOldestByKey` picks the same one) -- for the band to name, only while the
   *  composer has the keys (INPUT, this pane focused) on a live session. `null` otherwise. */
  const ctrlYCard: ApproveFact | null = (() => {
    // Only while the composer's own box holds the keys -- the same
    // condition `approveOldestByKey` answers under, so the band never names a Ctrl+y that does
    // nothing (the `Ctrl+r` search field).
    if (mode !== "input" || !paneFocused || sessionEnded || !composerBoxFocused) return null;
    const index = oldestWaitingPermission(timeline, answeredPermissions);
    const item = index === null ? undefined : timeline[index];
    // A card Approve cannot answer is not named: `Ctrl+y` would only flash that it cannot.
    return item?.kind === "permission" && cardApprovable(item.request)
      ? { tool: item.request.toolName, summary: cardSummary(item.request.input) }
      : null;
  })();
  /** Which card INPUT's `Ctrl+y` would approve (its tab and id), and
   *  whether it has been that card for `TYPING_GUARD_MS` yet. Without this a `Ctrl+y` approved
   *  whatever was oldest at keydown however recently it had become the target: a second `Ctrl+y`
   *  10 ms after the band moved on to the next card, a card that replaced a withdrawn one, one that
   *  arrived 30 ms before a `Ctrl+y` meant as something else. Every change of target -- an arrival,
   *  a withdrawal, a tab switch, this panel's own approval moving it on -- starts the wait again. A
   *  timer, as `TypingGuard.defer` uses; no new clock. */
  const ctrlYTargetKey: string | null = (() => {
    if (sessionEnded) return null;
    const index = oldestWaitingPermission(timeline, answeredPermissions);
    const item = index === null ? undefined : timeline[index];
    return item?.kind === "permission" ? `${activeTabRef.current}:${item.request.permissionId}` : null;
  })();
  useWaitForCtrlYTarget(ctrlYTargetRef, ctrlYTargetKey);
  /* INPUT on a dead session was a one-way trap, and the panel's own banners promised otherwise.
     The composer's textarea is `disabled` once the session ends, so `autoFocus` does nothing and
     focus stays on the root -- keys still ARRIVE, they are just dropped: `resolveKey`'s "input"
     branch resolves nothing but `Escape`. So `r`/`j`/`k`/`y` all went dead while the lost-session
     and ended-session rows kept printing "Press r to return to the start screen." and nothing on
     screen mentioned Escape.

     BROWSE is forced rather than teaching `r` to resolve in INPUT, because INPUT is not merely
     key-poor on a dead session -- it is empty: there is no box to type into, so the mode has
     nothing left to be. The two other routes into it are closed at their own sources: `resolveKey`
     refuses `i` when `ctx.sessionEnded` (`./keymap`), and `Composer` stops offering the focusable
     hint that a Tab could land on. This effect is the third case, a session that dies while the
     user is already in INPUT, which neither of those can reach. */
  useEffect(() => {
    // This used to write `mode`
    // directly, bypassing `exitRegion` -- a session dying during VISUAL left `frozenSnapshot` set
    // (and the DOM selection VISUAL built still live), and neither is what a dead session should be
    // showing. `exitRegion` is a no-op when `mode` was never visual/vline, so calling it here
    // unconditionally changes nothing for every other case this effect already handled.
    if (sessionEnded) {
      exitRegion();
      setMode("browse");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionEnded]);
  // The `?` keymap belongs to a live conversation; a session that just
  // ended is not a reason to keep it up, and the ended/lost banners' own `r` must be reachable
  // without an extra `?`/`Escape`/`q` first.
  useEffect(() => {
    if (sessionEnded) setKeymapOpen(false);
  }, [sessionEnded]);
  useEnterInput(
    inputRequest,
    overlayOpen,
    takeKeys,
    sessionStarted,
    sessionEnded,
    setMode,
    setComposerCaret,
    setComposerFocusRequest,
  );
  /* `arrive`: reverses the 2026-09-19 choice that `Ctrl+l` put the caret straight into the composer,
     for every keyboard arrival except a brand-new tab's (`enter_input`, unchanged, above). A
     card waiting in this tab gets the same landing `focus_permission` gives -- BROWSE, on the oldest
     one -- never a composer the user never asked to type into. Keyed on the request alone, like
     `inputRequest`.
     `Ctrl+h` then `Ctrl+l` used to jump to the bottom every time; the landing half is reversed for
     everything else, so the cursor is remembered: BROWSE, and the reader's own place. A reader who was
     following the bottom lands on the last row and keeps following (the same way a send does,
     `./follow.ts`); anyone else gets the row they left (by key, so a row removed above it while away
     does not shift the landing) and the scroll they left, through the park the `pane_focus` arm took.
     With no usable park (a launch arrival, a park for another tab, one a key or a hand on the list made
     stale), the live view decides the same way: following lands last, anything else moves nothing. */
  useEffect(() => {
    if (arriveRequest === 0) return;
    const park = arrivalParkRef.current;
    arrivalParkRef.current = null;
    // A key handled since the envelope arrived already acted on the panel as drawn (`landingAtKeyRef`):
    // the landing would undo it, so the key wins and nothing below runs.
    const atKey = landingAtKeyRef.current;
    landingAtKeyRef.current = null;
    if (atKey !== null && atKey !== typingGuard.keyCount()) return;
    // An arrival (a tab switch,
    // `Ctrl+h`/`Ctrl+l`) used to write `mode`/`cursor` directly below without ever calling
    // `exitRegion`, so `frozenSnapshot` -- and the row VISUAL was looking at in the OLD tab -- stayed
    // set while a different tab's live conversation took over underneath it. A no-op when VISUAL was
    // not on.
    exitRegion();
    // An overlay open over the conversation keeps the keys and the conversation's
    // cursor does not move -- landing BROWSE on the last row would fight
    // it for both.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    if (oldestPendingPermission(timeline) !== null) {
      // The card landing is one more effect away; a key handled before it yields it the same way.
      landingAtKeyRef.current = typingGuard.keyCount();
      setPermissionRequest((n) => n + 1);
      return;
    }
    const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    const last = Math.max(timeline.length - 1, 0);
    const usable = park !== null && park.tab === activeTabRef.current && park.atKey === typingGuard.keyCount();
    const following = usable
      ? park.following
      : (list === null || list.scrollTop + list.clientHeight >= list.scrollHeight - 1) && cursorRef.current === last;
    setMode("browse");
    if (following) {
      // Only when the cursor really moves -- a `"keep"` set for a `setCursor` that changes nothing
      // fires no `[cursor]` effect, so nothing consumes it, and it swallows the NEXT move's reveal
      // (the same convention the switch restore below follows).
      if (last !== cursorRef.current) landingRef.current = "keep";
      setCursor(last);
      resumeFollowing(list);
      return;
    }
    if (!usable || list === null) return;
    const index =
      (park.cursorKey === null ? null : indexOfKey(timeline, park.cursorKey)) ??
      Math.min(Math.max(park.view.cursor, 0), last);
    if (index !== cursorRef.current) landingRef.current = "keep";
    setCursor(index);
    // Never `park.view.atBottom` here: a park that is not following can still be at the bottom (`k`
    // once from the last row, `gg` in a conversation that fits), and re-arming following would undo
    // the stop that `k`/`gg` made on purpose. Written directly rather than through `restoreRef`, which
    // waits for a switch's snapshot and must not be clobbered by an arrival.
    applyViewScroll(list, park.view, false, true);
    // The row is authoritative: if it is entirely off screen at that scroll (the width changed while
    // the panel was hidden), bring it into view rather than leaving the scroll clamp to move the cursor.
    const rows = conversationRows(list);
    const row = rows[index];
    if (row !== undefined && clampCursorToView(list, rows, index) !== null) revealRow(list, row, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [arriveRequest]);
  /* A stop arrival asks, after the landing above, about the tab and state now on screen: `Stop this turn? (y/n)`
     while a turn runs, else the stop-background question with its own checks (`<leader>k`'s), else a line saying
     nothing runs. It only asks; the answer is the person's own `y`, under the y/n guard. */
  useEffect(() => {
    if (stopAskRequest === 0 || activeTab === null) return;
    const background = isExternalAgent(activeTab.agent)
      ? state.backgroundCalls
      : state.capabilities.backgroundTasks
        ? liveTasks(state.backgroundTasks.rows).length
        : 0;
    const work = workToStop(state);
    const ask = stopAskFor({ live: activeTab.state === "live", turnRunning: work !== null, background });
    if (ask === "turn" && work !== null) askStopTurn(activeTab.id, work);
    else if (ask === "background") askStopBackgroundOfActiveTab();
    else showFlash(NOTHING_TO_STOP);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [stopAskRequest]);
  /* `focus_permission`: BROWSE, with the cursor on the oldest pending card, so
     `a`/`d` answer it at once. `shell` sends it only when it counts a card; if the card was answered
     in between, there is nothing to land on, and the arrival is an ordinary keyboard one -- the
     composer, as `enter_input` gives it. Keyed on the request alone, like `inputRequest`. */
  useEffect(() => {
    if (permissionRequest === 0) return;
    // As in the `arrive` effect: a key handled since `focus_permission` (or `arrive`, which handed its
    // landing on to this effect) arrived wins over the landing. A tab switch's own card landing sets no
    // count and always lands.
    const atKey = landingAtKeyRef.current;
    landingAtKeyRef.current = null;
    if (atKey !== null && atKey !== typingGuard.keyCount()) return;
    // Same reasoning as `arrive`'s
    // own `exitRegion` call just above -- this lands the cursor on a specific card below, directly,
    // and a frozen VISUAL snapshot from a moment ago must not keep showing something else while that
    // happens.
    exitRegion();
    // Same as `arriveRequest` above. Note `focus_permission`'s own dispatch arm
    // already closes the chooser, so this only ever keeps the keys in
    // an open rename field or the `/` prompt.
    if (overlayOpen) {
      takeKeys();
      return;
    }
    const index = oldestPendingPermission(timeline);
    if (index === null) {
      setInputRequest((n) => n + 1);
      return;
    }
    setMode("browse");
    // Landing moves the cursor, and the `[cursor]` effect reveals its row: a scroll the user asked
    // for, announced the way a HINT landing's is (`./follow.ts`). Unannounced, `MessageList` takes it
    // for nobody's and keeps following, and the next delta or resize snaps an older card back out of
    // view.
    noteUserScroll(containerRef.current?.querySelector(".message-list"), "unknown");
    // A landing always reveals the card, even over a `"keep"` some earlier restore left behind; and
    // when the cursor is already on it (a switch restored it there), nothing fires the `[cursor]`
    // effect, so the row is revealed here -- after the restore's own scroll, which is a layout effect.
    landingRef.current = 0;
    // A card is read from its first line: one taller than the view is landed on its head, so the question
    // shows, not only its buttons (and the scroll up that takes the view there ends following with it).
    landingHeadRef.current = true;
    if (index === cursorRef.current) {
      // `exitRegion` above has only asked for the thawed list. If VISUAL was on,
      // the DOM here is still the frozen one and `index` is a live index -- a card that arrived
      // during VISUAL is not in it -- so the reveal waits for the live list to render.
      landingHeadRef.current = false;
      if (frozenSnapshotRef.current !== null) revealAfterThawRef.current = index;
      else {
        const root = containerRef.current;
        const row = root === null ? undefined : conversationRows(root)[index];
        if (row) revealRow(row.closest<HTMLElement>(".message-list"), row, 0, false, true);
      }
    } else setCursor(index);
    containerRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [permissionRequest]);
  /** `focus_permission`'s own reveal, deferred while the list on screen was still the frozen one:
   *  run once the thawed, live list has rendered, and only then indexed by the
   *  live cursor. A layout effect so the reveal lands before that list is painted. */
  useLayoutEffect(() => {
    if (frozenSnapshot !== null) return;
    const index = revealAfterThawRef.current;
    revealAfterThawRef.current = null;
    if (index === null) return;
    const root = containerRef.current;
    const row = root === null ? undefined : conversationRows(root)[index];
    if (row) revealRow(row.closest<HTMLElement>(".message-list"), row, 0, false, true);
  }, [frozenSnapshot]);

  /** The freeze's own backstop: "the effect that
   *  clears the selection whenever mode leaves visual/vline" -- designed once, and at first never
   *  written. Every known route out of VISUAL now calls `exitRegion` explicitly (the routes just above
   *  this file's `sessionEnded`/`arrive`/`focus_permission` effects, and the dispatch arms further
   *  down for `pane_focus`/`hint_collect`/the rename/`confirm_*`/`chooser`); this is what still cleans
   *  up `frozenSnapshot` and the built selection if some OTHER path -- today's or a later change's --
   *  ever moves `mode` away without going through one of them. Idempotent and cheap: `clearVisualLeftovers`
   *  no-ops once everything is already `null`, which is true on every render this effect's own routes
   *  did not just handle, including the very first one. */
  useEffect(() => {
    if (!isRegionMode(mode)) clearVisualLeftovers();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);
  /** The backstop for the other axis: VISUAL selects inside
   *  a live conversation, so the moment no conversation is shown any more (`sessionStarted` false:
   *  the tab went back to `not_started`/`starting`, or failed with nothing kept) it cannot still be
   *  on. The `error`/`handoff` arms call `exitRegion` themselves; this catches any other route that
   *  drops the conversation without touching `mode` -- which is exactly the shape that route had,
   *  and why the `[mode]` backstop above never saw it. A no-op outside VISUAL. */
  useEffect(() => {
    if (!sessionStarted) exitRegion();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionStarted]);

  useWatchFrozenSelection(
    isRegionMode,
    mode,
    messageListEl,
    visualBuiltRef,
    showFlash,
    regionModeName,
    modeRef,
    exitRegion,
  );

  /** The start screen's own DOM anchor -- `HintLayer`'s containing block and the `containerRef ??
   *  startScreenRef` fallback a couple of handlers below use. It used to also be focused whenever
   *  `!sessionStarted` (so `y`, handled by the now-deleted `handleStartScreenKeyDown`, was reachable
   *  without a prior click). `EmptyTab` owns that job for itself now, autofocusing its own live
   *  composer -- and this effect, left in place, stole that focus right back the moment the tab
   *  changed state: the composer's `autoFocus` runs at DOM insertion, but this ran on `sessionStarted`
   *  flipping in a `useEffect`, which fires afterwards and moved focus to the root, blurring the
   *  textarea into `Composer`'s own BROWSE fallback (found while testing the terminal-handoff flow: a
   *  fresh empty tab rendered its composer hint instead of a textarea). */
  const startScreenRef = useRef<HTMLDivElement>(null);

  /* The panel's half of the global `f` HINT.
     `shell` owns the session and every key typed during it -- the panel never sees those keys, so
     there is no HINT key handling here at all. The panel only answers `shell`'s five envelopes:
     report and freeze its visible targets, draw their labels, narrow them, land on one, clear.
     Refs, because the dispatch handler is installed once and must never read stale render state. */
  /** The HINT session this panel is part of, or null. Every envelope naming another one is ignored:
   *  a late `hint_show` from a session `shell` already gave up on must not
   *  draw labels over a panel that has moved on. */
  const hintSessionRef = useRef<number | null>(null);
  /** The targets frozen at `hint_collect`, in the order `shell` addresses them by index. Frozen, not
   *  re-read, so an index means the same element for the whole session even if the DOM changes. */
  const frozenRef = useRef<HintTarget[]>([]);
  const [hints, setHints] = useState<ShownHint[]>([]);
  const [hintTyped, setHintTyped] = useState("");
  /** A `hint_collect` sessionId waiting for the thawed list, the same deferral
   *  `revealAfterThawRef`/`focus_permission` uses: set only when `hint_collect`
   *  arrived while `frozenSnapshotRef.current` was still non-null, consumed by the layout effect just
   *  below `collectHintTargets`'s own declaration. */
  const pendingHintCollectRef = useRef<number | null>(null);
  /** `hint_collect`'s actual target freeze: read the live DOM, remember it by session, tell `shell` how
   *  many labels to draw. Split out of the dispatch arm so both the immediate path (VISUAL was already
   *  off) and the deferred one (below) call the same code. */
  function collectHintTargets(sessionId: number) {
    const root = containerRef.current ?? startScreenRef.current;
    frozenRef.current = root === null ? [] : hintTargets(root);
    hintSessionRef.current = sessionId;
    // A newer session supersedes whatever an older one still had on screen.
    setHints([]);
    setHintTyped("");
    postToRust({
      type: "hint_targets",
      request_id: nextRequestId(),
      session_id: sessionId,
      count: frozenRef.current.length,
    });
  }
  /** The deferred half of `collectHintTargets`: `hint_collect`'s dispatch arm only *asks* `exitRegion`
   *  to thaw the list (mirroring `focus_permission`'s own reveal effect above) -- it
   *  does not repaint synchronously, so collecting from `containerRef.current` right there could still
   *  read the frozen VISUAL DOM and miss a row that arrived during VISUAL (a linked permission card, a
   *  queued prompt): probed at 3 targets with VISUAL framing the collect versus 7 once thawed. Run once
   *  the thawed, live list has rendered -- a layout effect so the freeze lands before HINT's labels are
   *  painted on top of it, same as `revealAfterThawRef`'s own effect. */
  useLayoutEffect(() => {
    if (frozenSnapshot !== null) return;
    const sessionId = pendingHintCollectRef.current;
    pendingHintCollectRef.current = null;
    if (sessionId === null) return;
    collectHintTargets(sessionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [frozenSnapshot]);
  /** A code block HINT landed on: the next `y` copies exactly that block's code rather than the
   *  whole message. Cleared by that copy and by any other key the table resolves. */
  const copyCodeRef = useRef<HTMLElement | null>(null);
  /** The one way `copyCodeRef` is written. Keeps the ref (the dispatch handler and `enterRegion`
   *  read it synchronously) and draws the landing: `data-hint-landed` on the block itself -- it lives in
   *  markdown `innerHTML`, so React cannot render the attribute. (Until 2026-10-13 it also marked the
   *  list, `data-code-landed`, so the row's solid sign cell went hollow beside the outline; the chat
   *  page's cursor is a soft band that the outline is drawn over, so nothing reads that mark.) A block
   *  re-rendered by a streamed delta is a new `pre`: the old one leaves with its attribute, and the ref
   *  already lands nowhere useful then (`isConnected`), as before. */
  function markLandedCode(el: HTMLElement | null) {
    const previous = copyCodeRef.current;
    if (previous === el) return;
    previous?.removeAttribute("data-hint-landed");
    copyCodeRef.current = el;
    el?.setAttribute("data-hint-landed", "");
  }
  // A streamed delta that re-rendered the landed block took its outline with it, and there is
  // nothing left to clear. The ref itself is left as it was (what `y` does with a detached block is
  // unchanged). The landing also ends whenever the cursor is moved off the block's
  // row by anything but a key (which clears it itself) -- an arrival landing on a waiting card, say --
  // or the block would keep its outline while the cursor is elsewhere. An arrival that stays on the row (a park restored, or following on the last row) keeps
  // it, so `y` still copies the block after `Ctrl+h Ctrl+l`.
  useEffect(() => {
    const block = copyCodeRef.current;
    if (block === null || frozenSnapshotRef.current !== null) return;
    if (!block.isConnected) return;
    const root = containerRef.current;
    if (root === null) return;
    if (rowOf(root, block) !== (conversationRows(root)[cursor] ?? null)) markLandedCode(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [timeline, cursor]);
  /** A control HINT landed on, for the `mode` effect above to leave focused: landing from INPUT
   *  blurs the composer, which sets BROWSE, and that effect would otherwise take focus straight back
   *  to the root. Consumed or dropped on the very next commit (the effect just below). */
  const landedControlRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    landedControlRef.current = null;
  });
  /** Where focus was actually put by a key -- an `h`/`l` that moved it, a plain Tab
   *  -- or by a HINT landing, and the `typingGuard.keyCount()` it happened at (a key's own count; a
   *  HINT's labels never reach this page, so the count at the landing). Enter on a card's own button
   *  presses it only when this names that very button at the key right before Enter. Written where
   *  focus moves (`onKeyDown`'s `control` branch, the root's `onFocus`, `landOnHint`), never inferred
   *  from a key's name, and never cleared: any later key makes it stale by its count alone. A ref,
   *  not `landedControlRef` (which every commit clears): `landOnHint` runs from the dispatch
   *  installed once. */
  const placedRef = useRef<{ el: HTMLElement; atKey: number } | null>(null);
  /** The plain Tab `onKeyDown` saw last -- its `keyCount()` and its native event -- until the
   *  focus move it makes, else `null`. The root's `onFocus` records that move as a landing, and
   *  consumes this either way. Only if the Tab's default is still alive when the
   *  focus arrives, read off the event then rather than at the top of `onKeyDown` -- by then every
   *  handler has run, so a Tab one of them swallowed (the `/` or `:` line's, the cancel after a
   *  waiting prefix, a key the leader's sequence does not bind, an overlay's) moved nothing, and the
   *  next focus no key made is not its landing. */
  const tabAtKeyRef = useRef<{ atKey: number; event: globalThis.KeyboardEvent } | null>(null);

  /** Set from this panel posting `hint_request` until the HINT it asked for ends here (`endHint`,
   *  which every `hint_end` and `hint_land` reaches), or the timer that gives up waiting fires.
   *  `shell` attaches the window key controller that swallows HINT keys only once it has handled the script message,
   *  so until then a label typed right after `f` reached THIS panel's key table -- `a`/`d` pressed a permission
   *  card's Approve/Deny (HINT must never act, and its keys must never reach the panel), `i` opened the composer, and
   *  a second `f` asked for a second HINT that cancelled the first. While this is set, the capture-phase listener
   *  below swallows every key before any handler in the panel sees it. Once `shell`'s controller is attached the
   *  panel receives no keys anyway, so this outliving that moment costs nothing. Such a key is dropped, not replayed:
   *  the labels are not drawn yet, so it cannot have been meant for one. */
  const hintPendingRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  function clearHintPending() {
    if (hintPendingRef.current !== null) clearTimeout(hintPendingRef.current);
    hintPendingRef.current = null;
  }
  /** `f`: ask `shell` for a HINT, once. A held `f`'s auto-repeat asks nothing (review: two requests
   *  reaching `shell` toggled the HINT straight back off). */
  function requestHint(repeat: boolean) {
    if (repeat || hintPendingRef.current !== null) return;
    postToRust({ type: "hint_request", request_id: nextRequestId() });
    hintPendingRef.current = setTimeout(() => {
      hintPendingRef.current = null;
    }, HINT_PENDING_TIMEOUT_MS);
  }
  useEffect(() => {
    function swallowWhilePending(event: globalThis.KeyboardEvent) {
      if (hintPendingRef.current === null) return;
      event.preventDefault();
      event.stopImmediatePropagation();
    }
    // Capture phase on the window: ahead of React's own listener, the document-level replay above,
    // and the composer's and every control's own keydown -- including a focused button's native
    // activation, which is the default action this prevents.
    window.addEventListener("keydown", swallowWhilePending, true);
    return () => {
      window.removeEventListener("keydown", swallowWhilePending, true);
      clearHintPending();
    };
  }, []);

  /** The keys overlay, the detail popover and the handoff confirmation: what every route that lands the keys
   *  somewhere else, or opens another overlay over the conversation, closes so the two never fight for the keys
   *  (a keys overlay left up over INPUT swallowed every keystroke). */
  function closeTransientOverlays(keepTasks = false) {
    if (!keepTasks) {
      setTasksOpen(false);
      taskScratchTab.current = null;
    }
    setKeymapOpen(false);
    setDetail(null);
    setHandoffOpen(false);
  }

  useEffect(() => {
    installDispatch(function receive(payload) {
      if (payload.kind === "path_choices") {
        const record = pageRef.current.inFlight.get(payload.requestId);
        if (record?.kind !== "editor") return;
        // A choices envelope is the successful answer to the original editor request.
        receive({ kind: "command_result", requestId: payload.requestId, ok: true });
        const remote = remoteDesk.snapshot();
        const remoteOwnsKeys = remote.listOpen || remote.pairing !== null || remote.questions.length > 0 || remote.flagged.length > 0;
        if (payload.tab === activeTabRef.current && record.tab === payload.tab &&
            modeRef.current === "browse" && !pathChoicesCoveredRef.current && !remoteOwnsKeys &&
            hintPendingRef.current === null && !isEditableElement(document.activeElement)) {
          dropPendingKeys();
          beginPathPick(payload.paths);
          if (payload.stopped != null) showFlash(`gf: ${payload.stopped}; the list may be incomplete`);
          pathChoicesCoveredRef.current = true;
        } else if (payload.tab === activeTabRef.current) {
          showFlash(`gf: ${payload.paths.length} files match; gf again to choose${payload.stopped == null ? "" : ` (${payload.stopped})`}`);
        }
        return;
      }
      if (payload.kind === "command_result" && !payload.ok && payload.requestId === editReturnRef.current?.requestId) {
        editReturnRef.current = null;
      }
      const edit = editReturnRef.current;
      // A waiting question counts as a card to the host. Its own edit comes back via focus_permission even
      // without a permission card; that fallback would focus the hidden composer instead of the answer field.
      const questionReturn = payload.kind === "focus_permission" && payload.tab === edit?.tab && edit?.question !== undefined &&
        pageRef.current.state.pendingPermissions.length === 0 && pageRef.current.state.editorCards.length === 0;
      if (((payload.kind === "arrive" && payload.intent !== "stop") || questionReturn) && edit?.finished && edit.tab === activeTabRef.current) {
        // The landing is the edit's own and is not drawn, but it is still an arrival: a key the person was still
        // pressing as the keys came back answers nothing (INPUT's `Ctrl+y`, the question dock), as after any other.
        noteArrival();
        edit.landingHandled = true;
        if (edit.resumed) editReturnRef.current = null;
        return;
      }
      if (questionDock.current?.reply(payload)) return;
      if (payload.kind === "question_other_edited") return;
      if ((payload.kind === "task_output_result" || payload.kind === "command_result") && taskRequests.current.has(payload.requestId)) {
        const tab = taskRequests.current.get(payload.requestId);
        taskRequests.current.delete(payload.requestId);
        if (tab === activeTabRef.current) {
          tasksRef.current?.reply(payload.kind === "task_output_result" ? payload :
            { requestId: payload.requestId, ...(!payload.ok ? { error: payload.error } : {}) });
        }
        return;
      }
      if ((payload.kind === "models_result" || payload.kind === "settings_result" || payload.kind === "command_result") && settingsRequests.current.has(payload.requestId)) {
        const request = settingsRequests.current.get(payload.requestId)!;
        settingsRequests.current.delete(payload.requestId);
        answerSettings(request, payload);
        return;
      }
      if (payload.kind === "path_completions") {
        if (payload.seq !== atLastAsked.current.get(payload.tab)) return;
        setAtPathsByTab((previous) => new Map(previous).set(payload.tab, { items: payload.items, seq: payload.seq }));
        return;
      }
      // Every session-scoped envelope names its tab, and the page keeps only the active tab's state, so one for
      // any other tab -- a batch still in flight when the user switched away -- is dropped before any arm below
      // can touch the page.
      if (!acceptsEnvelope(payload as { kind: string; tab?: number }, activeTabRef.current)) return;
      const receivedAt = performance.now();
      const previousPage = pageRef.current;
      const nextPage = pageModel(previousPage, { ...payload, receivedAt: Date.now(), monotonicAt: receivedAt });
      if (nextPage.tabs !== null && (payload.kind === "tabs" || payload.kind === "snapshot" ||
          nextPage.state.editorCards !== previousPage.state.editorCards)) {
        const live = new Set(nextPage.tabs.tabs.map((tab) => tab.id));
        if (payload.kind === "tabs") {
          atLastAsked.current = pruneTabEntries(atLastAsked.current, live);
          setAtPathsByTab((previous) => pruneTabEntries(previous, live));
        }
        // A switch clears the page before its snapshot arrives; that empty state says nothing about its cards.
        cardViews.current = pruneCardViews(cardViews.current, live, payload.kind === "tabs" ? undefined : {
          tab: nextPage.tabs.active,
          pending: new Set(nextPage.state.editorCards.map((card) => card.permissionId)),
        });
      }
      // The refs at once, not at the next render: the next envelope of this batch (an `error` for the tab just
      // switched to, a snapshot after `tabs`) must start from this one's result.
      pageRef.current = nextPage;
      stateRef.current = nextPage.state;
      setPage(nextPage);
      // Redirects cancel synchronously too: their return envelopes can share one render with the scratch end.
      if (nextPage.chooser !== null || nextPage.renaming !== null || nextPage.confirm !== null || nextPage.review !== null ||
          payload.kind === "open_keymap" || payload.kind === "open_command_line" || payload.kind === "tab_detail" ||
          payload.kind === "hint_collect") editReturnRef.current = null;
      // Envelopes can arrive in one batch before React renders the overlay that takes the keys.
      if (nextPage.chooser !== null || nextPage.renaming !== null || nextPage.confirm !== null || nextPage.review !== null ||
          payload.kind === "open_keymap" || payload.kind === "open_command_line" || payload.kind === "tab_detail" ||
          payload.kind === "hint_collect" || payload.kind === "enter_input" || payload.kind === "focus_permission") {
        pathChoicesCoveredRef.current = true;
      }
      // A waiting `a`/`d`/`D` was aimed at a card these envelopes take the keys or the view from. Cancelled here,
      // in the same task as the envelope, so its timer can never come due in between and answer a card that is
      // no longer the one in front of the user.
      if (nextPage.answerEpoch !== previousPage.answerEpoch) typingGuard.cancel();
      if (CANCELS_WAITING_ANSWER.has(payload.kind)) {
        // A held `j`/`k`'s glide stops where it is: the keys, or the view, are someone else's now.
        const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null) cancelHold(list);
      }
      switch (payload.kind) {
        case "theme": {
          applyTheme(payload.vars);
          break;
        }
        case "display": {
          // The model keeps the level it has for a word this build does not know.
          if (!(THINKING_DISPLAYS as readonly string[]).includes(payload.thinking)) {
            console.warn("agent-ui: a display envelope named an unknown thinking level", payload.thinking);
          }
          break;
        }
        case "editor_typing": {
          // Window-level and Rust's alone (a cadence slower than the meter's step): it slows the meter, never
          // pauses it. Not among the envelopes that cancel a waiting answer or touch any other state -- it is not
          // about the panel's own keys.
          applyEditorTyping(document.documentElement, payload.typing, payload.periodMs);
          break;
        }
        case "pane_focus": {
          if (!payload.focused && editReturnRef.current?.resumed) editReturnRef.current = null;
          // The keys leaving can be the first half of the tab leaving this page (a join back into the base page
          // follows at once); a draft still waiting out its debounce would then be posted by a page that no
          // longer shows the tab, so it goes out now.
          if (!payload.focused) flushDraft();
          // A pending prefix is for the very next key; a pane switch in between (GTK takes `Ctrl+h`/`Ctrl+k`
          // before the WebView sees a keydown) must not leave it armed for a key pressed much later, when it
          // would complete a chord nobody meant to start. The leader engine's pending sequence, the box waiting
          // to show it, and a count (which would multiply a `j` pressed after coming back) go the same way.
          dropPendingKeys();
          // The keys overlay has no reason to stay drawn while another pane has the keys; left up, coming back
          // with `Ctrl+l` landed in INPUT with every keystroke swallowed by the overlay. So the keys leaving close
          // it, and the keys arriving do not: `prefix ?` from another module sends `open_keymap` before the focus
          // report that the chat now holds the keys, and closing here would take the sheet down as it opens, with
          // the way back to that module. The review overlay is not closed here (see the model's `pane_focus` arm).
          if (payload.focused) {
            setDetail(null);
            setHandoffOpen(false);
          } else closeTransientOverlays(taskScratchTab.current === activeTabRef.current && taskScratchTab.current !== null);
          // The `/` prompt and the `:` line are this panel's own, like the keys overlay.
          setSearch(null);
          setExLine(null);
          // A Tab whose focus move never landed inside this page (it left for another widget) must not make
          // whatever this page focuses on the way back a landing.
          tabAtKeyRef.current = null;
          // Losing focus is a route away from whatever this panel was showing, the bypass prompt included;
          // regaining it is not (this pane's own keydowns are the only thing that answers the prompt, and those
          // never fire while it is unfocused anyway).
          if (!payload.focused) cancelBypassConfirm();
          // Losing focus ends VISUAL; nothing else here sets `mode`.
          if (!payload.focused) exitRegion();
          // The keys leave: park where the reader is, after `exitRegion` so a VISUAL mode is never what is
          // parked. Only over a conversation: the empty tab lands its own way.
          if (!payload.focused) {
            const tab = activeTabRef.current;
            const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            if (tab !== null && list !== null) {
              const view = captureView(list);
              const items = timelineRef.current;
              arrivalParkRef.current = {
                tab,
                view,
                cursorKey: items[view.cursor]?.key ?? null,
                following: view.atBottom && view.cursor === items.length - 1,
                atKey: typingGuard.keyCount(),
              };
            } else arrivalParkRef.current = null;
          }
          // Only regaining focus is a request for the keys back. WebKitGTK's DOM focus across the round trip is
          // not guaranteed to have survived (`takeKeys`), so this is the one place that asks.
          if (payload.focused) setKeysRequest((n) => n + 1);
          break;
        }
        case "enter_input": {
          editReturnRef.current = null;
          // This puts the caret in the composer, so no overlay may be left covering it and eating what gets
          // typed. Its only sender is a brand-new tab, so there is never a card waiting to land on instead.
          closeTransientOverlays();
          endKeyPrompts();
          // This lands INPUT, never on a parked row.
          arrivalParkRef.current = null;
          setEmptyLanding("input");
          break;
        }
        case "arrive": {
          editReturnRef.current = null;
          // A scratch return keeps the task detail; other arrivals land on the conversation in BROWSE.
          closeTransientOverlays(taskScratchTab.current === activeTabRef.current && taskScratchTab.current !== null);
          taskScratchTab.current = null;
          endKeyPrompts();
          // A reserved two-key prefix or a leader sequence armed before this arrival means nothing about it.
          dropPendingKeys();
          setEmptyLanding("browse");
          noteArrival();
          landingAtKeyRef.current = typingGuard.keyCount();
          // Brought forward to be stopped: the chat asks (the effect after the landing); nothing stops here.
          if (payload.intent === "stop") setStopAskRequest((n) => n + 1);
          break;
        }
        case "focus_permission": {
          editReturnRef.current = null;
          // An overlay or landing that takes the keys ends a pending sequence, box and reserved prefix included,
          // so a key the overlay lets bubble can never advance a stale one. The overlays would cover the card the
          // cursor is about to land on.
          dropPendingKeys();
          closeTransientOverlays();
          endKeyPrompts();
          // A card landing wins over a parked row.
          arrivalParkRef.current = null;
          noteArrival();
          landingAtKeyRef.current = typingGuard.keyCount();
          break;
        }
        case "nav_key": {
          // A key, claimed by GTK before the WebView saw it, so it never reached `answerConfirm`: "any key but a
          // counted y cancels" is applied here instead. It is also a key pressed after any pending
          // `g`/`z`/`[`/`]` prefix or leader sequence, cancelled for the reason `pane_focus` cancels them -- left
          // armed, `g`, Ctrl+j, Ctrl+k, `g` ran a stale `gg`. Whether the chord is claimed or answered with
          // `nav_fallthrough` is decided in an effect against the current render.
          endKeyPrompts();
          dropPendingKeys();
          break;
        }
        case "keymap": {
          // A table that changed while a sequence was pending must never let the old table's binding run against
          // the new one. A deferred single-key binding was cancelled above for the same reason.
          clearSequence();
          break;
        }
        case "literal_key": {
          // `send-prefix`/`send-keys` from shell: WebKitGTK cannot be handed the key itself. `C-a` is what a text
          // field does with it -- select all of the focused one; any other key is ignored. Not
          // `isEditableElement`: this needs "has a text selection", which is exactly these two types. A key GTK
          // claimed, so the y/n cancel is applied here, as for `nav_key`.
          endKeyPrompts();
          if (payload.key !== "C-a") return;
          const el = document.activeElement;
          if (el instanceof HTMLTextAreaElement || (el instanceof HTMLInputElement && el.type === "text")) {
            el.select();
          }
          break;
        }
        case "open_keymap": {
          // `prefix ?` is GTK's chord, so no key of it reaches `answerConfirm` or VISUAL's own handler: a route
          // away from a y/n prompt and from VISUAL here. The start screen draws the overlay too.
          endKeyPrompts();
          exitRegion();
          setMode("browse");
          setKeymapPlugin(pluginKeysHelp(payload.plugin));
          keymapOpening.current = typeof payload.opening === "number" ? payload.opening : null;
          keymapClosedByKey.current = false;
          setKeymapRequest((n) => n + 1);
          setKeymapOpen(true);
          break;
        }
        case "open_command_line": {
          // `prefix :`, tmux's `command-prompt`. GTK took the chord, and before it was bound the armed prefix
          // swallowed the `:` and the letters after it ran as panel keys ("kill-window -a" moved the cursor,
          // entered INPUT and sent "ll-window -a"). This opens the same line BROWSE's own `:` opens: it runs
          // nothing, Enter says so, Esc closes it, and it has the keys -- on the start screen too. A route away
          // from every y/n prompt and `gf` picker, from VISUAL and from every overlay over the conversation; the
          // `/` prompt and this line are one command line, so opening one closes the other.
          endKeyPrompts();
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          dropPendingKeys();
          setMode("browse");
          setSearch(null);
          setExLine("");
          setLineFocusRequest((n) => n + 1);
          break;
        }
        case "hint_collect": {
          // A HINT ends VISUAL, and closes what would hide rows under it or take the keys its labels are about to
          // swallow. The `/model`/`/effort` picker too, open or on its way: a reply landing under the labels used
          // to open it there, and landing on the composer then left the keys on the picker.
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          // The labels and the label keys are GTK's, never a keydown here, so the next ordinary key after a
          // landing (the `y` that copies the code block just landed on, the first letter typed into the composer)
          // answered a y/n prompt left open under it. Rust drops its own prompt too.
          endKeyPrompts();
          setSearch(null);
          setExLine(null);
          dropPendingKeys();
          // `exitRegion` has only asked for the thawed list to render, so the DOM here can still be the frozen
          // VISUAL one, which misses any row that arrived during VISUAL (a linked permission card, a queued
          // prompt). Collecting then waits for the layout effect that runs once it is thawed.
          if (frozenSnapshotRef.current !== null) pendingHintCollectRef.current = payload.sessionId;
          else collectHintTargets(payload.sessionId);
          break;
        }
        case "hint_show": {
          if (payload.sessionId !== hintSessionRef.current) return;
          const frozen = frozenRef.current;
          setHints(payload.labels.slice(0, frozen.length).map((label, i) => ({ target: frozen[i], label })));
          setHintTyped("");
          break;
        }
        case "hint_prefix": {
          if (payload.sessionId !== hintSessionRef.current) return;
          setHintTyped(payload.typed);
          break;
        }
        case "hint_land": {
          if (payload.sessionId !== hintSessionRef.current) return;
          landOnHint(frozenRef.current[payload.index]);
          endHint();
          break;
        }
        case "hint_end": {
          if (payload.sessionId !== hintSessionRef.current) return;
          endHint();
          break;
        }
        case "tabs": {
          // A switch to a different tab clears every piece of the page's own per-conversation state (the model
          // clears its part) before the new tab's snapshot arrives. The first `tabs` this page sees takes this
          // branch too, since `activeTabRef.current` starts `null`.
          if (payload.active !== activeTabRef.current) {
            editReturnRef.current = null;
            setTasksOpen(false);
            taskScratchTab.current = null;
            switchedRef.current = true;
            // A switch ends VISUAL, and first: `saveView` below reads `modeRef.current` to park the old tab's
            // mode, and `exitRegion` sets that ref synchronously, so a mode this page cannot re-enter is never
            // what gets saved.
            exitRegion();
            // A sequence pending in the old tab's conversation means nothing about the new one.
            dropPendingKeys();
            // The tab stops being active, so whatever has not yet been mirrored goes now rather than waiting out
            // the rest of its debounce against a tab nobody is reading.
            flushDraft();
            const previous = activeTabRef.current;
            // A `j`/`k` still easing the view is finished first: the old tab's saved view is where that press was
            // going, and the ease must not go on carrying the new tab toward the old one's target.
            const leftList = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
            if (leftList !== null) settleListScroll(leftList);
            // The old tab's view is saved from the live refs, before the resets below overwrite the render state
            // they mirror, and only when there is an old tab.
            if (previous !== null) {
              saveView(viewStore.current, previous, captureView(leftList));
            }
            // An arrival park names a row of the tab just left.
            arrivalParkRef.current = null;
            restoreRef.current = takeView(viewStore.current, payload.active) ?? null;
            // The box goes empty on a switch, never the left tab's words: Rust's own `draft` envelope for the new
            // tab follows in the same batch, so this is never what the reader sees for longer than one dispatch.
            restoreSeq.current += 1;
            draftRef.current = "";
            setRestoredDraft({ text: "", seq: restoreSeq.current });
            setCursor(0);
            setExpanded({});
            setDetailed(false);
            // A switch always lands BROWSE, never whatever mode the tab was left in; nothing later in the switch
            // (the `snapshot` arm's restore included) may set it back to INPUT. The first `tabs` is no switch:
            // launch keeps its INPUT.
            setMode("browse");
            if (previous !== null) setEmptyLanding("browse");
            setKeymapOpen(false);
            setDetail(null);
            setHandoffOpen(false);
            // An open `/model`/`/effort` picker answers through `chooseSlashOption`, which sends to whatever tab
            // is active: left open across a switch, a row chosen from it went to the new, wrong tab. The armed
            // reply is a single flag, not scoped to a tab, and the old tab's reply is now dropped as another
            // tab's, so it is cleared here too rather than matched against this tab's next `turn_completed`.
            setSlashPicker(null);
            setSlashReply(null);
            pendingSlashPickerRef.current = null;
            // A `gf` or `gx` pick names paths or links of the old tab's conversation; left set, its next letter
            // opened one of them whichever tab had the keys.
            setPathPick(null);
            setLinkPick(null);
            // A y/n prompt, the `/` prompt and the `:` line were over the old tab's conversation.
            cancelBypassConfirm();
            setSearch(null);
            setExLine(null);
            answeredRef.current = new Set();
            setAnsweredPermissions(answeredRef.current);
            permissionReasons.current.clear();
            // The old tab's unread pill means nothing over a fresh, empty timeline. The `snapshot` arm's restore
            // seeds a real threshold back only for a tab whose saved view was actually parked.
            setUnreadLabel(null);
            unreadJumpRef.current = () => { };
            unseenAfterSeqRef.current = null;
          }
          activeTabRef.current = payload.active;
          forgetClosed(
            viewStore.current,
            payload.tabs.map((t) => t.id),
          );
          break;
        }
        case "tab_detail": {
          // The reply to `open_detail` (the band, `prefix i`): the popover opens on this tab's rows with the
          // cursor at the top, in BROWSE, and the keys overlay closes -- the two are mutually exclusive over the
          // conversation. A route away from a y/n prompt (the popover's own `y` copies a row) and from VISUAL.
          endKeyPrompts();
          exitRegion();
          setDetail(payload.rows);
          setDetailCursor(0);
          setMode("browse");
          setKeymapOpen(false);
          break;
        }
        case "snapshot": {
          const arrived = arrivedTabsRef.current.delete(payload.tab);
          // The view a switch saved for this tab, if it had one: cursor and expansions now (never the mode: the
          // switch lands BROWSE), the scroll position in the layout effect keyed on `restoreTick`, since it needs
          // the row this cursor lands on to exist in the DOM first.
          if (restoreRef.current !== null) {
            const view = restoreRef.current;
            // `landingRef` is set before the cursor moves so the `[cursor]` reveal effect does not fight this
            // restore with its own scroll -- and only when the cursor really moves: a `"keep"` for a move that
            // changes nothing fires no effect, so nothing consumes it, and it swallowed the next move's reveal. A
            // view following the end lands on the newest row after a plain switch. An arrival already carries
            // its current rows, so it keeps the cursor from the other window while the scroll follows the end.
            if (!view.atBottom || arrived) {
              if (view.cursor !== cursorRef.current) landingRef.current = "keep";
              setCursor(view.cursor);
            }
            setExpanded(view.expanded);
            setDetailed(view.detailed);
            setRestoreTick((n) => n + 1);
            // A parked view whose unread threshold was saved gets it seeded back into `MessageList`. A counter
            // of its own, not `restoreTick`: a `null -> value -> null -> value` sequence across three switches
            // must still re-fire the seed effect on the third.
            if (!view.atBottom && view.unseenAfterSeq !== null) {
              unseenSeedTick.current += 1;
              setUnseenSeed({ afterSeq: view.unseenAfterSeq, tick: unseenSeedTick.current });
            }
          }
          // A switch that lands on a tab already holding a card, while the panel has the keys, puts them on it --
          // the landing `Ctrl+l` gives within one tab. Read off the envelope (the timeline still reflects the old
          // tab until this batch renders); the `permissionRequest` effect finds the row once it does. A tab with
          // no saved view (a reload, a first mount, a tab never shown here) opens at the bottom, so the cursor
          // goes to the last row, where the view is; the card landing still wins over it.
          if (switchedRef.current) {
            switchedRef.current = false;
            landOnLastRef.current = restoreRef.current === null || (restoreRef.current.atBottom && !arrived);
            if (paneFocusedRef.current && (payload.state.pendingPermissions.length > 0 || (payload.state.editorCards?.length ?? 0) > 0)) setPermissionRequest((n) => n + 1);
          }
          break;
        }
        case "events": {
          const wakes = payload.events.filter((e) => e.type === "cli_cycle_started").length;
          if (wakes) tasksRef.current?.wake(wakes);
          // A new turn resets the render trace: each turn reports its own first text, once.
          if (payload.events.some((e) => e.type === "turn_started")) {
            firstTextReceivedAt.current = null;
            renderReportSent.current = false;
          }
          // A sent prompt takes the cursor once its row exists (the `[timeline]` effect reads and clears this).
          if (payload.events.some((e) => e.type === "user_prompt_submitted")) landOnPromptRef.current = true;
          for (const event of payload.events) {
            // The turn right after a bare `/model` or `/effort` send is the one candidate for a picker: armed by
            // `sendMessage`, read and cleared on the very next `turn_completed`, parsed either way, so a later
            // turn is never mistaken for the reply. `result_text` is the one field both backends fill with a
            // local command's reply (on the sidecar backend it otherwise arrives as ordinary assistant text). The
            // picker opens through `slashReply`, whose effect waits until no other overlay is open.
            if (event.type === "turn_completed") {
              const pending = pendingSlashPickerRef.current;
              pendingSlashPickerRef.current = null;
              if (pending === "model") {
                const parsed = parseModelReply(event.result_text);
                if (parsed !== null) setSlashReply({ kind: "model", options: parsed.options, current: parsed.current });
              } else if (pending === "effort") {
                const parsed = parseEffortReply(event.result_text);
                // The reply names no level; the tab's known one marks the picker when it is one of the choices.
                const known = stateRef.current.effort;
                if (parsed !== null) {
                  setSlashReply({ kind: "effort", options: parsed.options, current: known !== null && parsed.options.includes(known) ? known : null });
                }
              }
            }
          }
          // Stamped at receipt, so the span covers the work being measured rather than starting after it.
          if (
            firstTextReceivedAt.current === null &&
            payload.events.some((e) => e.type === "content_delta" && e.kind === "text" && e.text !== "")
          ) {
            firstTextReceivedAt.current = receivedAt;
          }
          break;
        }
        case "command_result": {
          setPendingCommands((prev) => {
            const next = new Set(prev);
            next.delete(payload.requestId);
            return next;
          });
          // The record as it was before this envelope ended it; the model has already put the banner, the
          // overlay's status and the handoff bookkeeping where they go (see `CommandRecord`).
          const record = previousPage.inFlight.get(payload.requestId);
          if (!payload.ok) {
            console.warn("agent-ui: command failed", payload.requestId, payload.error);
            if (record?.kind === "trust" || record?.kind === "review-command" || record?.review === true || record?.kind === "review") break;
            if (record?.kind === "picker-send") {
              // The picker's choice never touched the box, so nothing is put back: a footer flash alone, leaving
              // a draft already in the box exactly as the user set it.
              pendingSlashPickerRef.current = null;
              showFlash(payload.error);
            } else if (record?.kind === "send") {
              // A refused send never started a turn, so a picker it armed must not wait to be matched against
              // some later, unrelated reply -- whichever tab the refusal names, since the flag is not per tab.
              pendingSlashPickerRef.current = null;
              if (record.tab === activeTabRef.current) {
                // The composer cleared this optimistically; Rust refused it, so it goes back. `draftRef` names
                // exactly what the box holds now: if nothing was typed since the clear the refused text comes back
                // alone, else it goes before what was typed, separated by a newline, so neither is lost. Mirrored
                // to Rust here, once, so a later switch brings the recovered text back too (not in the
                // composer's restore effect, which also fires on a switch's clear and on Rust's own echo).
                const typedSince = draftRef.current;
                const recovered = typedSince === "" ? (record.text ?? "") : `${record.text ?? ""}\n${typedSince}`;
                restoreSeq.current += 1;
                setRestoredDraft({ text: recovered, seq: restoreSeq.current });
                mirrorDraft(recovered);
                refusedEchoRef.current = { tab: record.tab, requestId: payload.requestId, at: performance.now() };
              }
            } else if (record?.kind === "editor" || record?.kind === "link" || record?.kind === "tabnew" || record?.kind === "stop-background" || record?.kind === "follow" || record?.kind === "reset") {
              // A footer nicety, not the banner reserved for conversation-breaking errors.
              showFlash(payload.error);
            } else {
              // Refused, so not answered: Rust leaves the card waiting, and so must this panel -- otherwise `a`/`d`
              // and the card's own buttons stay dead until a reset.
              if (record?.kind === "permission" && record.permissionId !== undefined && record.tab === activeTabRef.current) {
                const permissionId = record.permissionId;
                if (answeredRef.current.has(permissionId)) {
                  const next = new Set(answeredRef.current);
                  next.delete(permissionId);
                  answeredRef.current = next;
                  setAnsweredPermissions(next);
                }
              }
            }
          }
          break;
        }
        case "handoff":
        case "error": {
          // The model resets the conversation, which neither ends the session as the page sees it nor changes
          // `mode`, so VISUAL -- and the conversation it froze -- would outlive what it was selecting in: the
          // restarted tab drew under the old frozen conversation (its cards included) until an Esc, and the empty
          // tab refused Shift+Tab as "Esc first" with no VISUAL handler left to take that Esc. After a handoff the
          // session really is closed, so there is nothing left to confirm either.
          exitRegion();
          setKeymapOpen(false);
          setHandoffOpen(false);
          break;
        }
        case "begin_rename": {
          // `prefix ,`: an inline field over this tab, prefilled and selected. Like every overlay that takes the
          // keys it ends a pending sequence and VISUAL and closes the other overlays. `prefix ,` is GTK's, so a
          // name starting with `y` typed into the field answered a y/n prompt left open; Rust drops its own too.
          // The `/` prompt and the `:` line close for the chooser's reason: committing or cancelling hands the
          // keys to the conversation root.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          endKeyPrompts();
          setSearch(null);
          setExLine(null);
          break;
        }
        case "confirm_close":
        case "confirm_close_others":
        case "confirm_bypass":
        case "confirm_restore": {
          // Drawn in the footer in place of the which-key strip, taking every key (`onKeyDown` checks it before
          // every other overlay). An overlay that takes the keys ends a pending sequence, and VISUAL, whose own
          // `y` would otherwise copy instead of answering. The chooser is left as the model leaves it: open under
          // a bypass prompt.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          break;
        }
        case "chooser": {
          // `prefix w`: ends a pending sequence and VISUAL, and closes the overlays that would fight it for the
          // keys -- the `/model`/`/effort` picker too, open or still on its way, since the bare command it waits
          // for was sent before the user turned to something else.
          dropPendingKeys();
          exitRegion();
          closeTransientOverlays();
          setSlashPicker(null);
          setSlashReply(null);
          pendingSlashPickerRef.current = null;
          // Opening the chooser ends a y/n prompt; the reverse is not true (a bypass prompt opens over it).
          endKeyPrompts();
          // Every way out of the chooser hands the keys to the conversation root, so a `/` prompt or `:` line
          // left open under it stayed drawn without the keys: `:`, then `prefix w` and Esc, then `l` Enter
          // walked onto Approve and pressed it.
          setSearch(null);
          setExLine(null);
          break;
        }
        case "notice": {
          // A notice can carry a name a phone claimed or an agent's words: its hidden characters are shown.
          showFlash(revealedText(payload.text));
          break;
        }
        case "view_export": {
          // A move to the other window takes this tab: its view goes with it, read from the live list when it is
          // the tab on screen, else the one a switch parked.
          const list = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
          if (list !== null && payload.tab === activeTabRef.current) settleListScroll(list);
          const view = exportView(viewStore.current, activeTabRef.current, payload.tab, () => captureView(list));
          // What was typed into the box, or chosen in a waiting question, and is still waiting out its mirror's
          // debounce goes ahead of the answer: the tab freezes once the answer is in, and a mirror sent after that
          // reaches a tab that is no longer this window's, so the other window would get it without that edit.
          flushDraft();
          if (payload.tab === activeTabRef.current) questionDock.current?.flush();
          postToRust({ type: "view_exported", request_id: nextRequestId(), request: payload.request, view });
          break;
        }
        case "view_import": {
          // The tab arrived from the other window: shown here as it was left there, once a switch shows it.
          arrivedTabsRef.current.add(payload.tab);
          if (!importView(viewStore.current, payload.tab, payload.view)) {
            console.warn("agent-ui: the moved tab's view was not one to keep", payload.tab);
          }
          break;
        }
        case "scratch": {
          const edit = editReturnRef.current;
          if (edit?.tab === payload.tab) {
            if (payload.editing) edit.started = true;
            else if (edit.started) edit.finished = true;
          }
          break;
        }
        case "draft": {
          // The page this tab just left sent its last mirror after the tab arrived here, and the box has been
          // edited since: that edit is newer, and its own mirror tells Rust so.
          if (payload.late === true && !(givenDraftRef.current?.tab === payload.tab && draftRef.current === givenDraftRef.current.text)) break;
          // The echo of a refused send this page put back itself: Rust's copy can trail what was typed since, and
          // the box already holds the message and everything typed after it.
          if (takeRefusedEcho(payload.tab, payload.refused)) break;
          // Otherwise Rust's copy wins; it is sent only on a switch, `ready`, a reset or a scratch return.
          restoreSeq.current += 1;
          draftRef.current = payload.text;
          givenDraftRef.current = { tab: payload.tab, text: payload.text };
          pendingDraftRef.current = null;
          setRestoredDraft({ text: payload.text, seq: restoreSeq.current });
          break;
        }
      }
    });
    requestHello();
    // The host keeps this view nearly hidden until the page says it has painted, so WebKit's first
    // buffer, drawn before the page has anything in it, never shows. The second frame callback runs
    // once a frame holding the mounted page has been produced.
    let paintFrame = requestAnimationFrame(() => {
      paintFrame = requestAnimationFrame(() => postToRust({ type: "painted" }));
    });
    // A page mounted again in the same document (a dev reload) starts unslowed; a real reload is a
    // new document, which has no attribute to begin with, and Rust tells it again on `ready`.
    return () => {
      cancelAnimationFrame(paintFrame);
      applyEditorTyping(document.documentElement, false, 0);
    };
  }, []);

  /** The composer mirror. `browse`/`input` mean a live BROWSE/INPUT (this tab's
   *  own `mode`) or the empty tab's menu/composer (`emptyMode`) -- there is no overlay of any kind
   *  drawn over either; everything else, an ended session included, folds into `other`. Recomputed
   *  every render (a plain `const`, not its own `useState`), so the effect below -- which posts only
   *  when it actually changed -- always compares against this render's true value rather than a
   *  copy that could itself go stale. Declared after the `ready`-posting mount effect above, on
   *  purpose: React fires a component's own effects in source order, and `ready` must lead every
   *  other post (`App handshake`'s own test), including this mirror's first one.
   *  Before the first `tabs` envelope, `activeTab` is `null` and the
   *  render below shows only "Connecting to the shell…" -- `EmptyTab` is not mounted yet, so there
   *  is no box of any kind (`emptyMode`'s "input" default was never a real composer here). That case
   *  must fold into `other`, the same as every other screen with no box, or Rust would claim `Ctrl+k`
   *  for a composer that does not exist. */
  const panelKeysMode: PanelKeysMode =
    overlayOpen || keymapOpen || detail !== null || confirm !== null || pathPick !== null || linkPick !== null
      ? "other"
      : sessionStarted
        ? sessionEnded
          ? "other"
          : mode === "input"
            ? "input"
            : "browse"
        : activeTab === null
          ? "other"
          : emptyMode === "input"
            ? "input"
            : "browse";
  useEffect(() => {
    postToRust({ type: "panel_keys", request_id: nextRequestId(), mode: panelKeysMode });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelKeysMode]);
  /** "Stale mirror, both ways": every `nav_key` this page cannot apply comes back
   *  as one `nav_fallthrough`, so Rust runs the chord's ordinary `move_focus` instead of dropping the
   *  key. `keymapOpen`/`confirm` are checked here rather than left to `EmptyTab`:
   *  they are this component's own state, drawn over its layout, and `EmptyTab` has no prop carrying
   *  either -- forwarding `navKey` to it unfiltered would let it act on a request this effect has
   *  already answered. `detail`/`pathPick`/`linkPick` are live-tab-only by construction (none ever renders
   *  while `!sessionStarted`), so they are read only inside that branch below. */
  useEffect(() => {
    if (navKey === null) return;
    const { direction } = navKey;
    if (overlayOpen || keymapOpen || confirm !== null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // Before the first `tabs` envelope there is no `EmptyTab` to
    // hand this to -- it is not mounted yet (the "Connecting to the shell…" div above is), so
    // queuing into `emptyNavKey` would only seed its `navKeySeenRef` at mount and silently drop the
    // key for good. Answer with a fallthrough instead, exactly as every other boxless screen does.
    if (activeTab === null) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    if (!sessionStarted) {
      setEmptyNavKey(navKey);
      return;
    }
    if (detail !== null || pathPick !== null || linkPick !== null || sessionEnded) {
      postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
      return;
    }
    // `Ctrl+j` is one of INPUT's own entry routes, and every INPUT route
    // ends the region (CARET/VISUAL/V-LINE alike). `mode ===
    // "browse"` alone left VISUAL/V-LINE out of this branch, so `Ctrl+j` there fell all the way to
    // the stale-mirror fallback below and ran Rust's ordinary `move_focus` instead -- the same
    // chord did something else entirely depending on which mode happened to be on.
    if (direction === "down" && (mode === "browse" || isRegionMode(mode))) {
      if (isRegionMode(mode)) exitRegion();
      // The `i`/`o` route: caret "kept", the same rule `Ctrl+j` follows
      // ("caret where it was left"). A bare `setMode("input")` would leave `Composer` reading
      // whatever `composerCaret` was last set to by an unrelated `A` press, not this chord's own
      // rule (reproduced with a scratch test -- a caret left at 3
      // landed at the end of the draft instead, because the last `mode` action before this chord
      // had been `A`, not `i`/`o`).
      setMode("input");
      setComposerCaret("kept");
      setComposerFocusRequest((n) => n + 1);
      return;
    }
    if (direction === "up" && mode === "input") {
      setMode("browse");
      return;
    }
    // Stale: the mirror said browse/input but this render's `mode` no longer agrees.
    postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [navKey]);

  /** Clears every trace of a HINT session: labels, prefix, the frozen list and the session itself,
   *  so no later envelope for it does anything. */
  function endHint() {
    clearHintPending();
    frozenRef.current = [];
    hintSessionRef.current = null;
    setHints([]);
    setHintTyped("");
  }

  /** Moves the keys to `target` and does nothing else -- HINT never acts:
   *  a button is focused, never clicked, and `Enter` is what presses it afterwards. A target whose
   *  element has left the DOM since it was frozen lands nowhere rather than somewhere else. */
  function landOnHint(target: HintTarget | undefined) {
    markLandedCode(null);
    if (target === undefined || target.kind === "path" || !target.el.isConnected) return;
    const root = containerRef.current;
    // Landing can scroll the conversation (a focused control, the cursor's row revealed), and that
    // scroll is the user's -- see the note on `noteUserScroll` in `onKeyDown`.
    noteUserScroll(root?.querySelector(".message-list"), "unknown");
    if (target.kind === "composer") {
      // The same route `Ctrl+l` takes (`inputRequest`): INPUT, with the caret in the box. Entering a
      // text box is what landing on one means; nothing is typed or sent.
      setInputRequest((n) => n + 1);
      return;
    }
    if (target.kind === "control" || target.kind === "link") {
      // An input (a permission card's reason box) takes the keys by being focused; a button is
      // selected by being focused, drawn as the solid cursor block. A web link is
      // the same landing: focused, never clicked, its row current -- and Enter on it is then the browser's
      // own activation (`LinkClicked`), not a key this panel claims.
      // A control inside a conversation row (a card's Approve) also brings the row cursor to that
      // row, the same place `l` would have reached it from: otherwise `h` from it would hand the
      // keys back to some other row, and `a`/`d` would answer a different card.
      const row = root === null ? null : rowOf(root, target.el);
      const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
      if (rowIndex !== null) setCursor(rowIndex);
      landedControlRef.current = target.el;
      setMode("browse");
      target.el.focus();
      // A landing -- the Enter right after it may press a card's button. Only if
      // focus took (a disabled control takes none); the labels never reach this page, so the count
      // now is the count the next key reads as "the key before".
      if (document.activeElement === target.el) placedRef.current = { el: target.el, atKey: typingGuard.keyCount() };
      return;
    }
    // A row or a code block: the row cursor goes there and the panel is back in BROWSE. The row's
    // index is read now, from the element, never taken from `target.rowIndex`: rows can be added
    // above it while the labels are up (a permission card is anchored right after its tool call),
    // and the index frozen at `hint_collect` would then name a different row.
    const row = target.kind === "row" ? target.el : root === null ? null : rowOf(root, target.el);
    const rowIndex = root === null || row === null ? null : rowIndexOf(root, row);
    if (rowIndex === null) return;
    setMode("browse");
    if (target.kind === "code") {
      markLandedCode(target.el);
      // Reveal the BLOCK, not its row. `revealRow`'s `nearest` on a row taller than the view whose
      // top is out and bottom in aligns the row's bottom with the list's (CSSOM View's "determine the
      // scroll-into-view position"), which can carry the block just landed on off the top.
      if (rowIndex !== cursorRef.current) landingRef.current = "keep";
      target.el.scrollIntoView({ block: "nearest" });
    }
    setCursor(rowIndex);
    root?.focus({ preventScroll: true });
  }

  /** Posts `ready` and tracks it as in-flight. Rust replies with `hello` (and a snapshot, if a
   *  session exists). Called on mount and again whenever the start screen comes back. */
  function requestHello() {
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    postToRust({ type: "ready", request_id: requestId });
  }

  /* Reports how long this WebView took to draw a turn's first assistant text. The effect runs after
     React has committed the DOM; the animation frame runs just before the browser paints it. That is
     a frame, not a photon -- read it as a floor on what the user perceives, never as a measured
     perceptual latency. Deliberately NOT cancelled on cleanup: with a delta arriving every ~33ms, a
     cleanup that cancelled the pending frame would re-arm faster than the frame could ever fire, and
     the mark would simply never be reported. */
  useEffect(() => {
    const receivedAt = firstTextReceivedAt.current;
    if (receivedAt === null || renderReportSent.current) return;
    renderReportSent.current = true;
    requestAnimationFrame(() => {
      post({ type: "turn_rendered", receive_to_frame_ms: performance.now() - receivedAt });
    });
  }, [state.transcript]);

  /** Adds `request_id` and the active tab to a tab command, and posts nothing when there is no
   *  active tab yet -- there would be nothing to name it with (an inbound command without
   *  a tab is a protocol error, and this side must not manufacture the outbound mirror of that).
   *
   *  `edit_draft` is recorded as an `"editor"` in-flight request so its own refusal
   *  flashes in the footer rather than filling `commandNotice`'s banner; so is `toggle_following`. */
  function post<M extends { type: string }>(message: M) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    if (message.type === "edit_draft") {
      editReturnRef.current = { tab, requestId, started: false, finished: false };
      trackRequest(requestId, { kind: "editor", tab });
    }
    if (message.type === "toggle_following") trackRequest(requestId, { kind: "follow", tab });
    postToRust({ ...message, request_id: requestId, tab } as unknown as OutboundMessage);
  }

  function handoffToTerminal() {
    const tab = activeTabRef.current;
    if (tab === null) return;
    // Reached by clicks (the detail popover's row, then the confirm's button) that never pass through
    // `answerConfirm`, and the tab it acts on stops being the tab a bypass prompt was drawn for: that
    // prompt goes (Rust drops its own in the `HandoffToTerminal` arm).
    cancelBypassConfirm();
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    trackRequest(requestId, { kind: "handoff", tab });
    // Set BEFORE the post, so the composer is disabled from this moment rather than from whenever a
    // reply comes back. Rust takes the session out of its own state inside the handler this message
    // reaches, and every send after that point would be refused.
    setHandoffRequests((current) => new Map(current).set(tab, requestId));
    setCommandNotice(null);
    postToRust({ type: "handoff_to_terminal", request_id: requestId, tab });
  }

  /** Sends what is typed. On a `NotStarted` tab this is what starts the backend, lazily, with that
   *  tab's remembered mode -- there is no separate `start_session` message any more.
   *
   *  `fromPicker`: `chooseSlashOption` passes `true`
   *  here rather than restoring the composer's own send path -- see `inFlight`'s own doc comment on
   *  `"picker-send"` for why the two need different refusal handling even though both post the same
   *  `send_message`. */
  function sendMessage(text: string, options?: { fromPicker?: boolean }) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    // A bare command is a question about the session, which it can answer with no turn, so nothing is queued.
    const bare = options?.fromPicker ? null : barePickerCommand(text);
    if (bare !== null && picksDirectly()) {
      askForPicker(bare);
      return;
    }
    if (!options?.fromPicker && isAcpAgent(activeTabInfo(tabsRef.current)?.agent) && settingRunsNow(text)) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    // The text is kept so a refusal can put it back (ordinary sends only -- `"picker-send"`'s own
    // refusal never reads `record.text` back into the box). Dropped again as soon as the reply
    // arrives.
    trackRequest(requestId, { kind: options?.fromPicker ? "picker-send" : "send", tab, text });
    setCommandNotice(null);
    // Arms `pendingSlashPickerRef` for the `events` handler above. Set
    // optimistically, before the post is even answered; the `command_result` handler below clears
    // it again on a refusal, since a send that never ran never starts the turn this is waiting for.
    // A picker choice always carries an argument ("/model haiku"), so `barePickerCommand` already
    // returns `null` for it -- this line is harmless either way, kept as one code path rather than
    // two.
    // Another agent's tab gets a leading `/` as typed, and no picker: `/model` and `/effort` are Claude's.
    pendingSlashPickerRef.current = isExternalAgent(activeTabInfo(tabsRef.current)?.agent) ? null : barePickerCommand(text);
    postToRust({ type: "send_message", request_id: requestId, tab, text });
    // Rust clears the tab's stored draft at any send. This one never touched the box, so what the box holds is
    // mirrored again, or a tab switch would come back to an empty box.
    if (options?.fromPicker && draftRef.current !== "") mirrorDraft(draftRef.current);
    // A send follows the reply, wherever the reader had scrolled (`./follow.ts`).
    resumeFollowing(containerRef.current?.querySelector(".message-list"));
  }

  /** Whether this tab's session changes its model and effort directly, with no turn. */
  function picksDirectly(): boolean {
    const agent = activeTabInfo(tabsRef.current)?.agent;
    // An ACP agent lists its own choices and the answer to the list says when it has none, so its tab always asks.
    return isAcpAgent(agent) || (stateRef.current.capabilities.directSettings === true && !isExternalAgent(agent));
  }

  /** Whether `tab` runs an ACP agent: its choices are the agent's own, in the agent's words. */
  function tabListsItsOwn(tab: TabId): boolean {
    return isAcpAgent(tabsRef.current?.tabs.find((t) => t.id === tab)?.agent);
  }

  /** Asks for what the picker lists. It opens on the answer, also while a turn runs, and no turn is started. */
  function askForPicker(kind: SlashPickerKind) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    settingsRequests.current.set(requestId, { what: "list", tab, kind });
    postToRust({ type: "list_models", request_id: requestId, tab });
  }

  /** The answer to a request of the direct route. One for a tab no longer shown opens and says nothing. */
  function answerSettings(request: SettingsRequest, payload: SettingsAnswer) {
    if (request.what === "typed-effort") {
      answerTypedEffort(request, payload);
      return;
    }
    if (request.tab !== activeTabRef.current) return;
    if (payload.kind === "command_result") {
      if (!payload.ok) showFlash(revealedText(payload.error));
      return;
    }
    if (payload.kind === "settings_result") {
      if (request.what === "change") showFlash(revealedText(settingsChangedWords(request.kind, payload.applied, request.chosen, request.running, tabListsItsOwn(request.tab))));
      return;
    }
    const own = tabListsItsOwn(request.tab);
    const picker = request.kind === "model"
      ? modelPicker(payload.models, payload.applied, own)
      : effortPicker(payload.models, payload.applied, own ? (payload.efforts ?? []) : null);
    if (picker === null) {
      const who = agentWord(activeTabInfo(tabsRef.current)?.agent) ?? "the agent";
      showFlash(own
        ? revealedText(`${who} reports no ${request.kind === "model" ? "models" : "effort levels"} to choose from`)
        : request.kind === "model" ? "the session lists no models" : "this model takes no effort level");
      return;
    }
    setSlashReply({ kind: request.kind, ...picker, direct: true });
  }

  /** The list a typed `/effort <level>` asked for. A level the session offers changes now. Any other, and a list
   *  that did not answer, leave it what any command typed during a turn is: queued, and the CLI answers at the
   *  turn's end, since the session takes an unknown level with no word and runs on. */
  function answerTypedEffort(request: Extract<SettingsRequest, { what: "typed-effort" }>, payload: SettingsAnswer) {
    const own = tabListsItsOwn(request.tab);
    const picker = payload.kind === "models_result" ? effortPicker(payload.models, payload.applied, own ? (payload.efforts ?? []) : null) : null;
    if (picker?.options.includes(request.level)) {
      changeSetting(request.tab, "effort", request.level, picker.labels?.[request.level] ?? request.level);
    } else if (own) {
      // The agent takes the words it listed and nothing else: say so rather than send the text to it as a prompt.
      const who = agentWord(activeTabInfo(tabsRef.current)?.agent) ?? "the agent";
      showFlash(revealedText(picker === null ? `${who} reports no effort levels to choose from` : `${who} offers ${picker.options.join(", ")}`));
    } else {
      queueInto(request.tab, request.text);
    }
  }

  /** Changes the session's model or effort directly; `settings_result` says in the band when it took effect. */
  function changeSetting(tab: TabId, kind: SlashPickerKind, value: string, chosen: string) {
    const requestId = nextRequestId();
    const running = stateRef.current.activeTurnId !== null ? { model: stateRef.current.model } : null;
    settingsRequests.current.set(requestId, { what: "change", tab, kind, chosen, running });
    postToRust(kind === "model"
      ? { type: "set_model", request_id: requestId, tab, model: value }
      : { type: "apply_effort", request_id: requestId, tab, effort: value === EFFORT_FROM_SETTINGS && !tabListsItsOwn(tab) ? "" : value });
  }

  /** A typed `/model <name>` or `/effort <level>` during a turn runs at once and is never held, as Claude Code runs
   *  them: the direct route takes it, and the turn goes on. Whether it did; with no direct route, or between turns, it
   *  is what any message is. */
  function settingRunsNow(text: string): boolean {
    const setting = settingCommand(text);
    const tab = activeTabRef.current;
    // An ACP agent has no command of ours to take these between turns, so they never go to it as a prompt.
    const between = isAcpAgent(activeTabInfo(tabsRef.current)?.agent);
    if (setting === null || tab === null || !picksDirectly() || (stateRef.current.activeTurnId === null && !between)) return false;
    postToRust({ type: "history_push", request_id: nextRequestId(), text });
    if (setting.kind === "model") {
      changeSetting(tab, "model", setting.value, setting.value);
    } else {
      const requestId = nextRequestId();
      settingsRequests.current.set(requestId, { what: "typed-effort", tab, level: setting.value, text });
      postToRust({ type: "list_models", request_id: requestId, tab });
    }
    return true;
  }

  /** A click on the band's model or effort, and their keys. With the direct route the picker opens from the session's
   *  own list, also during a turn. Without it: the bare command, whose reply opens its picker as typing it does, and
   *  on a Claude tab names the effort too. Sent as the picker's own send, so a refusal leaves the box alone. A running
   *  turn says so instead: a bare command waiting for the turn's end would open a picker long after the click. */
  function askBandPicker(command: "/model" | "/effort") {
    // The click left the keys on the band's button, which is no place for them: they go back where they were.
    setKeysRequest((n) => n + 1);
    if (picksDirectly()) {
      askForPicker(command === "/model" ? "model" : "effort");
      return;
    }
    if (stateRef.current.activeTurnId !== null) {
      showFlash(`a turn is running: choose the ${command === "/model" ? "model" : "effort"} when it ends`);
      return;
    }
    sendMessage(command, { fromPicker: true });
  }

  /** The picker's own Enter (`./components/SlashPicker`): sends the choice as an ordinary turn
   *  (never held back -- `barePickerCommand` only ever names a BARE command, and this always
   *  carries an argument) and closes the picker immediately, the same way choosing a chooser row
   *  closes the chooser. */
  function chooseSlashOption(value: string) {
    if (slashPicker === null) return;
    const command = slashPicker.kind === "model" ? "/model" : "/effort";
    const { kind, direct, labels } = slashPicker;
    setSlashPicker(null);
    const tab = activeTabRef.current;
    if (direct && tab !== null) {
      changeSetting(tab, kind, value, labels?.[value] ?? value);
    } else {
      sendMessage(`${command} ${value}`, { fromPicker: true });
    }
    // The picker held the keys; unmounted, it leaves them on
    // `<body>`, where no key reaches anything. `takeKeys` runs after the render that closed it.
    setKeysRequest((n) => n + 1);
  }

  /** The picker's own Escape/q: closes it, sends nothing, and hands the keys back (as
   *  `chooseSlashOption` does). */
  function cancelSlashPicker() {
    setSlashPicker(null);
    setKeysRequest((n) => n + 1);
  }

  /** An overlay whose render throws is closed through its own leave path (`close`,
   *  which also hands the keys back where that path does) and named in the band -- one throw used to
   *  unmount the whole panel. Called from `PanelErrorBoundary`'s `onError` at each overlay's site below. */
  function overlayFailed(what: string, close: () => void) {
    close();
    showFlash(`the ${what} failed and closed`);
  }

  /** Queues what is typed behind the running turn. Recorded as a `"send"` for the same reason
   *  `sendMessage` is: the composer cleared the box on Enter, and a refusal here (the session ended,
   *  or a handoff is pending, while this side still showed a turn running) means nothing was queued
   *  and nothing reached history -- Rust remembers a queued text only once it is queued -- so the
   *  box is the only place left to put it.
   *
   *  `send_now` is deliberately NOT recorded this way: its refusal can come after its text was
   *  already queued (an interrupt the backend refused), where putting it back would show it
   *  twice. Rust saves a `send_now`'s text to history before either outcome, so `↑` recovers it. */
  function queueMessage(text: string) {
    // Enter during a turn lands here, and a bare command needs no turn at all: this is where it would queue.
    const bare = barePickerCommand(text);
    if (bare !== null && picksDirectly()) {
      askForPicker(bare);
      return;
    }
    if (settingRunsNow(text)) return;
    const tab = activeTabRef.current;
    if (tab === null) return;
    queueInto(tab, text);
  }

  function queueInto(tab: TabId, text: string) {
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "queue_message", request_id: requestId, tab, text, turn_id: stateRef.current.activeTurnId });
  }

  function interrupt() {
    post({ type: "interrupt" });
  }

  /** The names for a word that starts with `@`, in the conversation's box and the empty tab's. */
  function askAtPaths(query: string, seq: number) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    atLastAsked.current.set(tab, seq);
    postToRust({ type: "path_complete", request_id: nextRequestId(), tab, query, seq });
  }

  /** Enter on a `!` line: a refusal puts it back in the box, as a send's does. */
  function runShell(text: string) {
    const tab = activeTabRef.current;
    if (tab === null) return;
    const requestId = nextRequestId();
    setPendingCommands((prev) => new Set(prev).add(requestId));
    trackRequest(requestId, { kind: "send", tab, text });
    setCommandNotice(null);
    postToRust({ type: "run_shell", request_id: requestId, tab, text });
    resumeFollowing(containerRef.current?.querySelector(".message-list"));
  }

  /** `r` on an ended tab (restart it, as Claude Code's resume would). `TabSet::reset` (Rust) folds
   *  `tab.queue` and Rust's OWN `tab.draft` together as the tab's fresh draft -- reading whatever
   *  Rust currently has on file, not whatever this box shows. A refusal's recovered text is mirrored
   *  through the same 300ms debounce ordinary typing uses (`mirrorDraft`), so pressing `r` inside
   *  that window used to send `reset_tab` while Rust's copy was still stale: the `draft` echo that
   *  came back both wiped the composer and nulled `pendingDraftRef`, silently dropping the mirror
   *  that debounce would otherwise have sent. `flushDraft()` here delivers whatever is still pending
   *  FIRST, the same way a real tab switch already does (the `tabs` handler) -- a no-op when
   *  nothing is pending. */
  function resetTab() {
    const tab = activeTabRef.current;
    if (tab === null) return;
    flushDraft();
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "reset", tab });
    postToRust({ type: "reset_tab", request_id: requestId, tab });
  }

  /** Every answer to a card goes through here -- its own buttons, Enter in its reason box, and
   *  `a`/`d` -- so one card is answered once, whichever of them gets there first. */
  function answerPermission(
    permissionId: string,
    decision: PermissionDecision,
    reason?: string,
    remember?: boolean,
    origin: "key" | "click" = "click",
  ) {
    const tab = activeTabRef.current;
    if (answeredRef.current.has(permissionId) || tab === null) return;
    answeredRef.current = new Set(answeredRef.current).add(permissionId);
    setAnsweredPermissions(answeredRef.current);
    // Recorded, so a refusal gives the card back (`inFlight`'s doc).
    const requestId = nextRequestId();
    trackRequest(requestId, { kind: "permission", tab, permissionId });
    // A card Eitri's own tool broker raised is answered on its own message, by its own id, and carries how the
    // person answered: no reason and no rule, which a broker card does not offer. Anything else is a provider's.
    if (stateRef.current.editorCards.some((card) => card.permissionId === permissionId)) {
      postToRust({ type: "editor_card_answer", request_id: requestId, tab, card_id: permissionId, decision, origin });
      return;
    }
    postToRust({
      type: "permission_response",
      request_id: requestId,
      tab,
      permission_id: permissionId,
      decision,
      reason,
      ...(remember ? { remember: true } : {}),
    });
  }

  /** Commits (or cancels) an inline rename. Both return the keys to the root, the same
   *  as every other overlay closing does. Sends `renaming.tab`, not the active tab: the field can
   *  stay open across a switch (`n`/`p`/digits/`l` move focus-free), so `post`'s
   *  `activeTabRef.current` would name the wrong tab -- the same reason `answerConfirm` sends a
   *  `"close"` confirm's own `confirm.tab` directly instead of going through `post` (a
   *  `"close_others"` confirm has no tab at all, and posts window-level like `TabVerb`). */
  function commitRename(name: string) {
    if (renaming !== null) {
      postToRust({ type: "rename_tab", request_id: nextRequestId(), tab: renaming.tab, name });
    }
    setRenaming(null);
    containerRef.current?.focus();
  }
  function cancelRename() {
    setRenaming(null);
    containerRef.current?.focus();
  }

  /** Opens the `:` line empty and hands it the keys; one already open is wiped, as vim's `:` gives a fresh line. */
  function openExLine() {
    setSearch(null);
    setExLine("");
    setLineFocusRequest((n) => n + 1);
  }

  /** Hands the keys back to whichever layout is on screen -- the conversation's own root, or the
   *  start screen's while no session has started yet. The same fallback `hint_collect` and the
   *  document-`<body>` replay above already use. */
  function returnKeysToRoot() {
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    // The start screen owns its BROWSE root. Its arrival edge focuses that root and leaves
    // INPUT while keeping the draft, just as returning to the conversation's root does.
    setEmptyLanding("browse");
    setArriveRequest((n) => n + 1);
  }

  /** The `/` and `:` lines stop their own Enter and Esc (`SearchBar`), so `onKeyDown` never
   *  hands those two keys to the typing guard -- and `:ls`, a pause, Enter, then `a` at once
   *  answered the cursor's card as a key standing alone. Each line's callback hands its key over
   *  here first, on `onKeyDown`'s own clock (`event.timeStamp` is what a test's faked `Date` moves). */
  function noteLineKey(event: KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) {
    // A key, and not a kill -- so a `Ctrl+y` after it is judged on its own.
    lastKeyWasKillRef.current = false;
    const cancelled = typingGuard.onKey(event.key, typingGuard.timeOf(event));
    if (cancelled !== null) showFlash(cancelled);
  }

  /** `prefix w` (open tabs first, then records to resume, as tmux's `choose-tree` lists open windows
   *  first). Every callback closes the overlay and returns the keys to the panel root -- the launch
   *  chooser is gone, so there is no `onLeave(true)` case that hands the keys to the editor
   *  instead any more. */
  function onChooserSwitch(tab: TabId) {
    postToRust({ type: "select_tab", request_id: nextRequestId(), tab });
    setChooser(null);
    returnKeysToRoot();
  }
  function onChooserResume(providerSessionId: string) {
    post({ type: "resume", provider_session_id: providerSessionId });
    setChooser(null);
    returnKeysToRoot();
  }
  /** The chooser's `Restore last session` row: the dashboard's `s`, reached from the editor. */
  function onChooserRestoreLast() {
    post({ type: "restore_last" });
    setChooser(null);
    returnKeysToRoot();
  }
  function onChooserCloseTab(tab: TabId) {
    // Rust's own close prompt is `prefix &`; this asks the same question tmux's `choose-tree` `x`
    // does, from the row's own label -- there is no round trip to Rust for this text.
    const label = chooser?.open.find((t) => t.tab === tab)?.label ?? `tab ${tab}`;
    setChooser(null);
    setConfirm({ kind: "close", tab, lines: [`close ${label}? (y/n)`] });
    returnKeysToRoot();
  }
  function onChooserLeave() {
    setChooser(null);
    // Closing the chooser returns to the visible layout's BROWSE root, with the start screen
    // using the same arrival edge as other keyboard arrivals.
    if (containerRef.current !== null) {
      containerRef.current.focus({ preventScroll: true });
      return;
    }
    setEmptyLanding("browse");
    setArriveRequest((n) => n + 1);
  }
  /** The chooser's own `New session` row: the same
   *  window-level `tab_verb new` the `tab.new` action already posts (`handleAction` above),
   *  reached from a second place. */
  function onChooserNewSession() {
    postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "new" });
    setChooser(null);
    returnKeysToRoot();
  }
  /** `Ctrl+r` on an open tab's own row (the tab bar's rename field, moved into the row): the chooser keeps its own
   *  local rename state (`Chooser.tsx`) rather than routing through `begin_rename`/`renaming` -- that pair is the tab
   *  bar's own overlay, which is not on screen while the chooser is. */
  function onChooserRenameTab(tab: TabId, name: string) {
    postToRust({ type: "rename_tab", request_id: nextRequestId(), tab, name });
  }
  /** `Shift+Tab` over `New session` or a record: the active tab's own mode
   *  when it has not started yet -- a resume would land in it -- else the window's remembered
   *  default, since a resume then opens a new tab. Either way Rust re-sends `tabs` with the new
   *  value, so the chooser's mode line follows without being re-sent itself. */
  function onChooserCycleMode() {
    const info = activeTabInfo(tabs);
    if (info !== null && info.state === "not_started") post({ type: "cycle_mode" });
    else postToRust({ type: "cycle_default_mode", request_id: nextRequestId() });
  }
  /** `Shift+Tab` on an open tab's own row: that tab's own toggle, named by the
   *  row -- `Chooser` offers it only where Rust acts on it (leaving bypass on any tab, entering it on
   *  the active one). The chooser stays open; a bypass prompt this raises is answered inside it. */
  function onChooserCycleTabMode(tab: TabId) {
    postToRust({ type: "cycle_mode", request_id: nextRequestId(), tab });
  }

  /** Every route away cancels the prompt, as tmux's `confirm-before` does: called from the `pane_focus` (losing
   *  focus), `tabs` (a DIFFERENT active tab), `arrive`, `focus_permission` and `enter_input`
   *  branches below -- and from every other route GTK claims before the
   *  WebView sees a key (`hint_collect`, `begin_rename`, `tab_detail`, `open_keymap`, `nav_key`,
   *  `literal_key`) and from a terminal handoff. Rust drops its own outstanding prompt on the same
   *  routes, so a `y` that slipped through is refused by the nonce. Ends a restore question too (it
   *  is another way into bypass, and Rust drops its own the same way); leaves a `close`/`close_others`
   *  confirm alone. **Only three callers are left** -- losing the pane's focus, a switch to
   *  another active tab and the terminal handoff, routes that take the keys nowhere inside the panel: every route
   *  that hands the keys to something else calls `endKeyPrompts` (below) instead, which ends all three kinds of
   *  prompt, because a `y` typed into what such a route opens answered a close prompt too. */
  function cancelBypassConfirm() {
    setConfirm((c) => (c?.kind === "bypass" || c?.kind === "restore" || c?.kind === "trust" || c?.kind === "stop_background" || c?.kind === "stop_turn" ? null : c));
    setHeldTrust(null);
  }

  /** `<leader>k`, and a stop arrival with no turn running: asks to stop the active tab's background work, or says
   *  why it cannot. Claude stops its roster explicitly; other providers keep the session-ending stop. */
  function askStopBackgroundOfActiveTab() {
    const tab = activeTab?.id ?? null;
    if (!isExternalAgent(activeTab?.agent)) {
      if (!state.capabilities.backgroundTasks) showFlash(CLAUDE_REPORTS_NO_BACKGROUND);
      else if (!state.capabilities.taskControl) showFlash("this sidecar cannot stop one task");
      else if (tab === null || activeTab?.state !== "live" || liveTasks(state.backgroundTasks.rows).length === 0) showFlash(NOTHING_IN_THE_BACKGROUND);
      else if (state.activeTurnId !== null) showFlash("a turn is running: /tasks can stop background work");
      else askStopBackground(tab, liveTasks(state.backgroundTasks.rows).length);
      return;
    }
    if (tab === null || activeTab?.state !== "live" || state.backgroundCalls === 0) {
      showFlash(nothingToStopWords(activeTab?.agent));
      return;
    }
    if (state.activeTurnId !== null) {
      showFlash("a turn is running: interrupting the turn stops them too");
      return;
    }
    askStopBackground(tab, state.backgroundCalls);
  }

  /** A stop arrival's question while `tab`'s `work` runs (a turn, or a stretch of Claude Code's work the chat cannot
   *  show). Like the other y/n prompts it takes the keys from whatever was open over the conversation; only a counted
   *  `y` sends the chat's own `interrupt` for `tab`, and only while `work` is still the work running. */
  function askStopTurn(tab: TabId, work: StopWork) {
    dropPendingKeys();
    exitRegion();
    setDetail(null);
    setHandoffOpen(false);
    setKeymapOpen(false);
    setConfirm({ kind: "stop_turn", tab, work, lines: [STOP_TURN_QUESTION], openedAt: performance.now() });
  }

  /** Opens `<leader>k`'s question for `tab`, naming `running` commands and the tab's agent. Like the other y/n
   *  prompts it takes the keys from whatever was open over the conversation. */
  function askStopBackground(tab: TabId, running: number) {
    dropPendingKeys();
    exitRegion();
    setDetail(null);
    setHandoffOpen(false);
    setKeymapOpen(false);
    const agent = tabsRef.current?.tabs.find((t) => t.id === tab)?.agent;
    const claude = !isExternalAgent(agent);
    const tasks = liveTasks(stateRef.current.backgroundTasks.rows);
    stopTasksSignature.current = JSON.stringify(tasks.map((t) => [t.task_id, t.run_id, t.kind]).sort());
    const word = agentWord(agent);
    const who = word === null ? "the agent" : word === "codex" ? "Codex" : word;
    setConfirm({ kind: "stop_background", tab, running, lines: [claude ? `Stop ${stopTaskWords(tasks)}? y/n` : stopBackgroundQuestion(who, running)], openedAt: performance.now() });
  }

  /** What a route that hands the keys to something else ends -- every
   *  pending y/n prompt (`close`, `close_others` and `bypass`, which `cancelBypassConfirm` leaves two of) and a
   *  `gf` letter picker. Both answer the next key they are given, ahead of whatever that key was meant for:
   *  `answerConfirm` runs in the root's capture phase and claims every key while any `confirm` is set, and the
   *  picker's branch of `onKeyDown` runs ahead of the rest of it. So a `y` typed into a rename field, the chooser,
   *  the composer or a card's reason box, which the route has just put the keys in, closed a tab; a letter opened a
   *  file. Called by every such route (`open_command_line`, `begin_rename`, `chooser`, `tab_detail`, `open_keymap`,
   *  `hint_collect`, `enter_input`, `arrive`, `focus_permission`, and the two keys GTK claims, `nav_key` and
   *  `literal_key` -- for which "any key but y cancels" is the prompt's own rule). NOT by `pane_focus` or a switch to
   *  another tab, which take the keys nowhere: a close prompt names its tab by id and stays drawn in the band, so
   *  the `y` that answers it is still a `y` to it (those two keep `cancelBypassConfirm`). Only state setters, so a
   *  listener installed once, with the first render's closure, may call it. */
  function endKeyPrompts() {
    setConfirm(null);
    setPathPick(null);
    setHeldTrust(null);
  }

  /** The window-close prompt owns every key ahead of everything else, in BOTH layouts:
   *  the conversation's own `onKeyDown` below, and `EmptyTab`'s root while the chat is on screen
   *  with no session yet (`prefix &` first shows the chat and gives it the keys). Lifted out of either
   *  handler so both call it first and agree. Returns whether it claimed the key -- only
   *  `confirm === null` does not.
   *
   *  Kind `"bypass"`: EVERY non-modifier key cancels the whole switch except
   *  a counted `y`/`Y` ("n/any key but a counted y/Y cancels"), and even a `y`/`Y` that
   *  arrives too soon after the prompt appeared or after another key -- typed anywhere in the panel,
   *  `lastKeyAtRef` sees all of it -- cancels too, with its own flash rather than the generic one,
   *  since the reader must know EXACTLY why it did not count. Either way the prompt closes:
   *  there is no world where a lone `y` fails the guard and the prompt is left open for a retry with
   *  no new envelope, since that would let a SECOND stray key close the gap the guard just refused.
   *
   *  Three more things, each a way a key did more than
   *  answer the prompt.
   *  - **Called in the CAPTURE phase at each layout's root** (`onKeyDownCapture` below), and it stops
   *    propagation of every key it claims. Called only from bubble handlers, a key from inside the
   *    chooser was answered twice (the chooser's own call, then the conversation root's with the same
   *    stale `confirm`: a second `confirm_bypass` refused as "no longer current" and drawn as an
   *    error banner), and an input's own Enter/Escape (the chooser's filter and rename, the tab bar's
   *    rename, the `/` prompt, the composer's send) ran first and left the prompt open -- but
   *    any key but a counted `y`, "Enter included", cancels the prompt and does nothing else.
   *  - **The typing guard sees the key**: it is fed here, since a claimed key never reaches
   *    `onKeyDown`'s own `typingGuard.onKey`. Without it the key that cancelled the prompt was
   *    invisible to the guard, and `x` then `a` 100 ms later approved the card the bypass prompt was drawn
   *    over.
   *  - **A held key's repeat never counts** (the guard's own rule for cards): a counted `y` can be answered
   *    with a reprompt within milliseconds, and the same held key's first autorepeat would then
   *    confirm a prompt listing a card the user never saw.
   *
   *  Found in a sandbox GUI pass (2026-09-27): a HELD key whose first press cancelled a prompt (the
   *  paragraph just above -- e.g. a `y` too soon to count) leaves `confirm` null, so every one of
   *  its physical auto-repeats after that used to hit the `confirm === null` return below and fall
   *  straight through as an ordinary key -- BROWSE's own `y` (copy the row under the cursor), which
   *  calls `copied()` and overwrites the "y must be pressed on its own..." flash above with
   *  "copied N chars" within about one repeat interval (~40ms). Fixed by swallowing that SAME key's
   *  repeats, specifically, until its keyup (`promptSwallowKeyRef`, set below, cleared by the effect
   *  that owns it) -- checked here, ahead of the `confirm === null` return, so it still applies once
   *  `confirm` is gone. A confirm that is NOT null (a fresh reprompt arrived in between) skips
   *  this and reaches the branch below unchanged, which already treats a repeat as never counting. */
  function answerConfirm(event: KeyboardEvent<HTMLElement>): boolean {
    // The keys have moved into a text field (a click on the composer, say) while the restore question
    // was up: what is typed there is text, never an answer -- the `y` of "you should" must not give a
    // tab bypass. The question ends and the key is left for the field.
    if (confirm?.kind === "restore" && isEditableElement(event.target)) {
      setConfirm(null);
      return false;
    }
    if (confirm === null) {
      if (promptSwallowKeyRef.current !== null && event.key === promptSwallowKeyRef.current && event.repeat) {
        event.preventDefault();
        event.stopPropagation();
        return true;
      }
      return false;
    }
    event.stopPropagation();
    // `isModifierKey`, not a four-name list: WebKitGTK names the Super key `"Super"`, not `"Meta"`, so
    // holding it used to cancel the prompt by itself, silently (sandbox pass, 2026-09-28).
    if (!isModifierKey(event.key)) {
      event.preventDefault();
      promptSwallowKeyRef.current = event.key;
      typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typingGuard.timeOf(event));
      if (confirm.kind === "bypass") {
        if (event.key === "y" || event.key === "Y") {
          // A y with Ctrl/Alt/Meta held, or one an input
          // method is composing, is an editing keystroke, never the lone y this prompt asks for -- the timing guard
          // alone let `Ctrl+y` or a pinyin syllable's first letter enter bypass. Shift stays (`Y`).
          // Reconciled with keymap.ts's card-answer rule onto one
          // shared predicate, `isPlainAnswerKey` -- this file's own original hand-written check
          // (superseded, not visible above any more) never tested Super/Hyper, the same gap the card
          // answers `a`/`d`/`D` had closed; a further gap neither side had, AltGraph, was closed the
          // same day. `event.nativeEvent` is what carries a real `getModifierState`, the same cast
          // `resolveKey`'s own call site uses.
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          if (
            plain &&
            !event.repeat &&
            bypassYesCounts(answerClock())
          ) {
            postToRust({
              type: "confirm_bypass",
              request_id: nextRequestId(),
              tab: confirm.tab,
              scope: confirm.scope,
              nonce: confirm.nonce,
            });
          } else {
            showFlash("y must be pressed on its own to enter bypass — Shift+Tab to ask again");
          }
        }
        setConfirm(null);
        return true;
      }
      if (confirm.kind === "trust") {
        // Escape alone works from inside a text field: it starts nothing. Every other key typed there is the
        // field's text, so it is taken and never answers.
        if (event.key !== "Escape" && isEditableElement(event.target)) {
          showFlash(trustFooter(confirm.view.remember, confirm.view.needsYes === true, confirm.view.state), { toPrompt: true });
          return true;
        }
        const pendingG = trustPendingGRef.current;
        trustPendingGRef.current = false;
        const action = resolveTrustKey(event.nativeEvent as unknown as TrustKeyEvent, {
          ...answerClock(),
          pendingG,
          onlyN: confirm.view.state === "too_large",
        });
        if (action.kind === "answer") {
          // Echoes what was drawn: the page never computes either value, so an answer to anything other than
          // the shown findings is refused by Rust.
          const answerId = nextRequestId();
          trackRequest(answerId, { kind: "trust", tab: confirm.tab });
          postToRust({
            type: "trust_answer",
            request_id: answerId,
            tab: confirm.tab,
            nonce: confirm.nonce,
            fingerprint: confirm.view.fingerprint,
            findings_digest: confirm.view.findingsDigest,
            trust: action.trust,
          });
          setConfirm(null);
        } else if (action.kind === "cancel") {
          const cancelId = nextRequestId();
          trackRequest(cancelId, { kind: "trust", tab: confirm.tab });
          postToRust({ type: "trust_cancel", request_id: cancelId, tab: confirm.tab, nonce: confirm.nonce });
          setConfirm(null);
        } else if (action.kind === "scroll") {
          const box = trustOverlayRef.current;
          if (box !== null) box.scrollTop = scrollTarget(action.by, box, rowScrollStep(box));
        } else if (action.kind === "pending_g") {
          trustPendingGRef.current = true;
        } else if (action.kind === "wait") {
          showFlash(TRUST_WAIT_FLASH, { toPrompt: true });
        } else {
          showFlash(trustFooter(confirm.view.remember, confirm.view.needsYes === true, confirm.view.state), { toPrompt: true });
        }
        return true;
      }
      if (confirm.kind === "restore") {
        // `n` is always safe: it brings the bypass tabs back in auto. `y` gives them back in bypass, so
        // it passes every test the bypass prompt's own `y` does: a plain key, not a repeat, and a
        // moment after the prompt appeared with nothing typed around it. Any other key cancels.
        const yes = event.key === "y" || event.key === "Y";
        const no = event.key === "n" || event.key === "N";
        if (yes || no) {
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          const counts =
            no ||
            (plain &&
              !event.repeat &&
              bypassYesCounts(answerClock()));
          if (counts) {
            postToRust({
              type: "restore_answer",
              request_id: nextRequestId(),
              nonce: confirm.nonce,
              keep_bypass: yes,
            });
          } else {
            showFlash("y must be pressed on its own to restore in bypass — n restores in auto");
          }
        }
        setConfirm(null);
        return true;
      }
      if (confirm.kind === "stop_background") {
        const claude = !isExternalAgent(activeTab?.agent);
        const tasks = liveTasks(state.backgroundTasks.rows);
        const running = claude ? tasks.length : state.backgroundCalls;
        const signature = JSON.stringify(tasks.map((t) => [t.task_id, t.run_id, t.kind]).sort());
        // Any key but a counted `y` puts the question away and stops nothing (`n`, Escape included).
        if (event.key === "y" || event.key === "Y") {
          const plain = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike);
          if (
            !plain ||
            event.repeat ||
            !bypassYesCounts(answerClock())
          ) {
            showFlash("y must be pressed on its own to stop them; nothing was stopped");
          } else if (activeTab?.id !== confirm.tab || activeTab?.state !== "live" || running === 0) {
            // Not the tab, or not the commands, the question was about any more.
            showFlash(running === 0 ? NOTHING_IN_THE_BACKGROUND : "the tab changed; nothing was stopped");
          } else if (claude && !state.capabilities.taskControl) {
            showFlash("this sidecar cannot stop one task");
          } else if (claude && state.activeTurnId !== null) {
            showFlash("a turn is running: /tasks can stop background work");
          } else if (running !== confirm.running || (claude && signature !== stopTasksSignature.current)) {
            // The count moved while the question was up: asked again with the count as it is now.
            askStopBackground(confirm.tab, running);
            return true;
          } else {
            const stopId = nextRequestId();
            trackRequest(stopId, { kind: "stop-background", tab: confirm.tab });
            postToRust({ type: "stop_background", request_id: stopId, tab: confirm.tab, running: confirm.running });
          }
        }
        setConfirm(null);
        return true;
      }
      if (confirm.kind === "stop_turn") {
        // Any key but a counted `y` puts the question away and stops nothing (`n`, Escape included).
        const answer = stopTurnAnswer(
          { key: event.key, plain: isPlainAnswerKey(event.nativeEvent as unknown as KeyLike), repeat: event.repeat },
          bypassYesCounts(answerClock()),
          {
            sameTab: activeTab?.id === confirm.tab && activeTab?.state === "live",
            asked: confirm.work,
            now: workToStop(state),
          },
        );
        if (answer.kind === "interrupt") {
          postToRust({ type: "interrupt", request_id: nextRequestId(), tab: confirm.tab });
        } else if (answer.kind === "refused") {
          showFlash(answer.flash);
        }
        setConfirm(null);
        return true;
      }
      // Every panel y/n accepts `y` and `Y`, like the window's own
      // `close_prompt.rs`. Any other key still cancels (tmux `confirm-before`).
      if (event.key === "y" || event.key === "Y") {
        if (confirm.kind === "close") {
          postToRust({ type: "close_tab", request_id: nextRequestId(), tab: confirm.tab });
        } else {
          postToRust({ type: "close_others", request_id: nextRequestId() });
        }
      }
      setConfirm(null); // tmux confirm-before: any other key cancels
    }
    return true;
  }

  /* Rendered on both screens. A refused command can return the panel to the start screen (a failed
     handoff close, for one), and a reason that only exists in the conversation view would be gone by
     the time it could be read. */
  const commandNoticeBanner =
    commandNotice === null ? null : (
      <div className="command-notice" role="alert" data-nav-stop="notice">
        <span>{commandNotice}</span>
        <button onClick={() => setCommandNotice(null)}>Dismiss</button>
      </div>
    );

  /* A fact about the window, not the last command: it has its own field and its own Dismiss, so neither a send
     nor a refused command's notice takes it away. */
  const rootNoteBanner =
    rootNote === null ? null : (
      <div className="command-notice root-note" role="status" data-nav-stop="root-note">
        <span>{rootNote}</span>
        <button onClick={() => setRootNote(null)}>Dismiss</button>
      </div>
    );

  const errorBanner =
    fatalError === null ? null : (
      <div className="fatal-error" role="alert" data-nav-stop="error">
        <strong>The agent session ended.</strong>
        {/* <pre>, not a <p>: the sidecar's startup diagnostics are multi-line and the exact text
            (a CLI version, a checkout revision) is the whole point. */}
        <pre>{fatalError}</pre>
        <button onClick={() => setFatalError(null)}>Dismiss</button>
      </div>
    );

  /** The band's prompt segment. A prompt takes the whole band, so a flash made while the trust question is up
   *  (a `y` that came too soon, a key it does not take) is drawn after the prompt, where it can be read. */
  const promptFlash = confirm !== null && confirm.kind === "trust" && flash !== null && flash.toPrompt === true && flash.seq > confirm.flashAfter;
  const confirmBandPrompt =
    confirm === null ? null : promptFlash ? `${confirm.lines.join(" · ")} · ${flash!.text}` : confirm.lines.join(" · ");
  /** The flash the band draws in its own one-row place: any but the trust question's answer drawn after its prompt. */
  const bandFlash = promptFlash ? null : flash;

  const trustOverlay = (
    <TrustOverlay
      confirm={confirm}
      setConfirm={setConfirm}
      overlayFailed={overlayFailed}
      trackRequest={trackRequest}
      trustOverlayRef={trustOverlayRef}
    />
  );

  /** The `:` command line, where vim draws it. It runs `:tabnew` and the trust commands and nothing else:
   *  Enter on anything else says so, Esc (and `Ctrl+[`) closes it silently. Drawn over whichever layout is on
   *  screen: a fresh window has only the start screen, and `:tabnew codex` is how it gets a Codex tab. */
  const commandLineOverlay =
    exLine === null ? null : (
      <PanelErrorBoundary
        name="command line"
        onError={() =>
          overlayFailed("command line", () => {
            setExLine(null);
            returnKeysToRoot();
          })
        }
      >
        <SearchBar
          lead=":"
          label="Command line"
          query={exLine}
          focusRequest={lineFocusRequest}
          onChange={setExLine}
          onAccept={(event) => {
            noteLineKey(event);
            const command = parseExCommand(exLine);
            if (command !== null && command.command === "tabnew") {
              // vim's `:tabnew`: the word names the agent; none is the new-tab key's own tab. Which words
              // name an agent is Rust's to say (`init.lua` may register more), and its refusal, naming
              // every accepted word, is flashed.
              const tabnewId = nextRequestId();
              if (command.agent !== null) {
                trackRequest(tabnewId, { kind: "tabnew", tab: activeTabRef.current ?? 0 });
              }
              postToRust({
                type: "tab_verb",
                request_id: tabnewId,
                verb: "new",
                ...(command.agent !== null ? { agent: command.agent } : {}),
              });
            } else if (command !== null) {
              const commandId = nextRequestId();
              trackRequest(commandId, { kind: "trust", tab: activeTabRef.current ?? 0 });
              // The trust it changes is the agent's of the tab on screen.
              const agent = activeTabInfo(tabsRef.current)?.agent ?? "claude";
              postToRust({ type: "trust_command", request_id: commandId, action: command.command, agent });
            } else {
              showFlash(`:${exLine} — no ex commands here; ? lists this panel's keys`);
            }
            setExLine(null);
            returnKeysToRoot();
          }}
          onCancel={(event) => {
            noteLineKey(event);
            setExLine(null);
            returnKeysToRoot();
          }}
        />
      </PanelErrorBoundary>
    );

  if (!sessionStarted) {
    // `errorBanner` deliberately stays out of this branch: a failed tab shows its own reason inside
    // `EmptyTab` (`activeTab.failure`), and a fatal error that arrived before any `tabs` envelope
    // (nothing to blame it on yet) still reaches the reader through `failure` below, as a fallback.
    return (
      <div
        className="agent-ui-root"
        ref={startScreenRef}
        tabIndex={-1}
        // Whether the panel holds the keys, for the stylesheet: the start screen's box is quiet without them.
        data-pane-focused={paneFocused ? "true" : "false"}
        // `.empty-tab` is centred, so a click above or below it lands here; taking focus would put the
        // keys on an element that handles none (a GUI pass, 2026-09-25). Its own background
        // only -- a press on anything inside is the browser's.
        onMouseDown={(event) => {
          if (event.target === event.currentTarget) event.preventDefault();
        }}
        // The `?` overlay owns every key while it is open, as it does over a conversation (the
        // live layout's `onKeyDown`); in capture, ahead of `EmptyTab`'s own handler and its
        // composer, which would otherwise take `m`, `j` or a typed letter under it. A y/n prompt
        // comes before even that, in capture for the reason `answerConfirm`'s own doc
        // gives: the chooser's inputs and the composer must not see a key the prompt takes.
        onKeyDownCapture={(event) => {
          if (answerConfirm(event)) return;
          if (tasksOpen) {
            onTasksKey(event);
            return;
          }

          if (!keymapOpen) return;
          onKeymapKey(event);
        }}
      >
        {tabs !== null && showTabBar(tabs.tabs.length, renaming !== null) && (
          <TabBar
            tabs={tabs.tabs}
            active={tabs.active}
            renaming={renaming}
            focusRequest={tabBarFocusRequest}
            onSelect={(tab) => postToRust({ type: "select_tab", request_id: nextRequestId(), tab })}
            onRenameCommit={commitRename}
            onRenameCancel={cancelRename}
          />
        )}
        {rootNoteBanner}
        {commandNoticeBanner}
        {/* The stage is the trust question's containing block: it spans everything above the band, so the
            band's own prompt and flashes stay readable while the question is up (an overlay over the whole
            root hid them). Normal flow is the same as without it: one column, `.empty-tab` centred in it. */}
        <div className="empty-stage">
          {activeTab === null ? (
            <div className="empty-tab">
              <p className="connecting">Connecting to the shell…</p>
            </div>
          ) : (
            <EmptyTab
              pages={pages}
              onLoadPage={(id) => postToRust({ type: "load_page", id })}
              onPageLine={(id) => postToRust({ type: "page_line", id })}
              onSlashAction={runSlashAction}
              hello={hello}
              tab={activeTab}
              handoff={handoff}
              failure={activeTab.failure ?? fatalError}
              paneFocused={paneFocused}
              entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
              // The dashboard's own first-run hint line reads this live, rather
              // than assuming the stock default -- the same value the `?` overlay already shows as
              // `prefixLabel`.
              editorLink={editorLink}
              focusRequest={inputRequest}
              arriveRequest={arriveRequest}
              // `EmptyTab` bumps `Composer`'s focus itself and lands its own root on a
              // bare `keysRequest`, so the `prefix w` chooser drawn over an empty tab 1 and a
              // `keysRequest` bump both reach it directly rather than through `App`'s own
              // `containerRef`, which does not exist on this layout.
              keysRequest={emptyKeysRequest}
              editReturnRequest={emptyEditReturnRequest}
              dropKeysRequest={emptyDropKeys}
              overlayOpen={overlayOpen}
              landing={emptyLanding}
              onModeChange={setEmptyMode}
              // Only a `navKey` the effect above found no overlay owning the keys
              // for -- this screen decides the rest itself (a starting/failed tab, its own menu vs.
              // composer), reporting back through `onNavFallthrough` when it cannot apply one either.
              navKeyRequest={emptyNavKey}
              onNavFallthrough={(direction) => postToRust({ type: "nav_fallthrough", request_id: nextRequestId(), direction })}
              restoredDraft={restoredDraft}
              // The leader engine (Space starts a leader sequence), which this screen once lacked
              // entirely. `EmptyTab`
              // keeps its own local copy of the sequence/box state (it already keeps its own `mode`
              // separately from the live conversation's), reusing only the pure table and the same
              // `runPanelAction` the live conversation's `onKeyDown` calls.
              panelTable={panelTable}
              onPanelAction={runPanelAction}
              typingGuard={typingGuard}
              onExLine={openExLine}
              onFlash={showFlash}
              onOpenUrl={openUrl}
              // Whoever sends from here is typing, and the conversation this starts must open where
              // they are: in INPUT. It mounted in BROWSE, so a second message typed straight away ran
              // as BROWSE keys (a GUI pass, 2026-09-25).
              onRunShell={runShell}
              atPaths={atPaths}
              onAtQuery={askAtPaths}
              onSend={(text) => {
                setMode("input");
                sendMessage(text);
              }}
              onResume={(id) => post({ type: "resume", provider_session_id: id })}
              onRestoreLast={() => post({ type: "restore_last" })}
              onCycleMode={() => post({ type: "cycle_mode" })}
              onReset={resetTab}
              onHint={requestHint}
              // The dashboard's `p` item: the window opens its own chooser; the page names no path.
              onChooseProject={() => postToRust({ type: "choose_project", request_id: nextRequestId() })}
              // The dashboard's `w` item: the same window-level
              // `tab_verb choose` `runPanelAction`'s `tab.choose` case posts for `prefix w`.
              onChooseSessions={() => postToRust({ type: "tab_verb", request_id: nextRequestId(), verb: "choose" })}
              onDraftChange={mirrorDraft}
              answerConfirm={answerConfirm}
              onQueue={(text) => {
                setMode("input");
                queueMessage(text);
              }}
              history={history}
              queueCount={heldCount(queue)}
              queue={queue}
              queueError={queueError}
              queueNote={queueNote}
              onTakeBackQueue={() => post({ type: "take_back_queue" })}
              queueTaken={queueTaken}
              onHistoryPush={(t) => postToRust({ type: "history_push", request_id: nextRequestId(), text: t })}
              onEditInNvim={(t) => post({ type: "edit_draft", text: t })}
              editingInNvim={scratchEditing}
              onOpenKeymap={() => {
                setMode("browse");
                setKeymapOpen(true);
              }}
            />
          )}
          {confirm?.kind === "trust" && trustOverlay}
        </div>
        {/* No `.agent-ui-scroller` on this screen: the band, and the window-close
            prompt it draws while `prefix &` is open, sit directly under the empty tab instead of
            below a composer that lives inside `EmptyTab` itself. The band replaces this screen's
            `Footer` the same way it replaces the conversation's; the editor context (once
            `EmptyTab`'s own `ContextLine`) is its `context` fact. */}
        {commandLineOverlay}
        <DeviceList desk={remoteDesk} covered={remoteCovered} />
        <RemoteEnroll desk={remoteDesk} covered={remoteCovered} />
        <StatusBand
          facts={{
            mode: emptyMode,
            pill: modePill(activeTab?.mode ?? "auto", true, true),
            showcmd: null,
            message: bandFlash?.text ?? null,
            prompt: confirmBandPrompt,
            warn: null,
            unread: null,
            cards: 0,
            queued: 0,
            context: contextFact(editorContext),
            link: editorLink,
            position: null,
            model: null,
            usage: null,
            // An empty tab has had no turns.
            ending: null,
            // Phones watch other sessions too: the mark does not wait for this tab to start.
            phones: connectedPhones.length,
            phoneDevices: connectedPhones,
            remoteSandbox,
          }}
          paneFocused={paneFocused}
          entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
        />
        {/* `prefix w`'s chooser can open over an empty tab 1 too -- `.agent-ui-root` is this layout's
            own positioned ancestor (it has no `.agent-ui-scroller` to nest inside). */}
        {tasksOverlay()}
        {chooser !== null && (
          <PanelErrorBoundary name="chooser" onError={() => overlayFailed("chooser", onChooserLeave)}>
            <Chooser
              envelope={chooser}
              tabs={tabs?.tabs ?? []}
              active={tabs?.active ?? null}
              defaultMode={tabs?.defaultMode ?? "auto"}
              projectDir={hello?.projectDir ?? ""}
              newTabChord={keymapHelp.newTabChord}
              focusRequest={chooserFocusRequest}
              dropKeysRequest={emptyDropKeys}
              restore={hello?.restore ?? null}
              onRestoreLast={onChooserRestoreLast}
              onSwitch={onChooserSwitch}
              onResume={onChooserResume}
              onNewSession={onChooserNewSession}
              onCloseTab={onChooserCloseTab}
              onRenameTab={onChooserRenameTab}
              onCycleMode={onChooserCycleMode}
              onCycleTabMode={onChooserCycleTabMode}
              onLeave={onChooserLeave}
              answerConfirm={answerConfirm}
            />
          </PanelErrorBoundary>
        )}
        {/* A bare /model or /effort reply opens this, the same
            positioning reasoning as the chooser just above -- no `.agent-ui-scroller` on this
            layout to nest inside either. */}
        {slashPicker !== null && (
          <PanelErrorBoundary name="picker" onError={() => overlayFailed("picker", cancelSlashPicker)}>
            <SlashPicker
              kind={slashPicker.kind}
              options={slashPicker.options}
              current={slashPicker.current}
              labels={slashPicker.labels}
              direct={slashPicker.direct}
              focusRequest={slashPickerFocusRequest}
              onChoose={chooseSlashOption}
              onCancel={cancelSlashPicker}
            />
          </PanelErrorBoundary>
        )}
        {/* The dashboard's `? Keys` (and `?` on an empty draft, `prefix ?`): drawn here too since the GUI
            pass (2026-09-26); `.agent-ui-root` is this layout's positioned ancestor, as for the
            chooser. */}
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              pages={pages}
              ref={keymapOverlayRef}
              onClose={closeKeymapByKey}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
              plugin={keymapPlugin ?? undefined}
              phones={connectedPhones}
              deviceList={remoteOn}
              startAt={keymapOpening.current !== null && keymapPlugin === null ? "prefix" : undefined}
              startRequest={keymapRequest}
              claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
              agent={isExternalAgent(activeTab?.agent) ? agentTitle(activeTab?.agent) : null}
              modes={hello === null ? undefined : tabPermissionModes(hello.permissionModes, activeTab?.agent)}
            />
          </PanelErrorBoundary>
        )}
        <HintLayer root={startScreenRef.current} hints={hints} typed={hintTyped} />
      </div>
    );
  }

  // Authoritative, server-originated. `activeTurnId` is set by a real TurnStarted event from the
  // provider and cleared by a real TurnCompleted -- never by this component optimistically marking
  // a turn as started when the user pressed Send. The reducer also clears it on a session that ends
  // without one, because no TurnCompleted is ever coming for a session that is gone.
  const turnInProgress = state.activeTurnId !== null;
  /** Enter while the turn runs puts the message into it (the agent reads it at its next step) when the
   *  tab's agent says it can; otherwise the message is held for the turn's end. */
  const enterSteers = (queueSteers ?? state.capabilities.steer) === true;
  /* The conversation is on its way out: Rust already owns the backend on a shutdown worker and will
     refuse every command until that finishes. The composer must reflect that rather than accepting
     input it cannot deliver. */
  const handingOff = tabs !== null && handoffRequests.has(tabs.active);
  const bandPicks = (!isExternalAgent(activeTab?.agent) || isAcpAgent(activeTab?.agent)) && !sessionEnded && !handingOff;

  /** `a`/`d`/`D` on a card, and Enter on a card's own button. `stillThere` says whether the card the key was aimed at
   *  can still take it, or is `null` when there is no card for the cursor (the "no card here" flash). The answer runs
   *  `TYPING_GUARD_MS` later, only if the key stood alone before and nothing cancels the wait; by then the card must
   *  still be there (`stillThere`) and the panel still in BROWSE. `key` is what the guard judges the key before
   *  against: only Enter passes it, for the guard's walk exception (`TypingGuard.defer`). */
  function waitThenAnswer(
    event: KeyboardEvent<HTMLDivElement>,
    typedAt: number,
    stillThere: (() => boolean) | null,
    run: () => void,
    key = "",
  ) {
    if (stillThere === null) {
      const waitingElsewhere = allCards(stateRef.current).some((p) => !answeredRef.current.has(p.permissionId));
      showFlash(waitingElsewhere ? CARD_ELSEWHERE_FLASH : NO_CARD_FLASH);
      return;
    }
    const waiting = typingGuard.defer(
      typedAt,
      event.repeat,
      () => {
        if (stillThere() && modeRef.current === "browse") run();
      },
      TYPING_FLASH,
      key,
    );
    if (!waiting) showFlash(TYPING_FLASH);
  }

  /** `a`/`d` answer the card at timeline index `target` by its own
   *  `permissionId`, through `answerPermission` -- the path the card's buttons take -- never by
   *  clicking a button a DOM query found. The query this replaced indexed every
   *  `[data-nav-stop="row"]` in the subtree with a TIMELINE index, so rows a model reply drew shifted
   *  it, and `a` on the card the cursor showed approved the one above it. The timeline item
   *  is the card the cursor is drawn on, by construction (`MessageList` draws `.row-current` on
   *  `timeline[cursor]`). `d` sends the reason already typed into the card's box, as its Deny does. */
  function answerCardByKey(
    event: KeyboardEvent<HTMLDivElement>,
    typedAt: number,
    target: number | null,
    decision: "allow" | "deny",
  ) {
    const item = target === null ? undefined : timeline[target];
    if (decision === "allow" && item?.kind === "permission" && !cardApprovable(item.request)) {
      showFlash(NOT_APPROVABLE_FLASH);
      return;
    }
    const permissionId = item?.kind === "permission" ? item.request.permissionId : null;
    const tab = activeTabRef.current;
    const stillThere =
      permissionId === null
        ? null
        : () => {
            // The same card, in the same tab, still pending and not yet answered; a card whose
            // session ended is inert (its buttons are disabled), so the key is too.
            const now = stateRef.current;
            return (
              activeTabRef.current === tab &&
              !answeredRef.current.has(permissionId) &&
              now.status.kind !== "unavailable" &&
              now.status.kind !== "closed" &&
              allCards(now).some((p) => p.permissionId === permissionId)
            );
          };
    waitThenAnswer(event, typedAt, stillThere, () => {
      if (permissionId === null) return;
      const reason = decision === "deny" ? permissionReasons.current.get(permissionId) || undefined : undefined;
      answerPermission(permissionId, decision, reason, undefined, "key");
    });
  }

  /** One key, not two steps: INPUT's `Ctrl+y` approves the
   *  ACTIVE tab's OLDEST waiting card (`oldestWaitingPermission`, the card a card landing takes)
   *  without leaving INPUT. Only from the composer's own box -- not the `Ctrl+r` search field,
   *  whose text is a query. With no card waiting it does nothing and leaves the key to the box.
   *  Otherwise the key is claimed (so the box never acts on it: its text, caret and history stay as
   *  they were) and runs the typing guard's rule as `a` does (`TypingGuard.defer`, no new clock): only a Ctrl+y
   *  with no key within `TYPING_GUARD_MS` before it -- so readline's `Ctrl+u`/`Ctrl+w` then `Ctrl+y`
   *  yank never approves -- and none within it after, and never a held key: it also waits for the `y`
   *  to be let go (`TypingGuard.deferUntilReleased`), so a Ctrl+y still down when the window ends
   *  approves only at its release, and its first repeat refuses it. At fire time the
   *  same card must still wait in the same tab, the session still live and the panel still in INPUT;
   *  it is answered by its own `permissionId` through `answerPermission`, the path its buttons and
   *  `a` take, never by pressing a button a DOM query found. */
  function approveOldestByKey(event: KeyboardEvent<HTMLDivElement>, typedAt: number, afterKill: boolean) {
    const box = event.target;
    if (!isComposerBox(box) && !(box instanceof HTMLElement && box.closest(".question-dock"))) return;
    const index = sessionEnded ? null : oldestWaitingPermission(timeline, answeredRef.current);
    const item = index === null ? undefined : timeline[index];
    if (item?.kind !== "permission") {
      // Nothing a `Ctrl+y` may answer waits. A broker card does, though: only a key on its own row answers it, so
      // the key is claimed and says so, rather than yanking text as if nothing were waiting.
      const brokerWaiting = !sessionEnded && timeline.some(
        (row) => row.kind === "permission" && row.request.editorCard !== undefined && !answeredRef.current.has(row.request.permissionId),
      );
      if (brokerWaiting) {
        event.preventDefault();
        showFlash(CTRL_Y_EDITOR_CARD_FLASH);
      }
      return;
    }
    event.preventDefault();
    if (!cardApprovable(item.request)) {
      showFlash(NOT_APPROVABLE_FLASH);
      return;
    }
    const permissionId = item.request.permissionId;
    const tab = activeTabRef.current;
    const targetKey = `${tab}:${permissionId}`;
    // Three refusals ahead of the wait, each saying why. The typing guard's before-half first (the
    // flash the fast `Ctrl+u` `Ctrl+y` already gave); then a kill right before it, whatever the pause;
    // then a card that has not been the one the band names for `TYPING_GUARD_MS`
    // -- which also refuses while this panel's own approval is still moving the band on.
    if (!typingGuard.mayAnswerNow("", typedAt, event.repeat)) {
      showFlash(CTRL_Y_TYPING_FLASH);
      return;
    }
    if (afterKill) {
      showFlash(CTRL_Y_YANK_FLASH);
      return;
    }
    const target = ctrlYTargetRef.current;
    if (target.key !== targetKey || !target.settled) {
      showFlash(CTRL_Y_TARGET_FLASH);
      return;
    }
    const waiting = typingGuard.deferUntilReleased(
      typedAt,
      event.repeat,
      () => {
        const now = stateRef.current;
        if (
          activeTabRef.current !== tab ||
          modeRef.current !== "input" ||
          // The box still has the keys -- a click into the `Ctrl+r` search, or
          // anywhere else, is no key and cancels nothing on its own.
          document.activeElement !== box ||
          !box.isConnected ||
          // Still the card the band named when the key was pressed.
          ctrlYTargetRef.current.key !== targetKey ||
          answeredRef.current.has(permissionId) ||
          now.status.kind === "unavailable" ||
          now.status.kind === "closed" ||
          !allCards(now).some((p) => p.permissionId === permissionId)
        ) {
          return;
        }
        answerPermission(permissionId, "allow");
      },
      CTRL_Y_TYPING_FLASH,
      heldKeyName(event.nativeEvent),
      CTRL_Y_HELD_FLASH,
    );
    if (!waiting) showFlash(CTRL_Y_TYPING_FLASH);
  }
  // ---- BROWSE visual mode ----
  //
  // Handled entirely in the root's `onKeyDownCapture`, never in `onKeyDown` below: every
  // VISUAL/V-LINE key is decided by `resolveKey`'s own visual branch and applied here, and the
  // capture handler stops its propagation so no card, button or reason box downstream ever sees it
  // (the reason box's own Enter/click handlers in particular).

  function visualRoot(): HTMLElement | null {
    return containerRef.current;
  }

  /** The region's own boundary -- "it never leaves `.message-list`", "inside the list" -- which is
   *  narrower than `visualRoot()` (the whole `.agent-ui-conversation` root, which also holds the
   *  activity line, the composer and the status band). Motions and the "still highlighted" check used to take `visualRoot()` itself,
   *  so a step past the last row could land in, and `y` could copy from, that live chrome. */
  function messageListEl(): HTMLElement | null {
    return visualRoot()?.querySelector<HTMLElement>(".message-list") ?? null;
  }

  /** The row (frozen DOM) `caret` sits in, or `null` if `caret` is not inside any row -- the shared
   *  first half of `rowKeyOfCaret` below and of the
   *  `scroll-line` case's own line-height read (`rowTextElement` needs a row, never `.message-list`
   *  itself: see its own doc comment on why reading the list directly gave a fraction of a real
   *  line). Extracted rather than duplicated when the second caller needed the same "caret -> row"
   *  step `rowKeyOfCaret` already had, one line further. */
  function rowElementOfCaret(caret: Caret): HTMLElement | null {
    const root = visualRoot();
    if (root === null) return null;
    const el = caret.node instanceof Element ? caret.node : caret.node.parentElement;
    if (el === null) return null;
    return rowOf(root, el);
  }

  /** The row `caret` sits in, as a key into `frozenTimeline` -- rows are matched by
   *  key, never by index, because a row's position in the FROZEN timeline and the LIVE one can
   *  differ once a linked card arrives during VISUAL. `null` when `caret` is
   *  not inside any row (should not happen: nothing lets the caret leave `.message-list`, and every
   *  row in the frozen DOM belongs to a row of `frozenTimeline`). */
  function rowKeyOfCaret(caret: Caret): string | null {
    const root = visualRoot();
    if (root === null || frozenTimeline === null) return null;
    const row = rowElementOfCaret(caret);
    if (row === null) return null;
    const frozenIndex = rowIndexOf(root, row);
    if (frozenIndex === null) return null;
    return frozenTimeline[frozenIndex]?.key ?? null;
  }

  /** Places the LIVE row cursor on the row named by `key` (found in the frozen DOM by
   *  `rowKeyOfCaret` above), without moving the view (`landingRef` "keep", the same convention every
   *  other cursor-preserving move in this file uses). A `key` no longer in the live timeline (should
   *  not happen for the row VISUAL was just looking at) is simply left alone.
   *
   *  `"keep"` used to be set even when the live index equals the
   *  cursor already on screen (the common `v…y`/`Esc`-on-the-cursor-row case) -- `landingRef` is
   *  consumed only by the `[cursor]` effect's own reveal, which never fires for a `setCursor` that
   *  changes nothing, so a stale `"keep"` sat there and silently swallowed the NEXT landing's reveal
   *  instead (a `/` search, `n`, a HINT row, `D`). Set only when the index actually changes, the same
   *  guard the `arrive` effect's own `"keep"` write already uses. */
  function landCursorOnRowKey(key: string | null) {
    if (key === null) return;
    const liveIndex = indexOfKey(timeline, key);
    if (liveIndex === null) return;
    if (liveIndex !== cursorRef.current) landingRef.current = "keep";
    setCursor(liveIndex);
  }

  /** Keeps the moving end on screen through every scrollable box between it and the list -- a
   *  260px `.tool-result-body`, a `.table-scroll` sideways, then the list itself -- each nudged the
   *  least that reveals the caret's own point (`visual.ts`'s `revealCaret`), never a whole enclosing
   *  element's `scrollIntoView`, and announces the move through `noteUserScroll`.
   *
   *  An earlier body called
   *  `scrollIntoView` on the caret's PARENT element, which for text sitting directly inside the
   *  260px box IS that box -- `scrollIntoView` moves an element's ANCESTORS, never its own
   *  `scrollTop`, so a caret past the box's own visible bottom was never brought back on screen.
   *
   *  The announcement covers the WHOLE key, not only this
   *  last nudge. A `j`/`k` whose goal-column snap or geometry probe had to reveal the line it
   *  measured (`visual.ts`) has already scrolled the list by the time this runs, so the caret is on
   *  screen and nothing here moves -- which used to mean nothing was announced, `follow.ts` never
   *  learned the view moved, and a `k` inside a followed reply left the list snapping back to the
   *  bottom when the region ended. `listTopBefore` is the list's `scrollTop` when the key arrived;
   *  any change from it is announced, `"up"` when the list went up (following stops at once, as
   *  BROWSE's own `k` does), `"unknown"` otherwise (the scroll's direction decides). Without real
   *  layout (jsdom) `revealCaret` does nothing and the old coarse fallback runs. */
  function scrollCaretIntoView(caret: Caret, listTopBefore: number | null) {
    const list = messageListEl();
    if (list === null) return;
    const moved = revealCaret(caret, list);
    if (!moved && caretRectMissing(caret)) {
      const startEl = caret.node instanceof Element ? caret.node : caret.node.parentElement;
      startEl?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
    }
    const listMoved = listTopBefore !== null && list.scrollTop !== listTopBefore;
    if (listMoved) noteUserScroll(list, list.scrollTop < listTopBefore! ? "up" : "unknown");
    else if (moved) noteUserScroll(list, "unknown");
  }

  /** `v`/`V` from BROWSE (`v` starts CARET, `V` starts V-LINE directly). Refuses with a flash and does nothing else when there is nowhere to start
   *  from (no row under the cursor, or the keys are on a banner/Stop rather than a row). Right
   *  after a HINT landed on a code block, the entry caret is that block's own first character
   *  (`landedCodeBlock`, `y`'s own `landedCode` read the same way); otherwise the
   *  cursor row's own first selectable character ON SCREEN (`entrySelectableCaret`: the first
   *  one under the list's top edge when the row's own top has scrolled off it). A pending BROWSE
   *  count is dropped.
   *
   *  `landedCodeBlock`: `onKeyDown`
   *  clears `copyCodeRef.current` (for its OWN `"copy"` case, the plain `y` a HINT-landed code block
   *  answers) before its switch ever reaches `case "visual"`/`case "caret"` below -- reading the ref
   *  here, as the first version of this function did, always saw `null`, so `v` right after a HINT
   *  never started inside the block. The caller now captures the ref's value once, alongside its own
   *  `landedCode`, and passes it through. */
  function enterRegion(line: boolean, landedCodeBlock: HTMLElement | null) {
    const root = visualRoot();
    if (root === null) return;
    if (edgeFocused) {
      showFlash("v selects from a row — j / k onto one");
      return;
    }
    const row = conversationRows(root)[cursor] ?? null;
    if (row === null) {
      showFlash("nothing to select");
      return;
    }
    const codeBlock = landedCodeBlock;
    const container = codeBlock !== null && codeBlock.isConnected && row.contains(codeBlock) ? codeBlock : row;
    const caret = container === row ? entrySelectableCaret(container, messageListEl()) : firstSelectableCaret(container);
    if (caret === null) {
      showFlash("nothing to select");
      return;
    }
    const sel = window.getSelection();
    if (sel === null) return;
    // BROWSE's `v` (line === false) lands in CARET; `V` (line === true) still goes straight
    // to V-LINE.
    const model: VisualModel = { anchor: caret, cursor: caret, kind: line ? "line" : "caret", goalX: null };
    visualModelRef.current = model;
    visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, model);
    // A count typed before `v` belongs to nothing now; VISUAL's own count starts fresh.
    countRef.current = null;
    visualCountRef.current = null;
    regionPendingGRef.current = null;
    setFrozenSnapshot({ state, expanded, detailed, ruleOffers, answeredPermissions, sessionEnded, turnDurations });
    modeRef.current = line ? "vline" : "caret";
    setMode(modeRef.current);
    root.focus({ preventScroll: true });
  }

  /** The guts of `exitRegion` below, minus its own `modeRef`/`setMode` write -- split out so the `[mode]` backstop effect further down
   *  can run the SAME cleanup after `mode` has ALREADY left "visual"/"vline" some other way, when
   *  `exitRegion`'s own guard would already have returned having done nothing. Clears the selection
   *  VISUAL itself built, and only that one (a mouse selection made meanwhile is left alone, checked with
   *  `selectionMatchesBuild`), and -- the actual bug -- `frozenSnapshot`: left set,
   *  `MessageList` (`state={frozenSnapshot?.state ?? state}` below) keeps showing a stale, answered,
   *  or foreign-tab conversation regardless of what `mode` now is, which is what let a keypress meant
   *  for the card ON SCREEN answer a DIFFERENT, live one instead (reproduced: a session
   *  ends during VISUAL, the tab is switched, and the still-frozen card's `a` approves the new tab's). */
  function clearVisualLeftovers() {
    const sel = window.getSelection();
    const built = visualBuiltRef.current;
    const root = visualRoot();
    if (sel !== null && built !== null && root !== null && selectionMatchesBuild(sel as unknown as SelectionLike, built, root)) {
      sel.removeAllRanges();
    }
    visualModelRef.current = null;
    visualBuiltRef.current = null;
    visualCountRef.current = null;
    regionPendingGRef.current = null;
    countRef.current = null;
    setFrozenSnapshot(null);
  }

  /** The band's own word for a region mode: `CARET`, `V-LINE`, else `VISUAL`. */
  function regionModeName(m: PanelMode): string {
    return m === "caret" ? "CARET" : m === "vline" ? "V-LINE" : "VISUAL";
  }

  /** Whether `m` is one of the region's own three modes (CARET/VISUAL/V-LINE together), the one
   *  check every exit route and the `[mode]` backstop share -- extracted once a third
   *  mode was added so no call site has to spell the disjunction out by hand. */
  function isRegionMode(m: PanelMode): m is "caret" | "visual" | "vline" {
    return m === "caret" || m === "visual" || m === "vline";
  }

  /** Ends the WHOLE region (CARET, VISUAL or V-LINE), back to BROWSE, wherever it is
   *  called from -- a no-op outside all three. `modeRef.current` is set synchronously (not only
   *  through `setMode`) so code that reads it immediately after calling this -- the `tabs` dispatch
   *  arm's own `saveView`, in particular -- never saves "caret"/"visual"/"vline" as a tab's parked
   *  mode (nothing in this panel ever restores one). */
  function exitRegion() {
    if (!isRegionMode(modeRef.current)) return;
    clearVisualLeftovers();
    modeRef.current = "browse";
    setMode("browse");
  }

  /** The whole CARET/VISUAL/V-LINE key table, run from the root's `onKeyDownCapture`, ahead of
   *  `onKeyDown` below and of every descendant's own keydown handler. A no-op outside the region
   *  (returns at once, claiming nothing, so `onKeyDown` and everything under the root see the key
   *  exactly as they do today). */
  function onVisualKeyDownCapture(event: KeyboardEvent<HTMLDivElement>) {
    const currentMode = modeRef.current;
    if (!isRegionMode(currentMode)) return;
    const root = containerRef.current;
    // A region key always targets the root; should focus have escaped it without going
    // through one of the explicit exit routes elsewhere in this file, this is the backstop -- end
    // the region and swallow the key so no descendant (a reason box, a button) ever sees it.
    if (root === null || event.target !== root) {
      exitRegion();
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    const typedAt = typingGuard.timeOf(event);
    // The typing guard sees every region key too, the same as every other keydown this panel
    // claims -- a card `a`/`d`/`D` deferred moments before the region started stays cancellable by
    // it.
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    const model = visualModelRef.current;
    if (model === null) {
      exitRegion();
      return;
    }
    // A bare modifier's own keydown never drops a region-local pending `g` -- the same
    // convention BROWSE's own `pendingRef` gets, above (`onKeyDown`'s own `isModifierKey` check):
    // physical Shift on the way to `G`/`V`/`$` must not silently cancel a `gg` in progress. Checked
    // before `count`/`regionPendingGRef` are read and cleared below, so both are simply left alone.
    if (regionPendingGRef.current !== null && isModifierKey(event.key)) {
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    // The list is the boundary, not `root` (`visualRoot()`, the whole conversation root, which also
    // holds live chrome the freeze never touches: the activity line, the composer, the status band).
    // A motion or the "still
    // highlighted" check taking `root` here could step into, and `y` could copy from, that chrome.
    const listEl = messageListEl();
    const count = visualCountRef.current;
    visualCountRef.current = null;
    const pendingG = regionPendingGRef.current;
    regionPendingGRef.current = null;
    const action = resolveKey(currentMode, event.nativeEvent as unknown as KeyLike, {
      sessionEnded,
      count,
      pending: pendingG ?? undefined,
      turnRunning: state.activeTurnId !== null,
    });
    if (action === null) {
      // Everything that reaches here with `action === null` is genuinely unclaimed --
      // idle Ctrl+c in VISUAL/V-LINE (native copy), or `gv`'s own reservation (which does nothing),
      // whose consumed pending `g` stays dropped, already cleared above. Nothing is swallowed:
      // the browser's own default runs undisturbed, and the mode stays exactly as it was -- "keeps
      // a count" included: a count typed before a bare
      // modifier's own keydown (physical Shift on the way to `$`/`V`) was left cleared above with
      // nothing to restore it. (A bare modifier itself never reaches this branch: the early return
      // above only covers one WITH a pending `g`; the ordinary case is `resolveCaretKey`/
      // `resolveVisualKey`'s own `isModifierKey` check, which also returns `null`, restored here.)
      visualCountRef.current = count;
      if (cancelled !== null) showFlash(cancelled);
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    const sel = window.getSelection();
    // Ending the region with THIS key means its own physical auto-repeats must never fall
    // through to BROWSE's/INPUT's meaning for the same key once `mode` has changed away (a held `d`
    // denying a card, a held `y` copying the row it just landed on, a held `>` typing a literal
    // character into the composer) -- `promptSwallowKeyRef` is the exact mechanism `answerConfirm`
    // already uses for a y/n prompt's own held key (App.tsx's own doc comment on it), reused here
    // for the same class of bug. Set right before ending on every route that leaves the region from
    // a keypress, so the very next (repeat) keydown of this key is swallowed by `answerConfirm`'s
    // own check -- which runs ahead of this handler on every key -- before it ever reaches BROWSE's
    // or INPUT's table.
    function endRegionFromKey() {
      promptSwallowKeyRef.current = event.key;
      exitRegion();
    }
    switch (action.kind) {
      case "count":
        visualCountRef.current = accumulateMotionCount(count, action.digit);
        return;
      case "pending":
        // The first `g` of `gg`, or of the `gv` reservation -- armed for the next key.
        regionPendingGRef.current = action.prefix;
        return;
      case "vmove": {
        // Motions repeat, as a held `l` does in vim -- no repeat guard here. `gg`/`G`
        // take no count and drop one (vim's `3G` is a buffer line number, which the panel does not
        // have) -- always exactly one step, regardless of what `count` held.
        if (sel === null || listEl === null) return;
        const times = action.motion === "gg" || action.motion === "G" ? 1 : Math.max(1, count ?? 1);
        const listTopBefore = listEl.scrollTop;
        const next = repeatMotion(sel as unknown as SelectionLike, model, action.motion, times, listEl);
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        scrollCaretIntoView(next.cursor, listTopBefore);
        return;
      }
      case "vswap": {
        // A repeated `o` does nothing. Not reachable from CARET (`resolveCaretKey` never
        // returns it), but the guard costs nothing to keep uniform.
        if (event.repeat || sel === null) return;
        const next = swapEnds(model);
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        scrollCaretIntoView(next.cursor, listEl?.scrollTop ?? null);
        return;
      }
      case "vtoggle": {
        // From CARET, `v`/`V` always START a selection mode; from VISUAL/V-LINE, the OTHER key
        // switches to the sibling one (the mode's OWN key is `vback`, below, never reaches here).
        // A repeated `v`/`V` does nothing either way.
        if (event.repeat || sel === null) return;
        const next: VisualModel = { ...model, kind: action.line ? "line" : "char" };
        visualModelRef.current = next;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, next);
        modeRef.current = action.line ? "vline" : "visual";
        setMode(modeRef.current);
        return;
      }
      case "vback": {
        // VISUAL/V-LINE's own key, or `Esc`, goes back to CARET on the MOVING end -- the
        // region and its freeze stay on (Esc from VISUAL is not an exit). A
        // repeated `v`/`V`/`Esc` does nothing.
        if (event.repeat || sel === null) return;
        const caretModel = caretModelOf(sel as unknown as SelectionLike, model);
        visualModelRef.current = caretModel;
        visualBuiltRef.current = rebuildSelection(sel as unknown as SelectionLike, caretModel);
        modeRef.current = "caret";
        setMode("caret");
        // The key that leaves a mode swallows its own
        // repeats until keyup. A held `Esc` used to step back to CARET on its first press and then
        // end the whole region on its first auto-repeat (CARET's own `Esc`), dropping the caret and
        // the freeze the user only meant to keep.
        promptSwallowKeyRef.current = event.key;
        return;
      }
      case "vopen-path": {
        if (event.repeat) return;
        if (currentMode === "caret") {
          const row = rowElementOfCaret(model.cursor);
          const ref = row !== null && listEl?.contains(row)
            ? pathAtCaret(row, model.cursor.node, model.cursor.offset) : null;
          if (ref === null) {
            showFlash("no path under the caret");
            return;
          }
          landCursorOnRowKey(rowKeyOfCaret(model.cursor));
          endRegionFromKey();
          openPath(ref);
          return;
        }
        if (sel === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing opened; v to start again");
          endRegionFromKey();
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        let paths: PathRef[];
        if (currentMode === "vline") {
          paths = pathsInText(text);
          if (paths.length === 0) {
            showFlash("gf: no path in the selected lines");
            return;
          }
        } else {
          const ref = text.trim().length > 256 ? null : parsePath(text);
          if (ref === null) {
            const trimmed = text.trim();
            const clean = revealedText(trimmed);
            const chars = Array.from(clean);
            showFlash(trimmed === "" ? "gf: nothing selected" : /[\s]/.test(trimmed)
              ? "gf: the selection is not one path (it holds a space or a line break)"
              : `gf: the selection is not a path: ${chars.slice(0, 60).join("")}${chars.length > 60 ? "…" : ""}`);
            return;
          }
          paths = [ref];
        }
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        if (paths.length > 1) {
          beginPathPick(paths, sel.rangeCount > 0 ? conversationRows(root) : [], sel.rangeCount > 0 ? sel.getRangeAt(0).cloneRange() : undefined);
        }
        landCursorOnRowKey(rowKeyOfCaret(earlier));
        endRegionFromKey();
        if (paths.length === 1) openPath(paths[0]);
        return;
      }
      case "vyank": {
        // A repeated `y` does nothing (and must not fall through as BROWSE's own `y`, which
        // `endRegionFromKey`'s `promptSwallowKeyRef` guards against for the repeat that follows).
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // Copy only what the user actually saw highlighted, and still inside `.message-list`.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        // After a charwise yank the row cursor goes to the row holding the FIRST yanked
        // character (`nvim: change.txt:1200`), i.e. the earlier of the two ends -- not wherever the
        // moving end (`cursor`) happened to be left.
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        const key = rowKeyOfCaret(earlier);
        landCursorOnRowKey(key);
        copied(text, key ?? undefined);
        endRegionFromKey();
        return;
      }
      case "vquote": {
        // `>` quotes the highlighted text into the tab's draft and ends the region into
        // INPUT -- never sends anything. A held `>` quotes once, its repeats swallowed by
        // `promptSwallowKeyRef` (set below) until keyup, so none is typed into the composer.
        if (event.repeat) return;
        if (sel === null || root === null || listEl === null) {
          endRegionFromKey();
          return;
        }
        const built = visualBuiltRef.current;
        // The text is exactly what y would copy, through the same check and shield; a failed check
        // quotes nothing, as y does -- same message, same whole-region ending as `vyank`'s own.
        if (built === null || !selectionMatchesBuild(sel as unknown as SelectionLike, built, listEl)) {
          showFlash("selection changed under it — nothing copied; v to start again");
          endRegionFromKey();
          return;
        }
        // Two refusals: an ended session (the box is disabled, as `i` is refused there) and
        // the draft open in nvim (`scratchEditing`) -- VISUAL/V-LINE STAYS, unlike every other exit
        // above, so the user can back out with `Esc`/`y` instead of losing the selection.
        if (sessionEnded) {
          showFlash("this session has ended — nothing to quote into");
          return;
        }
        if (scratchEditing) {
          showFlash("the draft is open in nvim — finish there first");
          return;
        }
        const text = copySelectionText(sel as unknown as SelectionLike, root, model);
        const quote = formatQuote(text);
        if (quote === null) {
          showFlash("nothing to quote");
          return;
        }
        const earlier = compareCarets(model.anchor, model.cursor) <= 0 ? model.anchor : model.cursor;
        const key = rowKeyOfCaret(earlier);
        landCursorOnRowKey(key);
        const nextDraft = appendQuote(draftRef.current, quote);
        promptSwallowKeyRef.current = event.key;
        exitRegion();
        modeRef.current = "input";
        setMode("input");
        setComposerCaret("end");
        setComposerFocusRequest((n) => n + 1);
        restoreSeq.current += 1;
        setRestoredDraft({ text: nextDraft, seq: restoreSeq.current });
        mirrorDraft(nextDraft);
        return;
      }
      case "vend": {
        // A repeated `Esc` does nothing (the one that ended a mode is swallowed upstream by
        // `promptSwallowKeyRef`; this covers a repeat that reaches CARET any other way).
        if (event.repeat && action.key === "Escape") return;
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        // `Esc` says nothing; any other unbound key says what it is not, naming CARET's
        // own hint or VISUAL/V-LINE's. `action.key`
        // interpolated raw read as "VISUAL ended:   is not a VISUAL key" for Space (the leader,
        // likely pressed out of habit) -- a human name for the one key worth naming, the same
        // `humanKey` uses for the leader elsewhere.
        //
        // Enter used to get the same treatment as
        // any other unbound key ("is not a VISUAL key (y copies)"), which is true but useless --
        // it never pointed at the one route that actually reaches a folded row from here
        // (text not drawn is not reachable; Enter before v unfolds a fold or a run). Enter
        // gets its own short flash naming that route instead, in every mode; every other unbound
        // key keeps its existing wording unchanged.
        if (action.key !== "Escape") {
          const keyName = action.key === " " ? "Space" : action.key;
          if (action.key === "Enter") {
            const modeName = regionModeName(currentMode);
            showFlash(`${modeName} ended: Enter is not a ${modeName} key -- in BROWSE, Enter unfolds a row, then v selects it`);
          } else if (currentMode === "caret") {
            showFlash(`CARET ended: ${keyName} is not a CARET key (v selects)`);
          } else {
            showFlash(`VISUAL ended: ${keyName} is not a VISUAL key (y copies)`);
          }
        }
        return;
      }
      case "vswallow":
        // A modified key this table does not otherwise claim. Already prevented/stopped
        // above; nothing else changes -- except an idle `Ctrl+c` in CARET specifically (a
        // consequence of the block caret), which is claimed rather than left to the engine's native copy and
        // says why.
        if (currentMode === "caret" && event.key === "c" && event.ctrlKey) {
          showFlash("nothing selected — v, then y");
        }
        return;
      case "scroll-line": {
        // Ctrl+e/Ctrl+y scroll the frozen list one line -- counted,
        // the same convention BROWSE's own identical action uses -- without moving the caret,
        // the selection, or the mode: distinct from every other case in this switch, none of which
        // touch `listEl.scrollTop` while leaving `model`/`sel` alone. `computedLineHeight` is read
        // off the caret's own row text (`rowTextElement`), never `.message-list` itself, which sets
        // neither font-size nor line-height of its own (that helper's doc comment: reading the list
        // directly gave a line a fraction of the real one, the exact bug it exists to avoid).
        if (listEl === null) return;
        const row = rowElementOfCaret(model.cursor);
        const scrollTimes = Math.max(1, count ?? 1);
        listEl.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : listEl) * scrollTimes;
        return;
      }
      case "keymap":
        // `?` ends the region and opens the keymap -- the which-key box, the panel's only
        // discovery route.
        landCursorOnRowKey(rowKeyOfCaret(model.cursor));
        endRegionFromKey();
        setKeymapOpen(true);
        return;
      case "interrupt":
        // The one Ctrl chord the region keeps its BROWSE meaning for -- the region itself
        // stays on.
        interrupt();
        return;
      default:
        return;
    }
  }

  /** The key table's home: mode + key + context in, an action out, applied here. Only claims what
   *  `resolveKey` claims -- an unrecognised key, or one INPUT leaves to the input method (a
   *  composing Escape), falls straight through with no `preventDefault`. That is what keeps
   *  Ctrl+h/l (pane switch), prefix r (panel reload) and Ctrl+Shift+O working: GTK takes those
   *  in the capture phase and this handler must not fight it for a chord it does not own.
   *
   *  Also stays out of any OTHER editable control in this subtree -- a permission card's
   *  deny-reason box, today. Found in review: with no guard, `j` typed there never reached the
   *  input (claimed and `preventDefault`ed as a cursor move) and `i` stole focus into the composer
   *  mid-word. The composer's own textarea is the one exception: while INPUT is active it lives
   *  inside `.composer`, and letting its keydowns reach here is exactly the path that lets `Escape`
   *  (bubbling from it) leave INPUT -- `resolveKey`'s "input" branch reacts to nothing else.
   *
   *  And out of any ACTIVATABLE control, for the reason spelled out on `isActivatableControl`: a
   *  focused button's activation IS the default action of the key, so claiming the key removes the
   *  only keyboard route to Approve/Deny/Stop/Dismiss/Cancel. That was a real, reported regression,
   *  not a hypothetical. */
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    // The window-close confirmation prompt owns every key, first of all, ahead of the `?` keymap
    // overlay and everything else below: tmux's own `confirm-before` prompt takes the
    // keys the same way.
    if (answerConfirm(event)) return;
    if (tasksOpen) {
      onTasksKey(event);
      return;
    }

    // Every other keydown is "a key" to the typing guard, ahead of the overlays
    // below so a key typed into one still counts. It cancels a waiting `a`/`d`/`D` -- which says
    // so in the band -- and this key still does whatever it does. Shift+Tab as WebKitGTK delivers
    // it (`isShiftTab`) is the walk key `Tab`, like the standard shape.
    const typedAt = typingGuard.timeOf(event);
    // A held `j`/`k` glides (`./jkHold`, started with the other motions below). A repeat of its key only
    // keeps it going -- still a key to the guard below, and still the user's own scroll to the list. Any
    // other key ends it first, as letting go would, and then acts on the panel as that leaves it. The
    // release is recorded before this key is: the guard then judges the key against the moment the held
    // one was let go, not against its last repeat.
    const glidingList = containerRef.current?.querySelector<HTMLElement>(".message-list") ?? null;
    const keepsGliding =
      glidingList !== null &&
      isHolding(glidingList) &&
      event.repeat &&
      keepHoldAlive(glidingList, event.code || event.key, performance.now());
    if (glidingList !== null && !keepsGliding) releaseHold(glidingList, performance.now());
    // The flash is the cancelled wait's own (`TypingGuard.defer`'s `cancelledFlash`): a cancelled
    // `f` or `L` says what did not happen, never the `a`/`d` text.
    const cancelled = typingGuard.onKey(isShiftTab(event) ? "Tab" : event.key, typedAt);
    if (cancelled !== null) showFlash(cancelled);
    if (keepsGliding) {
      holdClockRef.current = { typed: typedAt, perf: performance.now() };
      noteUserScroll(glidingList, holdDirectionRef.current > 0 ? "down" : "up");
      lastKeyWasKillRef.current = false;
      tabAtKeyRef.current = null;
      event.preventDefault();
      return;
    }
    // Whether the key before this one was a kill in the composer, and
    // whether this one is (a bare modifier is neither, and leaves it alone).
    const afterKill = lastKeyWasKillRef.current;
    if (!isModifierKey(event.key)) lastKeyWasKillRef.current = isKillKey(event);
    // A plain Tab's own focus move -- the browser's, right after this keydown -- is
    // a landing, which the root's `onFocus` records against this key's count. Any other key ends it.
    // Only a Tab whose default is still alive moves focus. One that is claimed moves
    // nothing, and the next focus no key made -- a mouse pressed on Approve and dragged off it -- must
    // not become its landing. A box claims some before this handler runs (the
    // `/` and `:` lines take every Tab), but this handler swallows others further down (the cancel
    // after `g`/`z`/`[`/`]`, the leader's sequence, `gf`'s pick, the `?` overlay, the detail
    // popover), so the default is known only once every handler has run: the native event is kept,
    // and `onFocus` reads it when the focus arrives.
    tabAtKeyRef.current =
      event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey
        ? { atKey: typingGuard.keyCount(), event: event.nativeEvent }
        : null;
    // A card's own answer buttons (Approve, Deny, Always allow): see the Enter/Space rules below.
    const onAnswerButton =
      isActivatableControl(event.target) && (event.target as HTMLElement).closest("[data-nav-action]") !== null;
    // The two modal overlays just below return ahead of those rules, so a
    // card button still holding focus under one -- a mouse pressed on Approve and dragged off it, with
    // no click -- took Enter or Space as the browser's own activation, answering the card with no
    // guard at all. Claimed here too: under either overlay those keys on a card button do nothing.
    if ((chooser !== null || slashPicker !== null) && onAnswerButton && (event.key === "Enter" || event.key === " ")) {
      event.preventDefault();
    }
    // The chooser is modal: it is drawn inside this root, and a key it does
    // not handle itself (`a`, `d`, anything but its own j/k/Enter/x/`/`/q/Esc/g/G/Ctrl+r/Shift+Tab)
    // bubbled here and reached `resolveKey`, whose `a`/`d` pressed the Approve/Deny of a card the
    // chooser was covering. It returns only AFTER the typing guard above has seen the key, so a key
    // typed into the chooser still counts as typing once it closes; and with no `preventDefault`,
    // since a key arriving from its filter or rename input is text that input still has to receive.
    if (chooser !== null) return;
    // The picker is modal the same way (its own `j`/`k`/`Enter`/`Escape`/`q`
    // handler `stopPropagation`s every key it claims, so this only ever runs for a key it does
    // not -- exactly the chooser's own reasoning just above).
    if (slashPicker !== null) return;
    // A path pick owns each key until its full label is typed. Cancelling keys never fall
    // through to the conversation underneath it, and auto-repeat never adds another letter.
    if (pathPick !== null) {
      event.preventDefault();
      if (event.repeat) return;
      if (event.key === "Backspace") {
        setPathPick({ ...pathPick, typed: pathPick.typed.slice(0, -1) });
        return;
      }
      if (isPlainAnswerKey(event.nativeEvent as unknown as KeyLike)) {
        const next = pathPick.typed + event.key;
        const index = pathPick.labels.indexOf(next);
        if (index !== -1) openPath(pathPick.refs[index]);
        else if (pathPick.labels.some((label) => label.startsWith(next))) {
          setPathPick({ ...pathPick, typed: next });
          return;
        }
      }
      setPathPick(null);
      return;
    }
    // `gx`'s pick owns the keys the same way, with two differences a
    // copy of the block above would get wrong. Only a PLAIN letter chooses -- no Ctrl, Alt, Meta, Super or
    // AltGr, and no key an input method is composing (`isPlainAnswerKey`, the rule `a`/`d` answer a card by),
    // so Ctrl+a or Alt+s can never open a link. And the `x` of `gx` still held (its auto-repeat), or a bare
    // Shift on its way to a capital, is not a choice: the pick keeps waiting rather than being cancelled the
    // instant it appears. Anything else ends it and does nothing.
    //
    // A key typed into a text field (the composer, a card's reason box) is that field's, not a choice: the pick
    // ends and the key falls through to whatever the field does with it.
    if (linkPick !== null && isEditableElement(event.target)) setLinkPick(null);
    else if (linkPick !== null) {
      event.preventDefault();
      if (event.repeat || isModifierKey(event.key)) return;
      const index = isPlainAnswerKey(event.nativeEvent as unknown as KeyLike) ? HINT_ALPHABET.indexOf(event.key) : -1;
      if (index !== -1 && index < linkPick.length) openUrl(linkPick[index]);
      setLinkPick(null);
      return;
    }
    // `Esc` closes `ContinueInTerminal`'s confirmation, the same way it
    // closes every other small overlay here. Deliberately not an early return for every key, unlike
    // `pathPick` above: the confirm/cancel buttons stay reachable by Tab and by the activatable-
    // control bypass further down, which this must not fight for keys it does not need.
    if (handoffOpen && event.key === "Escape") {
      event.preventDefault();
      setHandoffOpen(false);
      return;
    }
    // A bare modifier's own keydown (the Shift of `gT`) is not "the next key": it
    // must not drop a pending `g`/`z`/`[`/`]`, a count or its which-key box, as vim waits through it.
    // This comment once said "a count" while the check below it only ever read `pendingRef` -- a real `Ctrl+e`
    // arrives as the bare `Control` keydown FIRST (`ctrlKey: true` already set on that very event) and then `e`, so
    // "5 Ctrl+e" reached the unconditional `countRef.current = null` below on `Control`'s own keydown (`pendingRef`
    // is null with no `g`/`z`/`[`/`]` prefix armed) and `e` read the count back as gone, scrolling once instead of
    // five times.
    if ((pendingRef.current !== null || countRef.current !== null) && isModifierKey(event.key)) return;
    const root = containerRef.current;
    const pending = pendingRef.current;
    pendingRef.current = null;
    const count = countRef.current;
    countRef.current = null;
    // The box `pendingRef`'s own reserved prefix (`g`/`z`/`[`/`]`) armed is hidden here, ahead of
    // every early return below, the same way `pending` above is read then reset unconditionally --
    // `resolveKey`'s `"pending"` case below re-arms both if THIS key turns out to continue it. Only
    // while no leader/table sequence is running: that one manages its own box entirely through
    // `advanceSequence`'s "cancel" result below (never here), since clearing it here, before the
    // leader engine gets to read `seqRef.current`, would drop every key of a sequence past the
    // first (a plain `clearSequence()` here read `seqRef` as
    // already `null` on the second key of `<leader>bd`).
    if (seqRef.current === null) {
      cancelBoxTimer();
      hideBox();
    }
    // The `?` keymap overlay owns every key while it is open, ahead of the editable-
    // element and activatable-control checks below and ahead of `resolveKey` entirely -- not
    // routed through the key table at all, because closing is not one of `resolveKey`'s actions
    // (only OPENING is: its `{kind:"keymap"}` is reached below, and only ever with the overlay
    // already closed, since this branch returns first whenever it is open). `preventDefault`
    // unconditionally: `a`/`d` must not reach a permission card hidden underneath, and neither
    // must anything else this table would otherwise claim.
    if (keymapOpen) {
      onKeymapKey(event);
      return;
    }
    // The review overlay owns every key while it is open, the same way and for the same reason: `a`/`d` must
    // not reach a permission card hidden underneath, and nothing else this table claims may act on a
    // conversation the reader cannot see. Its own keys are `./review`'s, not `resolveKey`'s. A bare modifier
    // is not a key (a held Shift on its way to `G`), and neither it nor an unbound key ends a waiting `g`
    // except by being a different key.
    if (review !== null) {
      // The comment input is a text box inside the overlay: what is typed in it is its own (an `a` or a `d`
      // there is a letter, and reaches no card -- nothing below this branch runs). Its Enter and Escape
      // never get here, the box stops them; every other key does nothing at the root.
      const target = event.target;
      if (target instanceof HTMLInputElement && reviewOverlayRef.current?.contains(target)) return;
      event.preventDefault();
      if (isModifierKey(event.key)) return;
      const action = resolveReviewKey(event.nativeEvent as unknown as KeyLike, review.pendingG, review.prompt);
      if (action === null) {
        if (review.pendingG) setReview({ ...review, pendingG: false });
        return;
      }
      // An open hunk taller than its box scrolls inside it first, as a tall tool result does.
      if (action.kind === "move" && scrollBoxFirst(boxUnderCursor(reviewOverlayRef.current), action.delta, TOOL_RESULT_SCROLL_STEP_PX)) {
        if (review.pendingG) setReview({ ...review, pendingG: false });
        return;
      }
      const next = applyReviewKey(review, action, nextRequestId);
      setReview(next.state);
      if (next.effect !== null) runReviewEffect(next.effect, review.tab);
      return;
    }
    // The detail popover, the same way: it owns every key while it is open, ahead of everything
    // below (j/k move, y copies a line, Esc/q close). Its trailing `Continue in a terminal…` row is one more stop
    // past every fact row, so `j`/`k` walk `[0, detail.length]` now rather than `[0, detail.length - 1]`, and `Enter`
    // on that last stop opens `ContinueInTerminal`'s confirmation the same way the row's own click does.
    if (detail !== null) {
      event.preventDefault();
      if (event.key === "Escape" || event.key === "q") setDetail(null);
      else if (event.key === "j") setDetailCursor((c) => Math.min(c + 1, detail.length));
      else if (event.key === "k") setDetailCursor((c) => Math.max(c - 1, 0));
      else if (event.key === "y") {
        const value = detail[detailCursor]?.value;
        if (value !== undefined) copied(value, undefined);
      }
      else if (event.key === "Enter" && detailCursor === detail.length) {
        setDetail(null);
        setHandoffOpen(true);
      }
      return;
    }
    if (isEditableElement(event.target) && !(event.target as HTMLElement).closest(".composer")) {
      // A text box inside a row (a permission card's reason) takes every key as text, except Esc,
      // which hands the keys back to the row it sits in. Esc mid-composition still belongs to the
      // input method, as it does in the composer.
      if (event.key === "Escape" && !event.nativeEvent.isComposing && root !== null) {
        event.preventDefault();
        root.focus({ preventScroll: true });
      }
      return;
    }
    // A focused button activates on Enter and Space natively; claiming those would swallow the only
    // keyboard route to Approve (seen on an installed build). Every OTHER key still
    // reaches the table, which is what lets `h`/`j`/`k`/`l` carry on from a focused button.
    // On a card's own answer buttons (Approve, Deny, Always allow) Space is
    // claimed and does nothing -- not the button, not the leader. Claude Code's permission prompt
    // confirms with Enter; HTML's Space-activates is the default this overrides, only where a
    // button answers a permission.
    // Enter on those three is claimed too, and never activates them natively: this
    // handler presses the button itself, `TYPING_GUARD_MS` later and only after a landing on it (the
    // block below). The route a keyboard user relies on stays open -- `l`/`h`/Tab/HINT onto
    // Approve, then Enter -- only deferred, as `a`/`d` are.
    // Every other button (Stop, Dismiss, a tab, the chooser's rows) keeps both natively. A Space
    // that arrives inside a pending leader sequence is the sequence's, so it falls
    // through to the engine below -- claimed here first either way, so it never reaches the button.
    // (`onAnswerButton` is computed at the top, ahead of the two modal overlays' early returns.)
    // A pending prefix owns the next key, so Enter/Space on a focused control never activates
    // it; a pending count refuses a card's own buttons only.
    if (isActivatableControl(event.target) && (event.key === "Enter" || event.key === " ") && (pending !== null || (count !== null && onAnswerButton))) {
      event.preventDefault();
      if (pending === null) showFlash(COUNT_ANSWER_FLASH);
      return;
    }
    if (onAnswerButton && event.key === " ") {
      event.preventDefault();
      if (seqRef.current === null) return;
    }
    // `:ls⏎` once approved `rm -rf important` (2026-09-29): a card's own button
    // decides only the way `a`/`d` do. Enter is claimed first, so no path below leaves the native
    // activation in place, and the button is pressed through `waitThenAnswer` -- `TYPING_GUARD_MS`
    // later, any key in between cancelling it -- only with all three of: the typing guard's before-half
    // (`mayAnswerNow`, whose walk exception keeps a quick `l⏎`); no modifier at all (Shift included,
    // which `isPlainAnswerKey` lets through for `D`); and a landing, focus put on this very button by
    // the key right before this one (`placedRef`: an `h`/`l` that moved it, a Tab, or a HINT with no
    // key since). So `l`, `s`, Enter and `l`, `g`, `h`, Enter (a cancelled pair moves nothing) answer
    // nothing however slowly they are typed. And only on the card `a`/`d` would
    // answer from the cursor (`permissionTarget`) -- a Tab walks on from one card's buttons into
    // the next card's, and `l`/`h` then walk that card's (`currentStop` follows focus), while the row
    // cursor stays put, so a landing alone can sit on a card the cursor is not on.
    if (onAnswerButton && event.key === "Enter") {
      event.preventDefault();
      const button = event.target as HTMLButtonElement;
      const placed = placedRef.current;
      if (!typingGuard.mayAnswerNow("Enter", typedAt, event.repeat)) {
        showFlash(TYPING_FLASH);
        return;
      }
      if (event.shiftKey || !isPlainAnswerKey(event.nativeEvent as unknown as KeyLike)) {
        showFlash(MODIFIED_ENTER_FLASH);
        return;
      }
      if (placed === null || placed.el !== button || placed.atKey !== typingGuard.keyCount() - 1) {
        showFlash(ENTER_LANDING_FLASH);
        return;
      }
      const buttonRow = root === null ? null : rowOf(root, button);
      const buttonRowIndex = root === null || buttonRow === null ? null : rowIndexOf(root, buttonRow);
      if (buttonRowIndex === null || buttonRowIndex !== permissionTarget(answerableItems, cursor)) {
        showFlash(ENTER_ELSEWHERE_FLASH);
        return;
      }
      // The same card, in the same tab, still takes it at fire time -- as `answerCardByKey` checks.
      const tab = activeTabRef.current;
      waitThenAnswer(
        event,
        typedAt,
        () => activeTabRef.current === tab && button.isConnected && !button.disabled && document.activeElement === button,
        () => button.click(),
        "Enter",
      );
      return;
    }
    // A GUI pass (2026-09-27): the activity line's Stop keeps Enter and Space,
    // but only on a key that stands alone or ends a quick motion (`j` onto it, then Enter). `j` from
    // the last row lands here, so "just do it" typed after an arrival interrupted the turn with its
    // first Space, denying the waiting card with it -- the thing the typing guard exists to stop.
    const onStop = isActivatableControl(event.target) && (event.target as HTMLElement).matches(".activity-line .stop");
    if (
      onStop &&
      seqRef.current === null &&
      (event.key === "Enter" || event.key === " ") &&
      !typingGuard.mayActAfterMotion(typedAt, event.repeat)
    ) {
      event.preventDefault();
      showFlash(STOP_TYPING_FLASH);
      return;
    }
    // Any key within `TYPING_GUARD_MS` of a BROWSE `y` is typing -- vim's
    // `y i w` copied the row, opened INPUT on the `i` and typed the `w`. The copy is frozen and already
    // happened; what follows it inside the burst is swallowed and says how to type. A bare modifier is
    // not a key (the Shift of a capital), and a key an input method composes is not this panel's.
    // This sits AHEAD of the focused-control early return just
    // below, so `y` then Enter (or Space) on a focused button -- another tab's, Dismiss -- within the
    // burst is claimed too instead of activating it; a lone Enter after a pause is left to the button.
    if (
      mode === "browse" &&
      !isModifierKey(event.key) &&
      !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode }) &&
      typingGuard.afterCopy(typedAt)
    ) {
      event.preventDefault();
      // The wait this very key cancelled already said what did not happen, in its own words (and ends
      // with this hint): that message stays.
      if (cancelled === null) showFlash(TYPE_HINT_FLASH);
      return;
    }
    if (isActivatableControl(event.target) && (event.key === "Enter" || (event.key === " " && !onAnswerButton))) return;
    // The leader engine (`./leader`): a pending sequence owns
    // the next key, and only BROWSE ever starts one. An IME key is never a sequence key (the IME
    // owns it) -- checked here rather than folded into `startSequence`/`advanceSequence`
    // themselves, since neither of those takes an event to read `isComposing` off. A `{kind:"none"}`
    // start falls through to `resolveKey` below, never the other way; `startSequence` itself leaves
    // `g`/`z`/`[`/`]` to `resolveKey`'s own reserved two-key prefixes (`./leader`'s own doc comment).
    if (mode === "browse" && !isImeKey({ isComposing: event.nativeEvent.isComposing, keyCode: event.keyCode })) {
      if (seqRef.current !== null) {
        // A bare modifier's own keydown (the Control of a
        // Ctrl+c combo) is not "the next key" -- it must not cancel a pending sequence, the same way
        // `pendingRef`'s own `g`/`z`/`[`/`]` prefix is guarded just below and `TypingGuard.onKey`
        // ignores it. Without this, the bare `Control` cancelled the sequence and the `c` that
        // followed fell through to `resolveKey`'s Ctrl+c interrupt arm instead of being swallowed.
        if (isModifierKey(event.key)) return;
        event.preventDefault();
        // `advanceSequence` compares `event.key`
        // alone, so a real Ctrl/Alt chord (Ctrl+d after the bare Control above left the sequence
        // armed) reached it as plain "d" and could complete a binding no vim mapping would ever match
        // -- `<leader>bd` for Ctrl+d, not the bare `d` the table actually names. `startSequence`
        // already refuses to START a sequence on a Ctrl/Alt chord (just below); a chord arriving as a
        // CONTINUATION gets the same refusal: an unbound key after the leader is swallowed (a
        // chord matches no table key, modifier included, so it is unbound here by definition).
        applySeqStep(
          event.ctrlKey || event.altKey ? { kind: "cancel" } : advanceSequence(panelTable, seqRef.current.typed, event.key),
        );
        return;
      }
      // A prefix's second key never starts a sequence -- `g` then Space is a cancelled `g`
      // (`resolveKey`), not the leader, and `g` then `L` is not `L`'s tab step.
      if (pending === null && !event.ctrlKey && !event.altKey) {
        const start = startSequence(panelTable, event.key, isActivatableControl(event.target));
        if (start.kind !== "none") {
          event.preventDefault();
          // The leader starts a sequence only on a key that stands
          // alone or ends a quick motion -- in the middle of typed prose it is swallowed and says
          // so ("set up my" ran `<leader>m`, "the boy" `<leader>bo` and answered it with its `y`).
          if (event.key === panelTable.leader && !typingGuard.mayActAfterMotion(typedAt, event.repeat)) {
            showFlash(leaderTypingFlash(panelTable.leaderLabel));
            return;
          }
          // Any OTHER single-key table binding that runs on this very first key
          // -- H/L's default tab.prev/tab.next, or an nvim-read binding on some other plain letter --
          // used to run at once ("Only the leader ... runs as before"), so "Looks good, now add
          // tests⏎" switched tabs on its own `L` and sent the rest into another session's composer.
          // `mayActAfterMotion` cannot help here (`L` is the first key -- there is never
          // a motion run behind it), so this defers the same way `a`/`d` do: it runs
          // `TYPING_GUARD_MS` later, only if nothing else was typed meanwhile and the panel is still
          // in BROWSE (typingGuard.ts's own doc comment).
          if (start.kind === "run") {
            const { binding } = start;
            const flash = tableKeyTypingFlash(event.key, binding.desc);
            const waiting = typingGuard.defer(
              typedAt,
              event.repeat,
              () => {
                if (modeRef.current === "browse") applySeqStep({ kind: "run", binding });
              },
              flash,
            );
            if (!waiting && !event.repeat) showFlash(flash);
            return;
          }
          applySeqStep(start);
          return;
        }
      }
    }
    const action = resolveKey(mode, event.nativeEvent as unknown as KeyLike, {
      sessionEnded,
      pending,
      count,
      turnRunning: turnInProgress || shellRunning,
      table: panelTable,
    });
    if (action === null) return;
    // INPUT's Ctrl+y. Handled here, ahead of the unconditional `preventDefault`
    // further down, because with no card waiting the key stays the box's.
    if (action.kind === "approve-oldest") {
      approveOldestByKey(event, typedAt, afterKill);
      return;
    }
    // A digit accumulates into the count the NEXT `j`/`k`/`[[`/`]]` repeats -- read back out of
    // `countRef` (as `count`, above) by that key, once it arrives. Handled first, and returns at
    // once, so a bare digit never falls into the scroll-announcing or row-motion code below it.
    // Capped by `accumulateMotionCount` on every digit, not only at the end, so a long
    // run of digits never accumulates past `MAX_MOTION_COUNT` even transiently.
    if (action.kind === "count") {
      event.preventDefault();
      countRef.current = accumulateMotionCount(count, action.digit);
      return;
    }
    // How many times THIS key repeats its move: the count just accumulated (cleared above, so this
    // is the last one) if any, else once, same as vim's own default count of 1.
    const times = Math.max(1, count ?? 1);
    // Every key below that can move the conversation says so first, on the list itself, so
    // `MessageList` knows the scroll that follows is the user's (`./follow.ts`, 2026-09-24): it no
    // longer takes an unexplained drop in `scrollTop` for the reader leaving the bottom, because
    // WebKitGTK once produced such drops by itself mid-reply. `k`, `Ctrl+u` and `gg` end following at
    // once, as they always did; `j`, `Ctrl+d` and `G` let the scroll decide (reaching the bottom
    // re-arms it); `h`/`l` can focus a control that scrolls itself into view, either way. `Ctrl+e`/
    // `Ctrl+y` follow the identical `Ctrl+d`/`Ctrl+u` rule: `Ctrl+y` stops
    // following outright, `Ctrl+e` only re-arms it if the scroll actually reaches the bottom. A page
    // (`Ctrl+f`/`PageDown`/`PageUp`) follows the same rule, and so do the arrow keys,
    // which are `move`.
    const messageList = root?.querySelector<HTMLElement>(".message-list") ?? null;
    if (
      action.kind === "move" ||
      action.kind === "half-page" ||
      action.kind === "scroll-line" ||
      action.kind === "page"
    ) {
      noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
    } else if (action.kind === "jump") {
      // A counted jump goes to row N, so its direction is where that row sits against the cursor;
      // a bare `gg`/`G` names its end, wherever the cursor is.
      const up = count === null ? action.to === "first" : jumpTarget(action.to, count, timeline.length) < cursor;
      noteUserScroll(messageList, up ? "up" : "down");
    } else if (action.kind === "control") {
      noteUserScroll(messageList, "unknown");
    }
    // A held `j`/`k` (`./jkHold`): from its first repeat the view glides at a steady speed and the
    // cursor follows the row under its line, instead of a jump per repeat. Only for the row cursor in
    // BROWSE with no count (a count keeps its immediate steps), and only where the glide has somewhere to
    // go and motion is allowed; otherwise the repeat is a press like any other, below.
    if (
      action.kind === "move" &&
      event.repeat &&
      count === null &&
      !edgeFocused &&
      mode === "browse" &&
      frozenSnapshotRef.current === null &&
      messageList !== null &&
      root !== null &&
      currentStop(root, cursor) === (conversationRows(root)[cursor] ?? null)
    ) {
      const heldKey = event.key;
      const direction = action.delta;
      const started = startHold({
        list: messageList,
        rows: () => conversationRows(root),
        cursor,
        direction,
        code: event.code || event.key,
        now: performance.now(),
        onCursor: (index) => {
          // The glide placed the view itself; the landing effect must not move it again. Only for a cursor
          // that really moves: a "keep" no `[cursor]` effect consumes would swallow the next reveal.
          if (index !== cursorRef.current) landingRef.current = "keep";
          setCursor(index);
        },
        onRelease: (at) => {
          // The guard's clock is the keydowns' own; the release is dated on it from the last repeat.
          const clock = holdClockRef.current;
          typingGuard.onRelease(heldKey, clock.typed + Math.max(0, at - clock.perf));
        },
      });
      if (started) {
        holdClockRef.current = { typed: typedAt, perf: performance.now() };
        holdDirectionRef.current = direction;
        markLandedCode(null);
        event.preventDefault();
        return;
      }
    }
    // A press that arrives while the last one is still easing starts from where that one was going,
    // never from a frame in between: every decision below reads the list's geometry.
    if (messageList !== null) settleListScroll(messageList);
    // A code block HINT landed on is "the item" for exactly the next `y`; any other key moves on.
    const landedCode = copyCodeRef.current;
    markLandedCode(null);
    // The row the cursor is on, found by structure (`conversationRows`), never as the first
    // `.row-current` or the `cursor`-th `[data-nav-stop="row"]` in the subtree: a model reply's own
    // HTML can carry either. Read only by the keys that need it.
    const cursorRow = () => (root === null ? null : (conversationRows(root)[cursor] ?? null));
    // `j`/`k` scroll the cursor row's own overflow box (a long tool result) before they move off it
    // -- the vim behaviour -- and failing that scroll the conversation through a
    // current row that is taller than what is left of it on screen (`stepWithinRow`, which holds the
    // order and `./jkScroll` the rules). A count spends itself on those steps first; whatever it has
    // left moves rows below. This is DOM measurement, which `resolveKey`'s pure table must never do,
    // so it happens here, after the key already means "move the cursor". Only while the row cursor has
    // the keys: from a banner's button, `j` is a move, not a scroll of a row that is not current.
    let spentInRow = 0;
    if (action.kind === "move" && !edgeFocused) {
      spentInRow = stepWithinRow(cursorRow(), action.delta, times, !event.repeat);
      if (spentInRow > 0) {
        event.preventDefault();
        // `j`/`k` are row motions. If a control inside the row had the keys (after `l` onto a card's
        // Approve), scrolling the row could carry that control off screen while it still answered
        // Enter -- found while the scrolling change was checked, and left open there. So a scroll
        // step hands the keys back to the row first, the same place `j`/`k` leave them after a move.
        if (root !== null && document.activeElement !== root && root.contains(document.activeElement)) {
          root.focus({ preventScroll: true });
        }
        if (spentInRow >= times) return;
      }
    }
    if (action.kind === "move" || action.kind === "control" || action.kind === "answer") {
      event.preventDefault();
      if (root === null) return;
      if (action.kind === "move") {
        // A count repeats the step `times` times, each from the row the PREVIOUS step landed on
        // -- not `times` cells in one leap, so a stop with no row (a banner, the status line) still
        // ends the walk exactly where a single `j`/`k` onto it would. The walk
        // is `countedStop`, which reads the tree once whatever `times` is (up to `MAX_MOTION_COUNT`)
        // and stops at a boundary, where a step makes no progress (`clampStep` clamps rather than
        // returning `null` -- the C1c comment below). Its own doc has the measurements.
        const landing = countedStop(root, cursor, action.delta, times - spentInRow);
        if (landing === null) return;
        const target = landing.stop;
        // `j` that cannot move -- `nextStop` clamps rather than returning `null` at
        // a boundary (`clampStep`'s own doc comment), so "the last stop" is the stop the keys are
        // already on (the last row, or Stop while a turn runs), not `target === null` (that case is
        // an EMPTY conversation, handled above and unrelated). A plain `j` there walks on into the
        // box, as `j` does on the empty tab, once `stepWithinRow` above has brought a long last row to
        // its end (`TypingGuard.walkIntoBox`); a count is a distance, not this step. A card under the
        // cursor or a waiting question keeps the keys where the reader's answer is. Anything else
        // flashes once per press: a fresh `j` there, or the first repeat that stops after moving. Never
        // again on the repeats after that. `k` at the FIRST stop stays silent, as vim's own `k` on the
        // first line does (`:h j`) -- only `j` names a route the reader might actually want next.
        if (action.delta === 1) {
          if (target === landing.from) {
            const onCard = timeline[cursor]?.kind === "permission";
            const cardsAtPress = state.pendingPermissions.length;
            const walks =
              count === null &&
              !onCard &&
              !waitingQuestion &&
              !sessionEnded &&
              typingGuard.walkIntoBox(event, typedAt, () => {
                // A session can end, or a question or a card arrive, inside the wait. An ended session's box takes no
                // keys, INPUT with a question waiting is its dock, and a new card is where the reader's answer is.
                const { status, questions, pendingPermissions } = stateRef.current;
                const ended = status.kind === "unavailable" || status.kind === "closed";
                const asked = questions.some((q) => q.state.kind !== "settled") || pendingPermissions.length > cardsAtPress;
                if (modeRef.current !== "browse" || ended || asked) return;
                setMode("input");
                setComposerCaret("kept");
                setComposerFocusRequest((n) => n + 1);
              });
            if (!walks && (!event.repeat || heldMoveRef.current)) showFlash(TYPE_HINT_FLASH);
            heldMoveRef.current = false;
          } else {
            heldMoveRef.current = true;
          }
        }
        const finalRow = landing.row;
        if (finalRow !== null) {
          if (finalRow !== cursor) {
            landingRef.current = action.delta;
            // Only a lone press eases the view to the new row: a held key's repeats and a count land at once.
            landingAnimateRef.current = !event.repeat && times === 1;
          }
          setCursor(finalRow);
          root.focus({ preventScroll: true });
        } else {
          controlsOf(target)[0]?.focus();
        }
      } else if (action.kind === "control") {
        const stop = currentStop(root, cursor);
        const target = stop === null ? null : nextControl(stop, action.delta);
        if (target === "stop") root.focus({ preventScroll: true });
        else if (target !== null) {
          // A landing, recorded only where this key actually moved focus onto the
          // control -- a clamped `l` on the last one moves nothing and lands nothing.
          const moved = document.activeElement !== target;
          target.focus();
          if (moved && document.activeElement === target) placedRef.current = { el: target, atKey: typingGuard.keyCount() };
        }
      } else {
        // `a`/`d` answer the card by its id (`answerCardByKey`), through the same
        // `answerPermission` its buttons use, so one guard against a second answer covers both.
        // Not from a banner's button: the row cursor is hollow there, and not what keys act on,
        // so there is no card for it (a flash says so). Only the cursor's card or the
        // card gating its tool call (`permissionTarget`) -- the old "the only card, from any row"
        // is gone, since a key must answer the card the reader is looking at.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        answerCardByKey(event, typedAt, target, action.decision);
      }
      return;
    }
    event.preventDefault();
    switch (action.kind) {
      case "mode":
        // `i`/`o`/`A` enter INPUT only on a key that stands alone
        // (`mayStartInput`: a pure pause, and -- unlike the leader's `mayActAfterMotion` -- no motion
        // exception: "look at" walked with `l` and opened INPUT with `o`). In the
        // middle of typed prose the `i` of "explain" or the `o` of "follow-up" is swallowed and says how
        // to type, instead of opening the composer halfway through the word. A lone `i` after a pause is
        // unchanged; a fast `ji` chord now needs a pause.
        if (action.to === "input" && !typingGuard.mayStartInput(typedAt, event.repeat)) {
          if (cancelled === null) showFlash(TYPE_HINT_FLASH);
          break;
        }
        // INPUT's Esc: an `i`/`o`/`A` right after it re-enters at once (vim's `Esc i`; `mayStartInput`).
        if (action.to === "browse") typingGuard.noteLeftInput();
        setMode(action.to);
        // Entering INPUT (`i`/`o`/`A`) asks `Composer` to place the caret itself, rather than
        // leaving it to whatever a bare `autoFocus` on a freshly re-mounted, non-empty box happens
        // to land on. Leaving BROWSE (`to === "browse"`) carries no caret to place.
        if (action.to === "input" && !waitingQuestion) {
          setComposerCaret(action.caret);
          setComposerFocusRequest((n) => n + 1);
        }
        break;
      case "esc-blocked":
        // `Esc` never interrupts -- this is the reminder, not the action.
        showFlash(ESC_DOES_NOT_INTERRUPT_FLASH);
        break;
      case "toggle-expand": {
        const item = timeline[cursor];
        if (waitingQuestion && item?.kind === "message" && item.question?.interactionId === waitingQuestion.interactionId) {
          setMode("input");
          questionDock.current?.focus();
          break;
        }
        const toolId = item?.kind === "tool" ? item.call.toolUseId : item?.kind === "permission" ? item.request.toolUseId : null;
        const task = toolId ? liveTasks(state.backgroundTasks.rows).find((t) => t.tool_use_id === toolId) : null;
        if (task && event.key === "Enter") { openTasksOverlay(false, task.task_id); break; }
        // Keyed on the timeline KEY, not the cursor index -- see the doc comment on `expanded`
        // above for why an index would silently drift onto the wrong row. For a "run" key this
        // un-collapses it right back into its calls (`display.ts`'s `buildDisplay`).
        const key = timeline[cursor]?.key;
        if (key !== undefined) setExpanded((prev) => ({ ...prev, [key]: !prev[key] }));
        break;
      }
      case "fold": {
        // `zo` opens this row's own fold: a tool's result, or a collapsed run's calls
        // (the same `expanded[key]` Enter flips). vim's `zc` closes the INNERMOST open fold: this row's
        // own result if it is open, else the run it was unfolded from -- `buildDisplay` emits an expanded
        // run's calls under their own `t-<seq>` keys, so the run is found by asking the same timeline
        // folded with nothing expanded (`runKeyOf`, null in the detailed view and for a row in no run,
        // where `zc` closes nothing). Both SET `expanded[...]` where Enter flips it, so each is
        // idempotent, as vim's are: `zo` on an open fold and `zc` on a closed one change nothing.
        const key = timeline[cursor]?.key;
        if (key === undefined) break;
        const run =
          action.open || expanded[key]
            ? null
            : runKeyOf(
                buildTimeline(state),
                { expanded, detailed, turnRunning: state.activeTurnId !== null, thinking: thinkingDisplay },
                key,
              );
        setExpanded((prev) => ({ ...prev, [run ?? key]: action.open }));
        break;
      }
      case "detailed":
        // `Ctrl+o`. A per-tab toggle, mirrored to Rust in `saveView`/restored on a switch --
        // see `detailed`'s own doc comment.
        setDetailed((d) => !d);
        break;
      case "table-scroll": {
        // `zh`/`zl` scroll the CURRENT row's own table, never the conversation.
        const table = cursorRow()?.querySelector<HTMLElement>(".table-scroll") ?? null;
        if (table !== null) table.scrollLeft += action.delta * TOOL_RESULT_SCROLL_STEP_PX;
        break;
      }
      case "caret":
        // `v` from BROWSE starts CARET. `enterRegion` itself decides whether there is
        // anywhere to start (a row under the cursor, not a banner/Stop) and flashes when there is
        // not. `landedCode` is the same ref-read the `"copy"` case just below uses --
        // see `enterRegion`'s own doc comment: `copyCodeRef.current` was already cleared above by
        // the time this ran.
        enterRegion(false, landedCode);
        break;
      case "visual":
        // `V` from BROWSE still starts V-LINE directly.
        enterRegion(action.line, landedCode);
        break;
      case "copy": {
        // A `y` opens a burst in which every other key is typing (above).
        typingGuard.noteCopy(typedAt);
        // After HINT landed on a code block: that block's code, not the whole message. Only while it
        // is still in the DOM and still inside the row under the cursor -- anything else and the
        // user is looking at something else now.
        if (landedCode !== null && landedCode.isConnected && cursorRow()?.contains(landedCode)) {
          copied(codeBlockText(landedCode), timeline[cursor]?.key);
          break;
        }
        // No item at the cursor (an empty timeline) writes NOTHING, rather than clobbering
        // whatever the user already had on the clipboard with "" -- found in review.
        const item = timeline[cursor];
        if (item === undefined) break;
        // A reply's row copies what is drawn, not the markdown it came from: the source can hold
        // comments, raw HTML the sanitizer removed, and text that never reaches the screen.
        const body = item.kind === "message" ? (cursorRow()?.querySelector<HTMLElement>(".row-body") ?? null) : null;
        copied(body !== null ? renderedText(body) : primaryText(item), item.key);
        break;
      }
      case "copy-output": {
        const item = timeline[cursor];
        const text = item === undefined ? null : outputText(item);
        if (text === null) showFlash("this row has no output");
        else copied(text, item.key);
        break;
      }
      case "deny-reason": {
        if (root === null) break;
        // The same card `a`/`d` would answer, and the same wait.
        const target = edgeFocused ? null : permissionTarget(answerableItems, cursor);
        // Another agent's card has no reason box: its deny carries no reason, so `D` says what happens
        // instead of waiting for a box that is not there.
        const targetItem = target === null ? undefined : timeline[target];
        if (targetItem?.kind === "permission" && targetItem.request.editorCard !== undefined) {
          showFlash("an editor card takes no reason with a deny; d denies");
          break;
        }
        if (targetItem?.kind === "permission" && isExternalTool(targetItem.request.toolName)) {
          showFlash(`${agentTitle(activeTabInfo(tabsRef.current)?.agent)} takes no reason with a deny; d denies`);
          break;
        }
        // The card's row by structure: `conversationRows(root)[target]` is timeline item
        // `target`, which a card row holds no model HTML inside.
        const box =
          target === null
            ? null
            : (conversationRows(root)[target]?.querySelector<HTMLInputElement>(".permission-card input") ?? null);
        waitThenAnswer(event, typedAt, box === null ? null : () => box.isConnected, () => {
          if (box === null) return;
          // Re-read where the card is now: rows may have arrived above it during the wait.
          const row = rowOf(root, box);
          const index = row === null ? null : rowIndexOf(root, row);
          if (index !== null && index !== cursorRef.current) setCursor(index);
          landedControlRef.current = box;
          box.focus();
        });
        break;
      }
      case "restart":
        // `r` on an ended tab resets it to `NotStarted` IN PLACE: the tab keeps its
        // number and its rename, and the `tabs` envelope that follows is what actually drops this
        // render back to the empty tab -- there is no local start-screen reset any more.
        resetTab();
        break;
      case "review": {
        // Turn review. A tab with no session has nothing to review (the start screen never gets here, its
        // keys are the empty tab's own), and a request always names its tab.
        const tab = activeTabRef.current;
        if (tab !== null && sessionStartedRef.current) openReviewOverlay(tab);
        break;
      }
      case "keymap":
        // Only ever reached with the overlay closed -- the `keymapOpen` branch above returns
        // before `resolveKey` runs at all once it is open, `?`/`Escape`/`q` included, so this is
        // opening only, never a toggle-closed here.
        setKeymapOpen(true);
        break;
      case "hint": {
        // `f` is always the first key of whatever typed it ("fix the dashboard
        // layout⏎" started a HINT on its own `f`), so `mayActAfterMotion`'s backward-only check
        // cannot guard it -- this defers the same way `a`/`d` do (typingGuard.ts's own doc comment):
        // it asks `shell` for a HINT `TYPING_GUARD_MS` later, only if nothing else was typed
        // meanwhile and the panel is still in BROWSE. `shell` owns the HINT session once it starts:
        // it collects targets across the whole window and asks this panel for its own with
        // `hint_collect`. Until the HINT ends, this panel's keys are swallowed (`hintPendingRef`);
        // nothing else changes here.
        const waiting = typingGuard.defer(
          typedAt,
          event.repeat,
          () => {
            if (modeRef.current === "browse") requestHint(false);
          },
          hintTypingFlash(),
        );
        if (!waiting && !event.repeat) showFlash(hintTypingFlash());
        break;
      }
      case "pending":
        // Claimed (so the key does nothing else) and remembered for exactly one more key.
        pendingRef.current = action.prefix;
        // A count survives its prefix (`3gg`, `2gt`), so the second key reads it -- it was read
        // and reset at the top of this function, like the prefix itself.
        countRef.current = count;
        // The box shows what any of the five -- `g`/`z`/`[`/`]`/`Ctrl+w` -- can start once it has waited
        // `WHICH_KEY_DELAY_MS` with nothing completing it (replacing the strip's former `g`-only
        // 400ms line).
        // `clearSequence` at the top of this function on every later key -- the second `g` of `gg`
        // included -- is what cancels it before it fires.
        scheduleBoxTimer();
        break;
      case "panel": {
        // The second key of a reserved two-key prefix (`[b`) that also completes a panel-table
        // sequence (`resolveKey`'s own `ctx.pending` branch) -- everything else a table
        // sequence can run reaches here through the leader engine above instead.
        // A count before a `g` tab pair is vim's `{N}gt`/`{N}gT` (`:help gt`); every other pair
        // ignores it. `select_tab` names the tab's id, which is not the number the bar shows.
        const tabAction = action.binding.keys[0] === "g" ? action.binding.action : null;
        if (count !== null && (tabAction === "tab.next" || tabAction === "tab.prev")) {
          const target = tabs === null ? null : countedTabTarget(tabs, tabAction, count);
          if (target === null) showFlash(`no tab ${count}`);
          else postToRust({ type: "select_tab", request_id: nextRequestId(), tab: target });
          break;
        }
        runPanelAction(action.binding);
        break;
      }
      case "cancel":
        // Swallowed (`preventDefault` above), nothing runs. A key that merely ended a prefix
        // says nothing, as vim's `clearopbeep` only beeps; a count before a card answer says why.
        if (action.why === "count-on-answer") showFlash(COUNT_ANSWER_FLASH);
        break;
      case "pane":
        // `Ctrl+w h/j/k/l`, the keys to the module that way. shell runs the move
        // `Ctrl+h/j/k/l` make from this panel (`pane_nav` -> `move_focus`); the panel changes nothing
        // itself, and a side with no module leaves the keys where they are. The prefix was spent at the
        // top of this handler, and a count typed before it means nothing to a move (a table pair
        // ignores it).
        postToRust({ type: "pane_nav", request_id: nextRequestId(), direction: action.direction });
        break;
      case "half-page": {
        // Half the visible height, as in vim. Then, if the cursor's row is not on screen, the
        // cursor comes to the visible row NEAREST to it -- `clampCursorToView` (vim's scrolloff
        // rule, shared with the scroll-triggered rehome effect above and
        // with `scroll-line` just below). The view stays exactly where the scroll put it ("keep"):
        // re-revealing that row with `nearest` would pull the view back by up to a row.
        //
        // A re-home also takes DOM focus back to the root, as `move` and `jump` do. Found in
        // review: without it, a control that had the keys (Approve after `l`, a banner's Dismiss,
        // Stop) KEPT them while the cursor was drawn on another row, so `Enter` activated an
        // Approve scrolled out of sight and `j`/`k` steered from the old stop. `clampCursorToView`
        // returns `null` both when the row is still (partly) visible and when nothing on screen at
        // all -- the two cases the original inline version also left focus untouched for, so moving
        // to this shared helper does not change either.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        list.scrollTop += action.delta * Math.max(1, Math.floor(list.clientHeight / 2));
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "scroll-line": {
        // vim's own `:help CTRL-E`/`:help CTRL-Y`: one text line, not half a view -- and, unlike
        // `half-page` just above, counted (`times`, with the count's own cap). Box-first, the way `j`/`k` are
        // (`stepWithinRow`): a long tool result's own capped view takes as many of the count's
        // units as it can make progress on -- using ITS OWN step, `TOOL_RESULT_SCROLL_STEP_PX`, not
        // this action's one-line step ("the way j/k do it" is reusing the box's own scroller, not
        // inventing a line-based one for it) -- before the rest fall through to the conversation
        // itself, one line each. Re-homes the cursor through the identical rule `half-page` reuses
        // above.
        //
        // The box takes a unit only while it is at least partly on
        // screen (`scrollVisibleRowBox`), never through `j`/`k`'s bring-the-row-back step.
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list === null || root === null) break;
        const row = cursorRow();
        let remaining = times;
        while (remaining > 0 && scrollVisibleRowBox(row, action.delta)) remaining--;
        if (remaining > 0) {
          list.scrollTop += action.delta * computedLineHeight(row !== null ? rowTextElement(row) : list) * remaining;
        }
        // `noteUserScroll` above only ARMS
        // `MessageList`'s steering window -- the actual re-arm needs a real `scroll` event, which a
        // press absorbed entirely by the row's own box (the `while` loop above) never produces on
        // THIS list (`.tool-result-body` is a different scrollable element, and `scroll` does not
        // bubble). At the true bottom already -- exactly what a box-only `Ctrl+y` then `Ctrl+e`
        // leaves behind -- following stayed off for good even though the box round-tripped back to
        // where it started. A synthetic `scroll` event costs nothing when a real one is coming too
        // (`MessageList`'s `onScroll` only reads the list's CURRENT position) and lets `Ctrl+e`
        // re-arm by the same "did this reach the bottom" rule `Ctrl+d`/`G` already use, box-absorbed
        // or not. `Ctrl+y` (`action.delta < 0`) must not do this: it stops following outright, before
        // its own scroll, by design (the comment above the `noteUserScroll` call), and re-arming it
        // here would undo that the moment a `Ctrl+y` happened to leave the list at the bottom too.
        if (action.delta > 0) list.dispatchEvent(new Event("scroll"));
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "page": {
        // vim's `:help CTRL-F`: a whole view, two text lines of the old
        // one kept on screen, `times` of them (with the count's own cap). The two lines are the reader's:
        // the cursor row's own text (`rowTextElement`), never `.message-list` itself, which sets no
        // line height in `index.css` and so reads `normal` -- under the prose line, which is what
        // `scroll-line` above records. Like `half-page`, the list alone scrolls (a long tool result's
        // own box is not this key's), and a cursor whose row the page carried out of sight goes to the
        // nearest visible row, the view staying where the page put it ("keep"), the keys handed back
        // to the row so a control that had them cannot answer Enter for a row nobody sees.
        const list = messageList;
        if (list === null || root === null) break;
        const row = cursorRow();
        const line = computedLineHeight(row !== null ? rowTextElement(row) : list);
        list.scrollTop += action.delta * times * Math.max(1, list.clientHeight - 2 * line);
        const next = clampCursorToView(list, conversationRows(list), cursor);
        if (next !== null) {
          landingRef.current = "keep";
          setCursor(next);
          root.focus({ preventScroll: true });
        }
        break;
      }
      case "jump": {
        // `G` goes to the very END of the list, not merely to the last row's top: on a long last
        // reply, that is the line you want. `gg` goes to the very top.
        // With a count they go to row N instead (`{N}G`, `{N}gg`; `jumpTarget` clamps
        // it). That row is brought on screen the way a `j`/`k` landing brings one -- through the
        // cursor effect, which shows a tall row's near edge -- so the list is not scrolled to either
        // end. The landing is set only for a cursor that really moves: an unchanged cursor runs no
        // effect, and a value left behind would misplace some later, unrelated move.
        const target = jumpTarget(action.to, count, timeline.length);
        if (target !== cursor) landingRef.current = count === null ? "keep" : target > cursor ? 1 : -1;
        setCursor(target);
        const list = root?.querySelector<HTMLElement>(".message-list") ?? null;
        if (list !== null && count === null) list.scrollTop = action.to === "first" ? 0 : list.scrollHeight;
        // A bare `G` asks for the end outright: following resumes even where the view is there already and the
        // write moves nothing (no scroll event would say so), as after a zoom left a reader at the end.
        if (list !== null && count === null && action.to === "last") resumeFollowing(list);
        root?.focus({ preventScroll: true });
        break;
      }
      case "scroll-row": {
        // vim's `:help zt`/`zz`/`zb`: the ROW goes to an edge of the view -- its top,
        // its middle or its bottom, whatever its height -- never a text line. With a count it is row N
        // (the row `{N}G` goes to: `jumpTarget`, 1-based, clamped to the rows there are) and the cursor
        // goes there too. The scroll is this key's own, so it is said on the list first, like every
        // other (`noteUserScroll`): a view that moves up stops following at once. `"keep"` keeps the
        // reveal effect from re-aligning the row this has just placed, and `scrollTop` clamps at the
        // list's ends as a browser's does -- the last row's top cannot reach the top of a short list.
        const list = messageList;
        if (list === null || root === null) break;
        const target = count === null ? cursor : jumpTarget("first", count, timeline.length);
        const row = conversationRows(list)[target];
        if (row === undefined) break;
        const l = list.getBoundingClientRect();
        const r = row.getBoundingClientRect();
        const offset =
          action.where === "top"
            ? r.top - l.top
            : action.where === "bottom"
              ? r.bottom - l.bottom
              : (r.top + r.bottom - l.top - l.bottom) / 2;
        noteUserScroll(list, offset < 0 ? "up" : "down");
        list.scrollTop += offset;
        if (target !== cursor) {
          landingRef.current = "keep";
          setCursor(target);
        }
        root.focus({ preventScroll: true });
        break;
      }
      case "prompt-jump": {
        // `[[`/`]]`, repeated `times` times, each from the prompt the PREVIOUS step landed on --
        // stopping early (rather than wrapping) the moment there is no earlier/later prompt at all.
        let target: number | null = cursor;
        for (let n = 0; n < times && target !== null; n++) target = promptIndex(timeline, target, action.delta) ?? null;
        if (target !== null && target !== cursor) {
          noteUserScroll(messageList, action.delta > 0 ? "down" : "up");
          landingRef.current = action.delta;
          setCursor(target);
        }
        root?.focus({ preventScroll: true });
        break;
      }
      case "card-jump": {
        // `]p`/`[p` move the CURSOR to the next/previous card waiting for an answer,
        // wrapping as nvim's `]d` does, `times` cards on -- and do nothing else. No `answerPermission`, no
        // `waitThenAnswer`, no button pressed: a lone `a`/`d` afterwards answers the card this landed on
        // (`permissionTarget`) and an `a` typed hard on the heels of the jump is refused like any other.
        // A card waits when this panel has not answered it and its session lives -- a dead session's
        // cards stay drawn but inert, and `a` on one does nothing -- the test `answerCardByKey`'s
        // `stillThere` makes at fire time, made here so the cursor never lands on a card that cannot answer.
        const target = sessionEnded ? null : waitingCardAfter(timeline, cursor, action.delta, times, answeredRef.current);
        if (target === null) {
          showFlash(NO_WAITING_CARD_FLASH);
          break;
        }
        if (target !== cursor) {
          // The way the cursor really travels, not the key's: a wrap from the last card to the first goes UP
          // the list, and only a scroll up stops the list following. `landingRef` is the key's own -- which
          // edge of a row taller than the view to show (`revealRow`), the top after `]p`, the bottom after `[p`.
          noteUserScroll(messageList, target < cursor ? "up" : "down");
          landingRef.current = action.delta;
          setCursor(target);
        }
        // Even with the cursor staying (the one card waiting is the one it is on): the keys come back to the
        // row from a focused control -- Approve after `l`, Stop -- as every other cursor move does, so the
        // next `a` acts on the card the cursor shows and Enter cannot press a button it has left.
        root?.focus({ preventScroll: true });
        break;
      }
      case "interrupt":
        // `Ctrl+c` while a turn runs, from anywhere in BROWSE -- the same `interrupt()`
        // the activity line's own Stop button calls.
        interrupt();
        break;
      case "search":
        // The `/` prompt and the `:` line are one command line, as vim's are, so
        // opening either closes the other. Two boxes drawn, one without the keys, was a state a click
        // back on the conversation could reach, and `takeKeys` then focused the `/` one, newer or not.
        setExLine(null);
        setSearch({ query: "", origin: cursor });
        setLineFocusRequest((n) => n + 1);
        break;
      case "ex-line":
        // The box takes the keys (`SearchBar` focuses itself), runs nothing. A line already open -- the keys
        // elsewhere, since a key typed in it is its text -- is wiped and handed them again, as vim's `:` gives a
        // fresh command line that has them.
        openExLine();
        break;
      case "search-next": {
        const found = findMatch(timeline, lastSearchRef.current, cursor, action.delta, false);
        if (found === null) {
          if (lastSearchRef.current !== "") showFlash(`pattern not found: ${lastSearchRef.current}`);
        } else moveCursorTo(found);
        // Same reason `move`/`jump`/`prompt-jump` and the Ctrl+d/Ctrl+u re-home all do this: a
        // cursor move must take the keys back from a focused control (e.g. a permission card's
        // Approve after `l`), or a stale `document.activeElement` keeps answering Enter/Space for a
        // row the cursor no longer shows (found in review).
        root?.focus({ preventScroll: true });
        break;
      }
      case "open-path": {
        // A tool's own path field, or a path found in prose -- never a guess at what
        // the model meant, only what `pathsIn` (`../paths.ts`) can actually read off the row.
        const item = timeline[cursor];
        const paths = item === undefined ? [] : pathsIn(item);
        if (paths.length === 0) showFlash("no path on this row");
        else if (paths.length === 1) openPath(paths[0]);
        else {
          const row = cursorRow();
          beginPathPick(paths, row === null ? [] : [row]);
        }
        break;
      }
      case "open-link": {
        // The web link(s) of the row under the cursor, read off the DOM as the
        // reader sees them (`webLinks`: http(s) only, normalized, once per address). One whose visible text
        // is its own address, on screen, opens at once; several, a titled one, or one nobody can see wait for
        // a letter with each full address shown -- a link in a model's reply is never opened by a key alone
        // without the reader having seen where it goes. A count is ignored, as `gf`'s is.
        const row = cursorRow();
        const links = row === null ? [] : webLinks(row).slice(0, HINT_ALPHABET.length);
        if (links.length === 0) showFlash("no web link on this row");
        else if (links.length === 1 && root !== null && linkOpensAtOnce(links[0], root)) openUrl(links[0].url);
        else setLinkPick(links.map((link) => link.url));
        break;
      }
      case "view-in-editor": {
        // The row's whole text, uncut, in a read-only nvim scratch buffer. A pending edit card's row
        // is its proposed change, so it is shown as that: the file on disk beside the file as the card
        // would leave it, both read-only.
        const item = timeline[cursor];
        if (item === undefined) break;
        const requestId = nextRequestId();
        trackRequest(requestId, { kind: "editor", tab: activeTabRef.current ?? 0 });
        const stored = item.kind === "permission" ? item.request.editorCard?.proposal : undefined;
        const tab = activeTabRef.current;
        if (item.kind === "permission" && stored !== undefined && tab !== null) {
          // A proposal an editor tool computed: the shell holds both sides of each file, and one file shows per press.
          const { files } = stored;
          const shown = (cardViews.current.get(item.request.permissionId)?.shown ?? 0) % files.length;
          cardViews.current.set(item.request.permissionId, { tab, shown: shown + 1 });
          if (files.length > 1) {
            const path = revealedPathText(files[shown]);
            showFlash(`${shown + 1} of ${files.length}: ${path}`, { path });
          }
          postToRust({ type: "editor_card_view", request_id: requestId, tab, proposal: stored.id, file: shown });
          break;
        }
        const proposal = proposalOf(item, answeredRef.current);
        if (proposal !== null) {
          postToRust({ type: "view_proposal", request_id: requestId, path: proposal.path, change: proposal.change });
        } else {
          const { title, text } = viewText(item);
          postToRust({ type: "view_in_editor", request_id: requestId, title, text });
        }
        break;
      }
    }
  };

  /* A session that died is announced here, not left to be inferred from a status word in the
     header. `unavailable` specifically means this client stopped being able to observe the session
     -- the transcript above it can be missing its tail, or a piece out of its middle -- so the
     reason text (which says exactly what was lost) is rendered in full and cannot be dismissed.
     A hidden warning about incomplete output is the same thing as no warning. */
  /* An error row is a classifier site beside the failed tab's own: a
     first-turn failure the sidecar reports (the not-logged-in row, whose real shape was never
     captured) ends a live session here rather than failing the tab, so the reason gets its
     headline and remedy above the raw text too. `null` for any unrecognised reason: exactly what
     this showed before. */
  const sessionEndedProblem =
    state.status.kind === "unavailable" || state.status.kind === "closed"
      ? classify(state.status.reason, hello?.account ?? null)
      : null;
  // What `r` does on this row: Rust continues a session its agent stopped with the conversation whole, and
  // empties the tab to a start screen otherwise (`TabView::resumes_on_r`).
  const restartHint = activeTab?.resumesOnR
    ? "Press r to resume this conversation."
    : "Press r to start a new session here.";
  const sessionEndedBanner = (
    <SessionEndedBanner
      state={state}
      restartHint={restartHint}
      sessionEndedProblem={sessionEndedProblem}
    />
  );

  /** What the which-key box draws, or `null` while nothing is pending -- gated by `boxShown` in
   *  the JSX below, so a stale `pendingRef.current` left over after its own timer already fired is
   *  never rendered on its own: every place that abandons a fixed prefix without completing it
   *  (`onKeyDown`'s own top, ahead of the leader engine) resets `boxShown` in the same tick. A
   *  leader/table sequence (`seq`) and a reserved two-key prefix (`pendingRef`) can never both be
   *  set at once -- starting either clears the other -- so this checks `seq` first with no priority
   *  to decide between them. */
  const box: { title: string; entries: BoxEntry[] } | null =
    seq !== null
      ? { title: sequenceTitle(panelTable, seq.typed), entries: boxEntries(panelTable, seq.typed, modeFixed) }
      : pendingRef.current !== null
        ? {
            // `Ctrl+w`'s own spelling, not tmux's `"C-w"` (`PendingPrefix`'s): the other four are one character.
            title: pendingRef.current === "C-w" ? "Ctrl+w" : pendingRef.current,
            entries: [...FIXED_PENDING_ENTRIES[pendingRef.current], ...boxEntries(panelTable, [pendingRef.current], modeFixed)],
          }
        : null;

  return (
    <ProjectDirContext.Provider value={hello?.projectDir ?? ""}>
    <div
      className="agent-ui-root agent-ui-conversation"
      ref={containerRef}
      // Lets the stylesheet show key hints only where those keys would act (BROWSE, not INPUT).
      data-panel-mode={mode}
      data-pane-focused={paneFocused ? "true" : "false"}
      // Real, focusable, so BROWSE's keydown handler has a DOM node to bubble from -- see the
      // effect above `onKeyDown` that keeps focus here whenever `mode` is "browse".
      tabIndex={0}
      // A y/n prompt takes every key first, from wherever it was typed (the chooser and its inputs,
      // the tab bar's rename, the `/` prompt, the composer): see `answerConfirm`'s own doc. VISUAL's
      // own whole key table runs right after it, in the same capture-phase handler, ahead of
      // `onKeyDown` and of every descendant's own keydown -- a card's reason box denies from its own
      // handler before `onKeyDown` ever runs, so VISUAL has to beat it here.
      onKeyDownCapture={(event) => {
        if (answerConfirm(event)) return;
        if (tasksOpen) {
          onTasksKey(event);
          return;
        }

        const target = event.target instanceof Element ? event.target : null;
        const questionCovered = overlayOpen || keymapOpen || detail !== null || handoffOpen || pathPick !== null || linkPick !== null;
        if (waitingQuestion && target?.closest(".question-dock") && questionCovered) {
          onKeyDown(event);
          event.stopPropagation();
          return;
        }
        if (waitingQuestion && !questionCovered && mode === "input" && !target?.closest(".permission-card, .question-dock")) {
          questionDock.current?.key(event);
          if (event.isPropagationStopped()) return;
        }
        onVisualKeyDownCapture(event);
      }}
      onKeyDown={onKeyDown}
      // A pointer press anywhere in the panel is the user's explicit act and ends VISUAL
      // before the press's own handling runs (a click on Approve still approves; a click into a
      // card's reason box shows BROWSE at once, and the Enter that follows it then denies as the box
      // says, which is accepted). Capture phase, ahead of every descendant's
      // own `onClick`/`onMouseDown`.
      onPointerDownCapture={() => {
        exitRegion();
        arrivalParkRef.current = null; // A press in the panel is a place chosen by hand
      }}
      // Every link in the panel is a reply's (the page writes none of its own), and a click on one is the
      // browser's own navigation, which Rust launches with no look at where it goes. A link whose text is
      // not its own address shows that address first; a second click on it opens it. Capture phase, so
      // the default is gone before anything below sees the click; the click itself still propagates.
      onClickCapture={(event) => {
        const anchor = event.target instanceof Element ? event.target.closest<HTMLAnchorElement>("a[href]") : null;
        const root = containerRef.current;
        if (anchor === null || root === null) return;
        const now = Date.now();
        // Armed, it is checked again now: the flash that showed the address must still be the one up, and its host
        // whole on screen at this moment. If not, this click opens nothing and shows the address again.
        const held = heldLinkRef.current;
        const message = bandMessageRef.current;
        if (
          shownLinkRef.current !== null &&
          (held === null || flashRef.current?.seq !== held.seq || message === null || !lookOnScreen(message, held.look))
        ) {
          shownLinkRef.current = null;
        }
        const url = linkClickNeedsLook(anchor, root, shownLinkRef.current, now);
        if (url === null) {
          shownLinkRef.current = null;
          heldLinkRef.current = null;
          return;
        }
        event.preventDefault();
        // Not armed yet: the band shows the address whole (never cut: a long host's own domain is at its end) and
        // arms the second click only while it measures the host on screen (`onLookMeasured`).
        const text = `${url} — click again to open`;
        const look = linkLook(url);
        shownLinkRef.current = null;
        const seq = showFlash(text, { look });
        heldLinkRef.current = { link: { el: anchor, row: rowOf(root, anchor), url }, text, seq, look };
      }}
      onFocus={(event) => {
        // The focus move a plain Tab just made is a landing, at that Tab's count --
        // consumed by the first focus event after it, so a focus nothing typed (an effect, a later
        // envelope) is never one. And only while that Tab's default is still
        // alive, read now that every handler has run -- a Tab one of them claimed moved nothing.
        const lastTab = tabAtKeyRef.current;
        if (lastTab !== null && lastTab.atKey === typingGuard.keyCount() && !lastTab.event.defaultPrevented) {
          placedRef.current = { el: event.target as HTMLElement, atKey: lastTab.atKey };
        }
        tabAtKeyRef.current = null;
        // The outermost stop (`stopOf`): a link inside a reply is in the reply's row, whatever the
        // reply's own HTML claims to be.
        const stop = containerRef.current === null ? null : stopOf(containerRef.current, event.target as HTMLElement);
        setEdgeFocused(stop !== null && stop.getAttribute("data-nav-stop") !== "row");
        // VISUAL ends, before anything else happens, when focus lands inside the panel on
        // anything but its own root -- a click into a card's reason box, a button, the composer, the
        // tab bar.
        if (containerRef.current !== null && event.target !== containerRef.current) exitRegion();
      }}
    >
      {tabs !== null && showTabBar(tabs.tabs.length, renaming !== null) && (
        <TabBar
          tabs={tabs.tabs}
          active={tabs.active}
          renaming={renaming}
          focusRequest={tabBarFocusRequest}
          onSelect={(tab) => postToRust({ type: "select_tab", request_id: nextRequestId(), tab })}
          onRenameCommit={commitRename}
          onRenameCancel={cancelRename}
        />
      )}
      {errorBanner}
      {/* `sessionEnded` makes every pending card inert. The cards themselves are NOT removed: a
          permission that was still open when the session died is real history, and deleting it
          would read as a resolution nobody made. */}
      {/* The list, the `?` overlay and the detail popover share one positioned region, so either
          overlay covers the conversation and nothing else -- the activity line, the status row and
          the footer are outside its box rather than lifted back above it (see
          `.agent-ui-scroller` in index.css). The tab bar goes above `errorBanner`. */}
      <div className="agent-ui-scroller">
        {/* While VISUAL/V-LINE is on, `MessageList` reads the props frozen at entry --
            everything but `cursor`/`focused`/`visual` itself -- so the conversation it is built
            from does not change under a selection built directly on its DOM. `frozenTimeline` is
            the SAME pure computation over those frozen props, used only to translate a DOM row back
            into a live timeline key (`rowKeyOfCaret`/`landCursorOnRowKey` above); the `cursor` INDEX
            handed to `MessageList` here is the live cursor re-expressed against the FROZEN
            timeline's own indices, since that is the timeline `MessageList` itself will recompute
            internally from these same frozen props. A live row the frozen list does not have (a
            prompt sent at a turn's end moved the cursor onto it) is -1, no current row at all --
            the old fallback, the live index itself, marked whichever
            unrelated frozen row sat at that position as the cursor's. */}
        <PermissionCardContext.Provider value={cardContext}>
          <MessageList
            tab={activeTabId}
            state={frozenSnapshot?.state ?? state}
            turnDurations={frozenSnapshot?.turnDurations ?? turnDurations}
            sessionEnded={frozenSnapshot?.sessionEnded ?? sessionEnded}
            expanded={frozenSnapshot?.expanded ?? expanded}
            cursor={frozenTimeline === null ? cursor : (indexOfKey(frozenTimeline, timeline[cursor]?.key ?? "") ?? -1)}
            timeline={frozenTimeline ?? timeline}
            detailed={frozenSnapshot?.detailed ?? detailed}
            thinkingDisplay={thinkingDisplay}
            // The band is drawn at full strength only while the list has the keys: in INPUT they are in
            // the composer, and a bright band left above it would compete with the box being typed in.
            focused={paneFocused && !edgeFocused && mode !== "input"}
            ruleOffers={frozenSnapshot?.ruleOffers ?? ruleOffers}
            yankedKey={yanked?.key ?? null}
            onAnswerPermission={answerPermission}
            answeredPermissions={frozenSnapshot?.answeredPermissions ?? answeredPermissions}
            onPermissionReason={(permissionId, reason) => {
              if (reason === "") permissionReasons.current.delete(permissionId);
              else permissionReasons.current.set(permissionId, reason);
            }}
            onOpenPath={openPath}
            onUnreadChange={(label, jump, afterSeq) => {
              setUnreadLabel(label);
              unreadJumpRef.current = jump;
              unseenAfterSeqRef.current = afterSeq;
            }}
            unseenSeed={unseenSeed}
            visual={mode === "caret" ? "caret" : mode === "visual" ? "visual" : mode === "vline" ? "vline" : null}
          />
        </PermissionCardContext.Provider>
        {keymapOpen && (
          <PanelErrorBoundary name="keys overlay" onError={() => overlayFailed("keys overlay", () => setKeymapOpen(false))}>
            <KeymapOverlay
              pages={pages}
              ref={keymapOverlayRef}
              onClose={closeKeymapByKey}
              windowKeys={keymapHelp.window}
              prefixKeys={keymapHelp.prefixKeys}
              prefixLabel={keymapHelp.prefix}
              panel={panelTable}
              tmuxSkipped={keymapHelp.tmuxSkipped}
              companion={editorLink !== null}
              plugin={keymapPlugin ?? undefined}
              phones={connectedPhones}
              deviceList={remoteOn}
              startAt={keymapOpening.current !== null && keymapPlugin === null ? "prefix" : undefined}
              startRequest={keymapRequest}
              claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
              agent={isExternalAgent(activeTab?.agent) ? agentTitle(activeTab?.agent) : null}
              modes={hello === null ? undefined : tabPermissionModes(hello.permissionModes, activeTab?.agent)}
            />
          </PanelErrorBoundary>
        )}
        {confirm?.kind === "trust" && trustOverlay}
        {review !== null && (
          <PanelErrorBoundary name="review overlay" onError={() => overlayFailed("review overlay", () => setReview(null))}>
            <ReviewOverlay
              ref={reviewOverlayRef}
              state={review}
              onClose={() => setReview(null)}
              onCommentChange={(text) => setReview((current) => (current === null ? current : typeComment(current, text)))}
              onCommentAccept={(event) => {
                noteLineKey(event);
                const next = applyReviewKey(review, { kind: "accept" }, nextRequestId);
                setReview(next.state);
                if (next.effect !== null) runReviewEffect(next.effect, review.tab);
                returnKeysToRoot();
              }}
              onCommentCancel={(event) => {
                noteLineKey(event);
                setReview(applyReviewKey(review, { kind: "answer", answer: "cancel" }, nextRequestId).state);
                returnKeysToRoot();
              }}
            />
          </PanelErrorBoundary>
        )}
        {detail !== null && (
          <PanelErrorBoundary name="session details" onError={() => overlayFailed("session details", () => setDetail(null))}>
            <DetailPopover
              ref={detailRef}
              rows={detail}
              current={detailCursor}
              onClose={() => setDetail(null)}
              onHandoff={() => {
                setDetail(null);
                setHandoffOpen(true);
              }}
            />
          </PanelErrorBoundary>
        )}
        {tasksOverlay()}
        {chooser !== null && (
          <PanelErrorBoundary name="chooser" onError={() => overlayFailed("chooser", onChooserLeave)}>
            <Chooser
              envelope={chooser}
              tabs={tabs?.tabs ?? []}
              active={tabs?.active ?? null}
              defaultMode={tabs?.defaultMode ?? "auto"}
              projectDir={hello?.projectDir ?? ""}
              newTabChord={keymapHelp.newTabChord}
              focusRequest={chooserFocusRequest}
              dropKeysRequest={emptyDropKeys}
              restore={hello?.restore ?? null}
              onRestoreLast={onChooserRestoreLast}
              onSwitch={onChooserSwitch}
              onResume={onChooserResume}
              onNewSession={onChooserNewSession}
              onCloseTab={onChooserCloseTab}
              onRenameTab={onChooserRenameTab}
              onCycleMode={onChooserCycleMode}
              onCycleTabMode={onChooserCycleTabMode}
              onLeave={onChooserLeave}
              answerConfirm={answerConfirm}
            />
          </PanelErrorBoundary>
        )}
        {/* A bare /model or /effort reply opens this, positioned
            inside `.agent-ui-scroller` the same way the chooser just above is. */}
        {slashPicker !== null && (
          <PanelErrorBoundary name="picker" onError={() => overlayFailed("picker", cancelSlashPicker)}>
            <SlashPicker
              kind={slashPicker.kind}
              options={slashPicker.options}
              current={slashPicker.current}
              labels={slashPicker.labels}
              direct={slashPicker.direct}
              focusRequest={slashPickerFocusRequest}
              onChoose={chooseSlashOption}
              onCancel={cancelSlashPicker}
            />
          </PanelErrorBoundary>
        )}
        {/* The which-key box: `WHICH_KEY_DELAY_MS` after a leader/table
            sequence, or one of `resolveKey`'s own reserved `g`/`z`/`[`/`]` prefixes, is still
            pending. Positioned against `.agent-ui-scroller`, the same wrapper the two overlays
            above are (its own doc comment) -- `bottom: 0` is the footer's own top rule. `box` is
            `null` whenever neither kind of pending state names one (nothing pending, or `boxShown`
            is stale from a keystroke this render has not yet reset -- `clearSequence` always runs
            before either kind is set, so this can only ever under-render, never show a mismatch). */}
        {boxShown && box !== null && (
          <WhichKeyBox
            title={box.title}
            entries={box.entries}
            onPick={(key) => {
              // The same event the sequence engine would have read had this key been typed next --
              // dispatched onto the conversation root so `onKeyDown` decides it exactly once, the
              // same route a real keystroke takes (`./leader`'s own `BoxEntry.key` doc comment: it
              // is already the raw key, never the humanized label, except at the leader's own node,
              // which a click can never reach since the leader is only ever `typed[0]`).
              const root = containerRef.current;
              root?.focus({ preventScroll: true });
              root?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
            }}
          />
        )}
      </div>
      {sessionEndedBanner}
      {rootNoteBanner}
      {commandNoticeBanner}
      <QueueLines items={queue} error={queueError} note={queueNote} />
      <ActivityLine
        state={state}
        onOpenTasks={() => openTasksOverlay()}
        turnClock={turnClock}
        canInterrupt={state.capabilities.interrupt}
        onInterrupt={interrupt}
        queued={heldCount(queue)}
        pendingTool={oldestPendingTool(state)}
        mode={mode}
        shellRunning={shellRunning}
      />
      <TurnChanges state={state} review={reviewSettings} />
      {/* Above the composer, deliberately -- the confirmation
          used to sit below everything, at the very bottom; the band leaves no room there for a
          multi-line dialog, so it moves up here instead, controlled by `handoffOpen`. It must not
          compete with the lost-session banner for the space directly above the box, which is why it
          sits below that banner and the activity/queue lines rather than at the very top. Offered
          for a session that has ENDED too -- continuing a conversation that died here is arguably the
          case where a terminal helps most -- so the only conversation-state term is whether a turn
          is running. `canResume` comes from the provider's advertised capability, never the
          backend's name. */}
      <ContinueInTerminal
        open={handoffOpen}
        onClose={() => setHandoffOpen(false)}
        providerSessionId={state.providerSessionId}
        turnInProgress={turnInProgress}
        canResume={state.capabilities.resume}
        handingOff={handingOff}
        onHandoff={handoffToTerminal}
      />
      {waitingQuestion && activeTab && <QuestionDock
        key={questionIdentity} ref={questionDock} tab={activeTab.id} record={waitingQuestion}
        draft={activeTab.questionDraft?.[waitingQuestion.interactionId]}
        active={mode === "input" && paneFocused} blocked={questionBlocked} onActivate={() => setMode("input")}
        onEditStart={(requestId, index) => {
          editReturnRef.current = { tab: activeTab.id, requestId,
            question: { identity: questionIdentity!, index }, started: false, finished: false };
        }}
        stillWaiting={questionStillWaiting(activeTab.id, waitingQuestion.interactionId)}
      />}
      <Composer
        onSlashAction={runSlashAction}
        // A running turn no longer disables the composer -- it queues a follow-up instead. Only
        // a dead session, or one already being closed for a terminal handoff, takes no more turns:
        // clearing `activeTurnId` on a lost session must not hand the user an enabled composer
        // pointed at nothing.
        disabled={sessionEnded || handingOff}
        sessionEnded={sessionEnded}
        closing={handingOff}
        restoredDraft={restoredDraft}
        mode={mode}
        // The held-back slash commands are Claude Code's; another agent's tab sends a leading `/` as typed.
        claudeSlashCommands={!isExternalAgent(activeTab?.agent)}
        // The `/` menu's rows: the agent's own list when it reported one, Claude's built-ins on a Claude tab,
        // none otherwise (`completionSource`). Withheld while a y/n question is up: an open menu
        // would take the `Esc` that answers it, and the question owns the keys.
        slashCommands={confirm !== null ? null : completionSource(activeTab?.agent, state.availableCommands)}
        atPaths={atPaths}
        onAtQuery={confirm !== null || waitingQuestion ? undefined : askAtPaths}
        // The raw `inputRequest` used to reach `Composer` directly here, so its own
        // `[focusRequest, mode]` effect (focusing the textarea whenever `mode === "input"`) had no
        // way to know an overlay was drawn over it. `composerFocusRequest` is bumped from the
        // `inputRequest` effect's pass branch above (which already gates on `overlayOpen`) and from
        // `resolveKey`'s own `i`/`o`/`A` (C1a, the `"mode"` case above).
        focusRequest={composerFocusRequest}
        caretOnFocus={composerCaret}
        // Only while the box can take a message: a landing on a disabled textarea could not focus it.
        hintTarget={!(sessionEnded || handingOff)}
        onModeChange={(next) => { if (!waitingQuestion) setMode(next); }}
        onSend={sendMessage}
        onRunShell={runShell}
        onDraftChange={mirrorDraft}
        running={turnInProgress || shellRunning}
        enterSteers={enterSteers && !shellRunning}
        onQueue={queueMessage}
        onSendNow={(t) => {
          // `Ctrl+Enter` would end the running turn for a question that needs no turn.
          const bare = barePickerCommand(t);
          if (bare !== null && picksDirectly()) {
            askForPicker(bare);
            return;
          }
          if (settingRunsNow(t)) return;
          post({ type: "send_now", text: t });
          resumeFollowing(containerRef.current?.querySelector(".message-list"));
        }}
        history={history}
        // `↑` takes back only what is held: a message sent into the turn may already be read.
        queueCount={heldCount(queue)}
        onTakeBackQueue={() => post({ type: "take_back_queue" })}
        queueTaken={queueTaken}
        onHistoryPush={(t) => postToRust({ type: "history_push", request_id: nextRequestId(), text: t })}
        onInterrupt={interrupt}
        onEditInNvim={(t) => post({ type: "edit_draft", text: t })}
        editingInNvim={scratchEditing}
        onOpenKeymap={() => {
          setMode("browse");
          setKeymapOpen(true);
        }}
        // The `Ctrl+r` search's Enter/Tab/Escape reach the typing guard.
        onLineKey={noteLineKey}
      />
      {/* `gf` with several paths, and the open `/` prompt, each still own every key
          (`onKeyDown`'s dedicated branches, above `resolveKey` entirely) -- only WHERE they draw
          moved, once `Footer`'s third slot stopped existing. */}
      {/* These four overlays: each sits in its own `PanelErrorBoundary`, so a throw while it renders closes just
          it and says so in the band rather than unmounting the panel. The two pickers hold no focus
          (the root keeps the keys and reads the letter), so closing them is all it takes; the two
          lines are inputs that DO hold it, so they hand the keys back the way their own Esc does. */}
      {pathPick !== null && (
        <PanelErrorBoundary name="path picker" onError={() => overlayFailed("path picker", () => setPathPick(null))}>
          <PathPick
            refs={pathPick.refs.filter((_, i) => !pathPick.shown.includes(pathPick.labels[i]))}
            labels={pathPick.labels.filter((label) => !pathPick.shown.includes(label))}
            typed={pathPick.typed}
          />
          <HintLayer
            root={containerRef.current}
            hints={pathPick.targets.flatMap((target, i) => target === null ? [] : [{ target, label: pathPick.labels[i] }])}
            typed={pathPick.typed}
            onPlaced={pathLabelsPlaced}
          />
        </PanelErrorBoundary>
      )}
      {linkPick !== null && (
        <PanelErrorBoundary name="link picker" onError={() => overlayFailed("link picker", () => setLinkPick(null))}>
          <LinkPick urls={linkPick} />
        </PanelErrorBoundary>
      )}
      {search !== null && (
        <PanelErrorBoundary
          name="search line"
          onError={() =>
            overlayFailed("search line", () => {
              // Esc's own effect: the cursor goes back to where the search started.
              setCursor(search.origin);
              setSearch(null);
              returnKeysToRoot();
            })
          }
        >
          <SearchBar
            query={search.query}
            focusRequest={lineFocusRequest}
            onChange={(query) => {
              setSearch({ query, origin: search.origin });
              const found = findMatch(timeline, query, search.origin, 1, true);
              setCursor(found ?? search.origin);
            }}
            onAccept={(event) => {
              noteLineKey(event);
              lastSearchRef.current = search.query;
              if (search.query !== "" && findMatch(timeline, search.query, search.origin, 1, true) === null) {
                showFlash(`pattern not found: ${search.query}`);
                setCursor(search.origin);
              }
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
            onCancel={(event) => {
              noteLineKey(event);
              setCursor(search.origin);
              setSearch(null);
              containerRef.current?.focus({ preventScroll: true });
            }}
          />
        </PanelErrorBoundary>
      )}
      {commandLineOverlay}
      <DeviceList desk={remoteDesk} covered={remoteCovered} />
      <RemoteEnroll desk={remoteDesk} covered={remoteCovered} />
      {/* The bottom band: replaces `StatusRow`, `Footer`,
          `NewPill` and `ContextLine` with one vim-statusline row. `showcmd` reads the same `box`
          the which-key popup draws from -- vim shows `showcmd` immediately, without that popup's
          own `WHICH_KEY_DELAY_MS` wait (`nvim: 'showcmdloc'`), and `box` is
          already computed unconditionally above for exactly that reason. `prompt` is the window-
          close prompt -- the same y/n `confirm` the old footer drew in its third slot,
          now taking the whole band right of the mode section rather than one slot
          among several, so the question is never cut. Backend has no segment here at all (it
          carries nothing in a release build): `prefix i` only. */}
      <StatusBand
        facts={{
          mode,
          // `true` for `cycleOffered`: ignored entirely in short form (`modePill`'s own doc
          // comment) -- the capability this used to read is gone.
          pill: modePill(activeTab?.mode ?? "auto", true, true),
          showcmd: box !== null ? `${box.title}…` : null,
          message: bandFlash?.text ?? null,
          messageLook: bandFlash?.look,
          messagePath: bandFlash?.path,
          prompt: confirmBandPrompt,
          warn: statusWarning(state.provider, activeTab?.failure ?? null)
            ?? (state.apiRetry ? revealedText(cliPhaseText(state) ?? "API retry") : null)
            ?? (state.rateLimitWarning ? `Usage limit warning${typeof state.rateLimitWarning.scalars.resets_at_ms === "number" ? ` · limit resets ${new Date(state.rateLimitWarning.scalars.resets_at_ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false })}` : ""}` : null),
          unread: unreadLabel,
          cards: allCards(state).length,
          questions: waitingQuestions.length,
          queued: queue.length,
          context: sessionEnded ? null : contextFact(editorContext),
          // About the window, not the session: an ended session still has an editor beside it.
          link: editorLink,
          position: timeline.length === 0 ? null : `${cursor + 1}/${timeline.length}`,
          // While this tab's turn runs the band redraws its figures at most once a second (`steadyBand.ts`).
          streaming: state.activeTurnId !== null,
          scope: activeTab?.id ?? null,
          keyAge: Number.isFinite(currentKeyAtRef.current) ? performance.now() - currentKeyAtRef.current : undefined,
          model: shortModel(state.fallbackModel === null ? state.model : revealedText(state.fallbackModel)),
          effort: state.effort,
          // This tab's last reported usage, right of the model; nothing until one arrives.
          usage: usageSegment(state.usage),
          // How the latest turn ended when it did not complete, until the next turn starts.
          ending: latestTurnEnding(state),
          // The card INPUT's Ctrl+y would approve, while the box has the keys.
          approve: ctrlYCard,
          // A finished turn of this tab changed files on disk; `c` shows them.
          review: activeTab !== null && reviewHints[activeTab.id] !== undefined
            ? { files: reviewHints[activeTab.id].files, keyOnly: reviewSettings.hint === "key" } : null,
          // An interrupted revert is named whatever `review.hint` says: a half-written file is not a diff notice.
          recovery: reviewRecovery.length > 0 ? { path: reviewRecovery[0].path, more: reviewRecovery.length - 1 } : null,
          // Commands this tab's agent left running after its turns ended; nothing while there are none.
          background: sessionEnded ? null : state.backgroundCalls,
          following: activeTab?.following,
          // Phones connected to this desk (remote control); the mark stays while there is one.
          phones: connectedPhones.length,
          phoneDevices: connectedPhones,
          remoteSandbox,
          backgroundTasks: sessionEnded ? [] : state.backend === "sidecar" ? state.backgroundTasks.rows : undefined,
        }}
        paneFocused={paneFocused}
        entryKey={panelEntryKey(keymapHelp.window, editorLink !== null)}
        onOpenDetail={() => post({ type: "open_detail" })}
        onLookMeasured={onLookMeasured}
        messageElRef={bandMessageRef}
        onOpenTasks={() => openTasksOverlay()}
        onJump={() => unreadJumpRef.current()}
        // `/model` and `/effort` are Claude's and an ACP agent's own list. A Codex tab shows what it reports and offers no choice.
        onPickModel={bandPicks ? () => askBandPicker("/model") : undefined}
        onPickEffort={bandPicks ? () => askBandPicker("/effort") : undefined}
      />
      <HintLayer root={containerRef.current} hints={hints} typed={hintTyped} />
    </div>
    </ProjectDirContext.Provider>
  );
}
