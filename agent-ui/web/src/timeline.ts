import type {
  AgentUiState,
  QuestionRecord,
  NoticeRecord,
  PermissionRequestRecord,
  Seq,
  ThinkingBlock,
  ToolCallRecord,
  TranscriptMessage,
  TurnEnding,
  TurnStartRecord,
  TurnCause,
  UserPromptRecord,
} from "./types";

/** One thing that happened in the conversation, ready to render in order.
 *
 * A view over the collections rather than a fifth copy of them: `AgentUiState` stays the
 * shape Rust's `AgentSessionProjection` serializes, and nothing here can drift out of sync with it
 * because nothing here is stored but each record's own item, made from that record alone and held
 * only while the record is (`buildTimeline`'s caches). */
/** `key` is a React key, unique across the whole timeline and stable for as long as the item is.
 *
 * Every kind keys on `seq` rather than on their own identity, which is what makes uniqueness
 * real rather than hoped for. `seq` is unique by construction across all the collections:
 * `AgentSessionProjection::apply` assigns it once per event and no event creates two items
 * (`agent/tests/projection.rs::no_single_event_ever_creates_more_than_one_item` pins that half),
 * and `reducer.ts` continues the same counter from a snapshot's `throughRevision`.
 *
 * Identity keys fail on the proto3 absent-string hazard: an unset id arrives as `""`, and several
 * tool calls can have that id at once. Rust's `pending_permissions` HashMap collapses duplicate
 * empty ids, but the reducer appends permission requests without deduplication, so two live cards
 * can share `permissionId: ""`. Duplicate React keys mis-reconcile the DOM.
 *
 * What keying on `seq` does NOT fix, and is not claimed to: `reducer.ts`'s `permission_resolved`
 * still filters by `permissionId`, so resolving one of two `""` cards removes both. That is a
 * reducer-level consequence of the same hazard and wants its own change. */
export type TimelineItem = (
  /** `delivered`: a message the agent read inside its running turn (`UserPromptRecord.delivered`). */
  | { kind: "prompt"; seq: Seq; key: string; text: string; delivered?: true }
  | { kind: "message"; seq: Seq; key: string; text: string; question?: QuestionRecord; plain?: true }
  | { kind: "tool"; seq: Seq; key: string; call: ToolCallRecord }
  | { kind: "permission"; seq: Seq; key: string; request: PermissionRequestRecord }
  /** Where a turn that did not complete stopped: failed, hit a limit, interrupted, or cut off by the
   *  session ending. Keyed `e-<seq>` like the others, by the `seq` of the event that ended the turn. */
  | { kind: "ending"; seq: Seq; key: string; ending: TurnEnding }
  /** A CLI turn cause or task notification, with optional plain text detail. */
  | {
      kind: "wake"; seq: Seq; key: string; text: string; body: string | null;
      scheduled: boolean; count: number; inlineBody?: boolean;
    }
  /** A durable CLI notice; transient activity stays out of the transcript. */
  | { kind: "notice"; seq: Seq; key: string; notice: NoticeRecord }
  /** One block of the agent's thinking, keyed `k-<seq>`. `open` while the agent is still writing it
   *  (the state's `thinkingOpen`, for its last block only). Whether and how it is drawn is
   *  `display.ts`'s and `MessageList`'s to decide, by the panel's `agent.thinking` level. */
  | { kind: "thinking"; seq: Seq; key: string; block: ThinkingBlock; open: boolean }
  /** A collapsed run of finished tool calls, drawn as one row. Made by `display.ts`, never by
   *  `buildTimeline` -- this function's own ordering (by `seq`, cards anchored after their call)
   *  stays exactly as it was, and folding several rows into one is a DISPLAY decision layered on
   *  top of it, not a change to what the conversation actually contains. `key` is `r-<first seq>`. */
  | {
      kind: "run";
      seq: Seq;
      key: string;
      calls: ToolCallRecord[];
      /** Finished thinking blocks that sat between two of `calls`, folded away with them; absent when
       *  none did. Expanding the run puts them back as rows of their own. */
      thoughts?: ThinkingBlock[];
    }) & { depth?: number };

/** Durable notices drawn in sequence with the conversation. */
const ROW_NOTICE_KINDS: ReadonlySet<NoticeRecord["kind"]> = new Set([
  "hook_block", "unseen_turn", "model_fallback", "model_refusal", "compaction", "informational", "peer_hold", "rate_limit",
  "shell_output",
]);

function drawsNotice(notice: NoticeRecord): boolean {
  if (!ROW_NOTICE_KINDS.has(notice.kind)) return false;
  if (notice.kind === "informational") return notice.level !== "info";
  if (notice.kind === "peer_hold") return ["held", "dropped"].includes(String(notice.scalars.hold_state));
  if (notice.kind === "rate_limit") return !["allowed", "allowed_warning", "rejected"].includes(String(notice.scalars.rate_status));
  return true;
}

const scalarText = (notice: NoticeRecord, key: string): string =>
  typeof notice.scalars[key] === "string" ? notice.scalars[key] as string : "";
const firstLine = (text: string): string => Array.from(text.split(/\r?\n/, 1)[0]).slice(0, 200).join("");

/** Task notifications and task-caused turns share the same headline. */
export function taskWakeText(cause: TurnCause): string {
  const desc = cause.description ?? cause.taskId ?? "";
  const summary = cause.summary ?? "";
  const status = cause.taskStatus ?? "completed";
  if (cause.taskKind === "workflow") return summary || `Dynamic workflow "${desc}" ${status}`;
  if (cause.taskKind === "monitor" && status === "running") return `Monitor event: "${desc}"`;
  if (["local_agent", "agent"].includes(cause.taskKind ?? "") && status === "completed") return `Agent "${desc}" finished`;
  if (["shell", "local_bash"].includes(cause.taskKind ?? "")) {
    if (summary.includes("was stopped after reaching its background time limit")) return summary;
    const code = summary.match(/exit code[ :]+(-?\d+)/i)?.[1];
    return `Background command "${desc}" ${status === "failed" ? "failed" : status === "stopped" ? "stopped" : "completed"}${code && Number(code) !== 0 ? ` (exit code ${code})` : ""}`;
  }
  return `Task "${desc}" ${status === "completed" ? "completed in background" : status}`;
}

type WakeItem = Extract<TimelineItem, { kind: "wake" }>;

/** The second line of a row only when it says more than the headline already does. */
const extraBody = (text: string, body: string | null): string | null =>
  body === null || body === "" || text === body || text.endsWith(body) ? null : body;

/** Wake rows are memoized on their item like every other row: the same object while what they say is
 *  the same. `held` is the one the record produced last time. */
function sameWake(held: WakeItem | undefined, item: WakeItem): WakeItem {
  if (held && held.seq === item.seq && held.text === item.text && held.body === item.body &&
      held.scheduled === item.scheduled && held.inlineBody === item.inlineBody) return held;
  return item;
}

const wakeItems = new WeakMap<TurnStartRecord, WakeItem>();

function wakeItem(start: TurnStartRecord): WakeItem {
  const cause = start.cause;
  let text = "Claude Code started this turn";
  let body: string | null = null;
  if (cause?.kind === "scheduled") {
    text = cause.wakeupSource === "loop_wakeup" && cause.scheduleReason
      ? `Scheduled wake-up: ${cause.scheduleReason}`
      : cause.wakeupSource === "schedule_wakeup" && cause.prompt
        ? `Scheduled task fired: ${firstLine(cause.prompt)}` : "Scheduled wake-up";
  } else if (cause?.kind === "peer") {
    text = `› Message from @${cause.peerName ?? "unknown"}: ${firstLine(cause.text ?? "")}`;
    body = extraBody(text, cause.text);
  } else if (cause?.kind === "task") {
    text = taskWakeText(cause);
    body = extraBody(text, cause.summary ?? cause.text);
  }
  const item = sameWake(wakeItems.get(start), {
    kind: "wake", seq: start.seq, key: `w-${start.seq}`, text, body,
    scheduled: cause?.kind === "scheduled", count: 1,
    inlineBody: cause?.taskKind === "monitor" && cause.taskStatus === "running",
  });
  wakeItems.set(start, item);
  return item;
}

/** What the turn that a task started says about the task: the notice record carries only the id, the
 *  status and the CLI's words, so the kind and description come from that turn's cause. */
function taskCauses(starts: readonly TurnStartRecord[]): Map<string, TurnCause> {
  const byTask = new Map<string, TurnCause>();
  for (const start of starts) {
    const cause = start.cause;
    if (cause?.kind === "task" && cause.taskId && !byTask.has(cause.taskId)) byTask.set(cause.taskId, cause);
  }
  return byTask;
}

const taskNotices = new WeakMap<NoticeRecord, WakeItem>();

function taskNoticeItem(notice: NoticeRecord, known: TurnCause | undefined): WakeItem {
  const cause: TurnCause = {
    kind: "task", taskId: notice.taskId, taskKind: known?.taskKind ?? null,
    taskStatus: notice.taskStatus ?? known?.taskStatus ?? null, description: known?.description ?? null,
    summary: notice.content, peerName: null, text: null, wakeupSource: null, scheduledForMs: null,
    scheduleReason: null, cronId: null, prompt: null,
  };
  // Without the kind the row cannot be worded by it; the CLI's own words stand instead.
  const text = cause.taskKind ? taskWakeText(cause) : notice.content || taskWakeText(cause);
  const item = sameWake(taskNotices.get(notice), {
    kind: "wake", seq: notice.seq, key: `w-${notice.seq}`, text, body: extraBody(text, notice.content),
    scheduled: false, count: 1, inlineBody: cause.taskKind === "monitor" && cause.taskStatus === "running",
  });
  taskNotices.set(notice, item);
  return item;
}

/** A run of empty scheduled ticks keeps only its latest combined item beside its first row. */
const foldedWakes = new WeakMap<WakeItem, WakeItem>();

function foldedWake(first: WakeItem, count: number): WakeItem {
  let item = foldedWakes.get(first);
  if (item?.count !== count) foldedWakes.set(first, (item = { ...first, count }));
  return item;
}

export function wakeText(item: Extract<TimelineItem, { kind: "wake" }>): string {
  return `${item.text}${item.count > 1 ? ` ×${item.count}` : ""}`;
}

export function noticeBody(notice: NoticeRecord): string | null {
  if (notice.kind === "shell_output") {
    return (notice.content?.split("\n").length ?? 0) > SHELL_ROW_LINES ? notice.content : null;
  }
  return notice.kind === "compaction" ? notice.content : null;
}

/** The lines of a `!` line's output a closed row shows; `index.css` clamps to the same number. */
export const SHELL_ROW_LINES = 12;

/** The sentence a turn this panel never saw is introduced by: its answer follows as a reply of its own. */
export const UNSEEN_TURN_TEXT = "Claude Code ran a turn this panel could not show; reopen the tab to see its steps";

/** What a notice row says, whole: what `y` copies and `/` searches. The hook's words are the CLI's,
 *  drawn as plain text; the row itself shows only their first lines. A warning's `⚠` is the row's sign
 *  (`MessageList`), not part of the text, so the row shows it once. */
export function noticeText(notice: NoticeRecord): string {
  switch (notice.kind) {
    case "hook_block":
      return notice.content?.startsWith("UserPromptSubmit operation blocked by hook:")
        ? notice.content : `UserPromptSubmit operation blocked by hook: ${notice.content ?? ""}`;
    case "unseen_turn":
      return UNSEEN_TURN_TEXT;
    case "peer_hold":
      return `Message from @${scalarText(notice, "peer_name")} ${notice.scalars.hold_state === "dropped"
        ? "dropped after 5 minutes unanswered" : "held: its sender runs in a different permission mode"}`;
    case "rate_limit":
      return "Usage limit reset · continuing automatically";
    case "compaction": {
      const { pre_tokens: before, post_tokens: after } = notice.scalars;
      const tokens = (n: number) => n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);
      const figures = typeof before === "number" && typeof after === "number"
        ? ` · ${tokens(before)} → ${tokens(after)} tokens` : "";
      const trigger = scalarText(notice, "trigger");
      return `──── Conversation compacted${figures}${trigger ? ` (${trigger})` : ""} ────`;
    }
    default:
      return notice.content ?? notice.kind;
  }
}

/** Whether a `toolUseId` can identify a tool call at all.
 *
 * Two values cannot, and both are real:
 *   null -- the legacy backend (a `shell/legacy-backend` build only) sends no id
 *           (`agent/src/session.rs`).
 *   ""   -- proto3 has no absent-string, so an unset `tool_use_id` arrives as "" from the sidecar.
 *           A `ToolCallRecord.toolUseId` crosses the same boundary and can be "" for the same
 *           reason, so admitting it would anchor an arbitrary card to an arbitrary call.
 *
 * Shared by timeline anchoring, `MessageList`'s awaiting-permission marker and `PermissionCard`'s
 * call label so their checks cannot drift apart. */
export function isUsableLink(toolUseId: string | null): toolUseId is string {
  return toolUseId !== null && toolUseId !== "";
}

/**
 * The collections merged into the one sequence they actually formed.
 *
 * Ordering comes from `seq`, which Rust's `AgentSessionProjection::apply` assigns and
 * `serialize_snapshot_for_js` ships -- this function sorts by an authoritative key, it does not
 * infer an order from the shape of the data. That is what makes the result survive a reload or a
 * `UiDelivery::Resync`: both discard the frontend's whole state and rebuild it from a snapshot,
 * and the snapshot carries these numbers.
 *
 * The one placement that is NOT by `seq`: a pending permission whose `toolUseId` names a tool call
 * present here is emitted immediately after that call, because the card is about that specific
 * invocation and a turn can have several of the same tool in flight. Everything else -- an unlinked
 * card, or one naming a call this state does not have -- falls back to its own `seq`, so nothing is
 * ever dropped for want of a link.
 *
 * SAY IT PLAINLY, because a reader will otherwise assume this function is uniformly exercised: that
 * anchoring branch -- `anchored`, `knownToolUseIds`, the per-call sibling sort, the `anchored.delete`
 * guard, roughly half of what is below -- CANNOT RUN ON THE LEGACY BACKEND, which is what a source
 * build still starts on. Every legacy permission carries `toolUseId: null` (`agent/src/session.rs`
 * passes `tool_use_id: None`, deliberately, with its reasons written out there), so `isUsableLink`
 * rejects all of them and every card takes the `unanchored` path.
 *
 * Only the sidecar backend sends a real id. That path was driven on a real screen against the
 * installed CLI (2.1.272) and linked-permission anchoring was exercised there, so it is not covered
 * by unit tests alone; `timeline.test.ts` and `MessageList.test.tsx` still cover it, and are the only
 * coverage of the branch on the DEFAULT backend, where it is unreachable by construction.
 * The transcript/tool merge and sort also run on legacy, where all cards are unanchored.
 */
let timelineBuilds = 0;
/** How many times `buildTimeline` has run in this page: the performance tests count it. */
export function timelineBuildCount(): number {
  return timelineBuilds;
}

/* Each item is made once per record and handed out again while that record is the same object.
   The reducer replaces only the records an event changed (the reply or thought being streamed, the
   call that completed), and every row is memoized on its item, so a batch of streamed deltas renders
   the one row whose record changed rather than every row of the conversation. A snapshot brings new
   records, and with them new items, once. Each cache also checks the fields its item copies, so a
   record changed in place (only ever a test) is never answered with a stale item. */
const promptItems = new WeakMap<UserPromptRecord, TimelineItem>();
const messageItems = new WeakMap<TranscriptMessage, TimelineItem>();
const toolItems = new WeakMap<ToolCallRecord, TimelineItem>();
const permissionItems = new WeakMap<PermissionRequestRecord, TimelineItem>();
const endingItems = new WeakMap<TurnEnding, TimelineItem>();
const noticeItems = new WeakMap<NoticeRecord, TimelineItem>();
/** By block, one item for each value of `open`: a block's item changes when it closes. */
const thinkingItems = new WeakMap<ThinkingBlock, { open?: TimelineItem; closed?: TimelineItem }>();

function promptItem(prompt: UserPromptRecord): TimelineItem {
  const held = promptItems.get(prompt);
  if (held?.kind === "prompt" && held.seq === prompt.seq && held.text === prompt.text && held.delivered === prompt.delivered)
    return held;
  const item: TimelineItem = {
    kind: "prompt",
    seq: prompt.seq,
    key: `u-${prompt.seq}`,
    text: prompt.text,
    ...(prompt.delivered ? { delivered: true as const } : {}),
  };
  promptItems.set(prompt, item);
  return item;
}

function messageItem(message: TranscriptMessage): TimelineItem {
  const held = messageItems.get(message);
  if (held?.kind === "message" && held.seq === message.seq && held.text === message.text) return held;
  const item: TimelineItem = { kind: "message", seq: message.seq, key: `m-${message.seq}`, text: message.text };
  messageItems.set(message, item);
  return item;
}

function toolItem(call: ToolCallRecord): TimelineItem {
  const held = toolItems.get(call);
  if (held !== undefined && held.seq === call.seq) return held;
  const item: TimelineItem = { kind: "tool", seq: call.seq, key: `t-${call.seq}`, call };
  toolItems.set(call, item);
  return item;
}

/** Every card waiting on this tab: the provider's (`pendingPermissions`) and the ones Eitri's own tool broker raised
 *  (`editorCards`). Anything that asks "is a card pending" reads this, so a broker card is never missed. Provider
 *  cards first, each list in its own order. With no broker card it is `pendingPermissions` itself, so a memo keyed on that list keeps its identity. */
export function allCards(
  state: Pick<AgentUiState, "pendingPermissions"> & Partial<Pick<AgentUiState, "editorCards">>,
): PermissionRequestRecord[] {
  const own = state.editorCards;
  return own === undefined || own.length === 0 ? state.pendingPermissions : [...state.pendingPermissions, ...own];
}

/** What a broker card says, as plain text: its title, then each line. `null` for a provider's request, whose
 *  readers use its `input`. */
export function editorCardText(request: PermissionRequestRecord): string | null {
  const card = request.editorCard;
  return card === undefined ? null : [card.title, ...card.body.map((line) => line.text)].join("\n");
}

function permissionItem(request: PermissionRequestRecord): TimelineItem {
  const held = permissionItems.get(request);
  if (held !== undefined && held.seq === request.seq) return held;
  // A broker card's `seq` is the revision the next event would take, so a later row may share it: its key is its id.
  const key = request.editorCard === undefined ? `p-${request.seq}` : `ec-${request.permissionId}`;
  const item: TimelineItem = { kind: "permission", seq: request.seq, key, request };
  permissionItems.set(request, item);
  return item;
}

function endingItem(ending: TurnEnding): TimelineItem {
  const held = endingItems.get(ending);
  if (held !== undefined && held.seq === ending.seq) return held;
  const item: TimelineItem = { kind: "ending", seq: ending.seq, key: `e-${ending.seq}`, ending };
  endingItems.set(ending, item);
  return item;
}

function noticeItem(notice: NoticeRecord): TimelineItem {
  const held = noticeItems.get(notice);
  if (held !== undefined && held.seq === notice.seq) return held;
  const item: TimelineItem = { kind: "notice", seq: notice.seq, key: `n-${notice.seq}`, notice };
  noticeItems.set(notice, item);
  return item;
}

function thinkingItem(block: ThinkingBlock, open: boolean): TimelineItem {
  let slots = thinkingItems.get(block);
  if (slots === undefined) {
    slots = {};
    thinkingItems.set(block, slots);
  }
  const held = open ? slots.open : slots.closed;
  if (held !== undefined && held.seq === block.seq) return held;
  const item: TimelineItem = { kind: "thinking", seq: block.seq, key: `k-${block.seq}`, block, open };
  if (open) slots.open = item;
  else slots.closed = item;
  return item;
}

/** The transcript keeps exactly the answers sent to the CLI, including partial answers. */
export function questionLines(record: QuestionRecord): { text: string; muted: boolean }[] {
  const lines: { text: string; muted: boolean }[] = [];
  const settled = record.state.kind === "settled" ? record.state : null;
  record.questions.forEach((question, index) => {
    lines.push({ text: `? ${question.header ? `${question.header}: ` : ""}${question.question}`, muted: false });
    for (const answer of settled?.answers.filter((answer) => answer.questionIndex === index) ?? []) {
      lines.push({ text: `→ ${answer.answer}`, muted: true });
      if (answer.notes) lines.push({ text: `→ notes: ${answer.notes}`, muted: true });
    }
  });
  const outcome = settled?.outcome;
  const status = !settled ? "waiting for your answer"
    : outcome === "chat_requested" ? "→ chat about this"
    : outcome === "declined" ? "User declined to answer questions"
    : outcome === "interrupted" ? "(not answered: interrupted)"
    : outcome === "expired" ? "(not answered: Claude Code withdrew the question)"
    : outcome === "session_closed" ? "(not answered: session closed)"
    : outcome === "provider_failed" ? "(not answered: provider failed)"
    : outcome === "host_refused" ? "(not answered: host refused)" : "";
  if (status) lines.push({ text: status, muted: true });
  if (settled?.reason) lines.push({ text: settled.reason, muted: true });
  if (record.unsupportedReason) lines.push({ text: record.unsupportedReason, muted: true });
  return lines;
}

const questionItems = new WeakMap<QuestionRecord, TimelineItem>();

function questionItem(record: QuestionRecord, call?: ToolCallRecord): TimelineItem {
  const key = call ? `t-${call.seq}` : `q-${record.seq}`;
  const held = questionItems.get(record);
  if (held?.key === key) return held;
  const item: TimelineItem = { kind: "message", seq: call?.seq ?? record.seq, key: call ? `t-${call.seq}` : `q-${record.seq}`,
    text: questionLines(record).map((line) => line.text).join("\n"), question: record };
  questionItems.set(record, item);
  return item;
}

function questionFallback(call: ToolCallRecord): TimelineItem {
  const content = call.result?.content;
  const text = typeof content === "string" ? content : Array.isArray(content)
    ? content.map((block) => typeof block?.text === "string" ? block.text : "").filter(Boolean).join("\n")
    : content == null ? "" : JSON.stringify(content);
  return { kind: "message", seq: call.seq, key: `t-${call.seq}`, plain: true,
    text: `Claude asked a question${text ? `\n${text}` : ""}` };
}

export function buildTimeline(state: AgentUiState): TimelineItem[] {
  const firstTaskNotice = new Map<string, Seq>();
  for (const notice of state.notices ?? []) {
    if (notice.kind !== "task" || !notice.taskId) continue;
    const previous = firstTaskNotice.get(notice.taskId);
    if (previous === undefined || notice.seq < previous) firstTaskNotice.set(notice.taskId, notice.seq);
  }
  timelineBuilds += 1;
  const causes = taskCauses(state.turnStarts ?? []);
  // Which linked cards hang off which call. A call may gate more than one.
  const anchored = new Map<string, PermissionRequestRecord[]>();
  const unanchored: PermissionRequestRecord[] = [];
  const knownToolUseIds = new Set(state.toolCalls.map((call) => call.toolUseId).filter(isUsableLink));

  for (const request of allCards(state)) {
    const link = request.toolUseId;
    if (isUsableLink(link) && knownToolUseIds.has(link)) {
      const siblings = anchored.get(link);
      if (siblings) siblings.push(request);
      else anchored.set(link, [request]);
    } else {
      unanchored.push(request);
    }
  }
  // Several cards on one call keep the order they were requested in.
  for (const siblings of anchored.values()) siblings.sort((a, b) => a.seq - b.seq);

  const questionByCall = new Map<string, QuestionRecord>();
  for (const record of state.questions ?? []) if (record.toolUseId && !questionByCall.has(record.toolUseId)) questionByCall.set(record.toolUseId, record);

  const base: TimelineItem[] = [
    ...state.userPrompts.map(promptItem),
    ...state.transcript.map(messageItem),
    ...state.toolCalls.map((call) => {
      const record = questionByCall.get(call.toolUseId);
      if (record) return questionItem(record, call);
      if (call.name === "AskUserQuestion") return questionFallback(call);
      const background = state.backgroundTasks?.rows.some((task) => task.tool_use_id === call.toolUseId && task.is_backgrounded === true);
      return toolItem(background && !call.background ? { ...call, background: true } : call);
    }),
    ...(state.questions ?? []).filter((record) => !record.toolUseId || !knownToolUseIds.has(record.toolUseId)).map((record) => questionItem(record)),
    ...unanchored.map(permissionItem),
    ...state.turnEndings.map(endingItem),
    // `?? []` for a hand-built state in a test, as for thinking below.
    ...(state.notices ?? []).filter(drawsNotice).map(noticeItem),
    ...(state.notices ?? []).filter((notice) => notice.kind === "task").map((notice) => taskNoticeItem(notice, causes.get(notice.taskId ?? ""))),
    // A turn a task started whose notification is already a row draws nothing more.
    ...(state.turnStarts ?? []).filter((start) => !start.cause?.taskId ||
      (firstTaskNotice.get(start.cause.taskId) ?? Infinity) > start.seq).map(wakeItem),
    // A snapshot from a build before thinking was kept has none (`applySnapshot` fills `[]`); the
    // `?? []` covers a hand-built state in a test.
    ...(state.thinking ?? []).map((block, i, all) => thinkingItem(block, state.thinkingOpen === true && i === all.length - 1)),
  ];
  base.sort((a, b) => a.seq - b.seq);

  const out: TimelineItem[] = [];
  for (const item of base) {
    out.push(item);
    if (item.kind !== "tool") continue;
    const siblings = anchored.get(item.call.toolUseId);
    if (!siblings) continue;
    // Deleted as it is consumed, so two calls sharing one id (which should not happen, and which
    // this function must not amplify if it does) cannot emit the same card twice.
    anchored.delete(item.call.toolUseId);
    for (const request of siblings) out.push(permissionItem(request));
  }
  const turnIds = new Map((state.turnStarts ?? []).map((start) => [start.seq, start.turnId]));
  const emptyScheduled = new Set<Seq>();
  let hasReply = false;
  for (let i = out.length - 1; i >= 0; i--) {
    const item = out[i];
    if (item.kind === "message" && item.text !== "" && !item.question) hasReply = true;
    if (item.kind === "wake") {
      if (item.scheduled && !hasReply && turnIds.get(item.seq) !== state.activeTurnId) emptyScheduled.add(item.seq);
      hasReply = false;
    }
  }
  const folded: TimelineItem[] = [];
  for (let i = 0; i < out.length; i++) {
    const item = out[i];
    if (item.kind === "wake" && item.scheduled && emptyScheduled.has(item.seq)) {
      let end = i + 1;
      while (end < out.length) {
        const next = out[end];
        if (next.kind !== "wake" || !next.scheduled || !emptyScheduled.has(next.seq)) break;
        end++;
      }
      folded.push(end > i + 1 ? foldedWake(item, end - i) : item);
      i = end - 1;
    } else folded.push(item);
  }
  return folded;
}

/**
 * Where `focus_permission` puts the cursor: the timeline index of the OLDEST
 * pending card -- the lowest `seq`, the one the model has waited on longest, whatever row it is
 * drawn under -- or `null` when no card is pending.
 */
export function oldestPendingPermission(timeline: TimelineItem[]): number | null {
  let best: number | null = null;
  timeline.forEach((item, index) => {
    if (item.kind !== "permission") return;
    const current = best === null ? null : timeline[best];
    if (current === null || (current.kind === "permission" && item.seq < current.seq)) best = index;
  });
  return best;
}

/** `[[`/`]]`: the next `prompt` row before (`-1`) or after (`1`) `from`, or `null` when there is
 *  none. Never wraps -- `[[` at the first prompt, or `]]` at the last, simply does nothing, the same
 *  way `clampStep` (`nav.ts`) stops `j`/`k` at either end rather than cycling. */
export function promptIndex(timeline: TimelineItem[], from: number, delta: 1 | -1): number | null {
  for (let i = from + delta; i >= 0 && i < timeline.length; i += delta) {
    if (timeline[i].kind === "prompt") return i;
  }
  return null;
}

/** Whether `item` is a card waiting for an answer from this panel: a permission item whose id is not in
 *  `answered` -- the ids this tab has answered, by a card's button or in card mode, which stay drawn (inert)
 *  until the provider resolves them. The one condition a timeline cannot know is whether the session still
 *  lives (a dead session's cards are inert, `PermissionCard`): that is the caller's. */
function isWaitingCard(item: TimelineItem, answered: ReadonlySet<string>): boolean {
  return item.kind === "permission" && !answered.has(item.request.permissionId);
}

/** Card mode's `Alt-n` (+1) / `Alt-p` (-1): the next waiting card after (before) `from`, wrapping as nvim's `]d`
 *  does (`vim.diagnostic.jump`'s default) -- unlike `[[`/`]]` (`promptIndex`), which stop at either end. A card
 *  this panel already answered is not waiting. With one card waiting, and `from` on it, the answer is `from`
 *  itself (a lone diagnostic is its own next); `null` when none waits at all. `from` may lie outside the
 *  timeline (it counts round the ring), so a cursor left one past a shortened list still finds a real row. */
export function waitingCardIndex(
  timeline: TimelineItem[],
  from: number,
  delta: 1 | -1,
  answered: ReadonlySet<string>,
): number | null {
  const n = timeline.length;
  for (let step = 1; step <= n; step++) {
    const i = (((from + delta * step) % n) + n) % n;
    if (isWaitingCard(timeline[i], answered)) return i;
  }
  return null;
}

/** `waitingCardIndex` taken `times` times, each step from the card the last one landed on (card mode steps once). The first step reaches a waiting card and every step after it only goes round the
 *  ring of them, so a count is at most as many steps as there are waiting cards -- `times - 1` modulo that,
 *  plus the first -- however large it is: it costs O(waiting x rows), where repeating the single step `times`
 *  times cost O(times x rows), which for the panel's cap of 9999 over a long conversation with one card waiting
 *  is a whole scan of it 9999 times inside one keydown. The same landing either way. `null` when no card waits. */
export function waitingCardAfter(
  timeline: TimelineItem[],
  from: number,
  delta: 1 | -1,
  times: number,
  answered: ReadonlySet<string>,
): number | null {
  let waiting = 0;
  for (const item of timeline) if (isWaitingCard(item, answered)) waiting++;
  if (waiting === 0) return null;
  let target = from;
  const steps = ((Math.max(1, times) - 1) % waiting) + 1;
  for (let step = 0; step < steps; step++) target = waitingCardIndex(timeline, target, delta, answered) ?? target;
  return target;
}

/** Structural stops are exclusive of the starting row and never wrap. */
export function nextStop(
  timeline: TimelineItem[],
  from: number,
  delta: 1 | -1,
  what: "tool" | "error" | "edit",
): number | null {
  for (let i = from + delta; i >= 0 && i < timeline.length; i += delta) {
    const item = timeline[i];
    if (what === "tool") {
      if (item.kind === "tool" || item.kind === "run") return i;
    } else if (what === "error") {
      if (item.kind === "tool" && (item.call.result?.isError || item.call.denied)) return i;
      if (item.kind === "notice" && ["hook_block", "model_refusal"].includes(item.notice.kind)) return i;
      if (item.kind === "ending" && (item.ending.kind === "failed" || item.ending.kind === "lost")) return i;
    } else if (item.kind === "tool") {
      const { name, result } = item.call;
      if (result && !result.isError && (
        ["Write", "Edit", "MultiEdit", "NotebookEdit", "acp:edit", "acp:delete", "acp:move"].includes(name) ||
        (name === "fileChange" && result.content === "applied")
      )) return i;
    }
  }
  return null;
}
