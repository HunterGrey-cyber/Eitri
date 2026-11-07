import type { CompletionCommand } from "./slashCommands";

/** The ordering key every item in a conversation carries, shared across `TranscriptMessage`,
 * `ToolCallRecord`, `PermissionRequestRecord` and `UserPromptRecord` so the four can be
 * interleaved into the one sequence they really formed.
 *
 * Authoritative, not derived here: Rust's `AgentSessionProjection::apply` assigns it from the same
 * counter as `last_revision`, and `serialize_snapshot_for_js` ships it. That is what makes the
 * order survive a reload or a `UiDelivery::Resync` -- both throw this whole state away and rebuild
 * it from a snapshot, so an order the frontend had accumulated for itself would vanish with it.
 *
 * `reducer.ts` continues the same numbering for events folded after a snapshot (see `nextSeq`),
 * using the wire's own delivery order rather than inventing one. */
export type Seq = number;
/** One assistant message. `seq` is where the message STARTED -- appending a streamed chunk never
 * moves it, or a reply still streaming would keep sliding below the tool call that interrupted it. */
export type TranscriptMessage = { seq: Seq; text: string };
/** One prompt the user sent, as they typed it.
 *
 * NOT what went on the wire: wire 1 composes editor context into the outgoing turn above both
 * backends, and Rust deliberately records the pre-composition text (`agent/src/projection.rs`'s
 * `UserPromptSubmitted`). Rendering the wire text would show the user a file path and a selection
 * they never wrote. */
export type SentFrom = { deviceId: string; name: string };

export type UserPromptRecord = {
  seq: Seq;
  text: string;
  /** A message put into a running turn that the agent read there (`user_message_delivered`): drawn as
   *  part of that turn, not as the start of a new one. Absent on every other prompt; Rust sends the key
   *  only when it is true, so the two folds stay equal. */
  delivered?: true;
};
/** One run of the agent's thinking (Rust's `ThinkingBlock`): consecutive thinking deltas of one turn,
 * ordered by the delta that opened it, as a reply is. Plain text the provider sent: drawn as text,
 * never as markdown or HTML. */
export type ThinkingBlock = { seq: Seq; turnId: string; text: string };

/** Who started a turn (Rust's `TurnOrigin`): `client` is a turn this panel sent, and what a turn with no
 *  `turn_attributed` is; `follow_up` a turn the CLI ran after another to read a message; `cli` a turn the
 *  CLI started by itself. */
export type TurnOrigin = "client" | "follow_up" | "cli";
export type TurnCauseKind = "task" | "scheduled" | "peer" | "other" | "unknown";
/** Why the CLI started a turn, as the event spells it (snake_case, Rust's `TurnCause`). Every string is
 *  the CLI's or a task's text: drawn as plain text. */
export type WireTurnCause = {
  kind: TurnCauseKind;
  tool_use_id?: string;
  task_id: string | null;
  task_kind: string | null;
  task_status: string | null;
  description: string | null;
  summary: string | null;
  peer_name: string | null;
  text: string | null;
  wakeup_source: string | null;
  scheduled_for_ms: number | null;
  schedule_reason: string | null;
  cron_id: string | null;
  prompt: string | null;
};
/** The same cause as the state and a snapshot keep it. */
export type TurnCause = {
  kind: TurnCauseKind;
  toolUseId?: string;
  taskId: string | null;
  taskKind: string | null;
  taskStatus: string | null;
  description: string | null;
  summary: string | null;
  peerName: string | null;
  text: string | null;
  wakeupSource: string | null;
  scheduledForMs: number | null;
  scheduleReason: string | null;
  cronId: string | null;
  prompt: string | null;
};
/** The start of a turn the user did not send, as a row of the conversation (Rust's `TurnStartRecord`).
 *  A turn of the user's own has none. */
export type TurnStartRecord = { seq: Seq; turnId: string; origin: TurnOrigin; cause: TurnCause | null };

/** Rust's `SessionNoticeKind`: both recorded rows and live activity or band updates. */
export type SessionNoticeKind =
  | "api_retry"
  | "compacting"
  | "notification"
  | "task"
  | "model_fallback"
  | "model_refusal"
  | "compaction"
  | "informational"
  | "hook_block"
  | "peer_hold"
  | "rate_limit"
  | "shell_output"
  | "unseen_turn";
export type NoticeLevel = "info" | "notice" | "suggestion" | "warning";
/** One notice row (Rust's `NoticeRecord`). `content` is the CLI's own words when it gave any; `scalars`
 *  the plain values a notice without words is worded from, by name. Both are drawn as plain text. */
export type NoticeRecord = {
  seq: Seq;
  kind: SessionNoticeKind;
  level: NoticeLevel;
  content: string | null;
  scalars: Record<string, unknown>;
  taskId: string | null;
  toolUseId: string | null;
  taskStatus: string | null;
};

/** How a question set ended (Rust's `QuestionOutcome`). */
export type QuestionOutcome =
  | "answered"
  | "declined"
  | "chat_requested"
  | "interrupted"
  | "expired"
  | "session_closed"
  | "provider_failed"
  | "host_refused";
export type WireQuestionOption = { label: string; description: string; preview: string | null };
export type WireQuestionSpec = { question: string; header: string; multi_select: boolean; options: WireQuestionOption[] };
export type WireShownAnswer = { question_index: number; answer: string; notes: string | null };
export type QuestionOption = { label: string; description: string; preview: string | null };
export type QuestionSpec = { question: string; header: string; multiSelect: boolean; options: QuestionOption[] };
/** One answer as it was sent, for the question at `questionIndex` of its set. */
export type ShownAnswer = { questionIndex: number; answer: string; notes: string | null };
export type QuestionState =
  | { kind: "waiting" }
  | { kind: "answering" }
  | { kind: "settled"; outcome: QuestionOutcome; answers: ShownAnswer[]; reason: string | null };
/** One question set Claude asked, as a row of the conversation (Rust's `QuestionRecord`). Not a
 *  permission card: nothing that answers cards reads it. */
export type QuestionRecord = {
  seq: Seq;
  interactionId: string;
  toolUseId: string | null;
  /** The turn that was running when it was asked, if one was. */
  turnId: string | null;
  questions: QuestionSpec[];
  state: QuestionState;
  unsupportedReason: string | null;
};
/** How the panel shows thinking (`init.lua`'s `agent.thinking`, the `display` envelope): `collapsed`
 * (the default) is one muted row per block, its text behind the row's fold; `preview` shows a block's
 * last lines in a box of fixed height while it is written, then the collapsed row; `expanded` shows every
 * block whole; `hidden` draws no thinking rows. The detailed view (`Ctrl+o`) shows every block whole
 * whatever this says. */
export type ThinkingDisplay = "collapsed" | "preview" | "expanded" | "hidden";
export const THINKING_DISPLAYS: readonly ThinkingDisplay[] = ["collapsed", "preview", "expanded", "hidden"];
export type SubagentStream = {
  transcript: TranscriptMessage[];
  thinking: ThinkingBlock[];
  toolCalls: ToolCallRecord[];
  assistantMessageOpen: boolean;
  thinkingOpen: boolean;
};
export type ToolCallRecord = {
  children?: SubagentStream;
  background?: boolean;
  seq: Seq;
  toolUseId: string;
  name: string;
  input: unknown;
  result: { content: unknown; isError: boolean } | null;
  /** The turn the call started in (`tool_call_started`'s `turn_id`, and the snapshot's `turnId`). A
   *  result only ever arrives inside its own turn, so a `null` result whose turn is not the active
   *  one never will (`MessageList`'s `isAbandonedCall`). Absent only where nothing carried it. */
  turnId?: string;
  /** The saved prefix rule that answered this call's permission request
   *  (`Bash(git log *)`), when a rule and not the user did. Absent on every other call. */
  allowedByRule?: string;
  /** This `Write`'s card was raised over no file (see `PermissionRequestRecord`),
   *  OR this `Write` was answered by the acceptEdits fast path with no card
   *  raised at all, over a path that did not exist when the call started. Either way: a completed
   *  row reads "Creates a new file" rather than the overwrite warning. */
  createsFile?: boolean;
  /** The CLI's own prompt for this call was answered without a card -- "Claude Code
   *  asked — allowed in bypass" / "— allowed with your approval". Absent on every other call. */
  promptNote?: string;
  /** This `Write`/`Edit`/`NotebookEdit` was answered by the acceptEdits fast path
   *  (`agent::permission_policy`) with no card. Absent on every other call, a carded or
   *  rule-answered one (`allowedByRule`) included -- the two are mutually exclusive, since no rule
   *  ever fires for these three tools at all. */
  allowedByAuto?: boolean;
  /** The CLI itself refused this call (Rust's `ToolCallDenial`, from a `permission_denied` event):
   *  its auto-mode classifier most often, whose `reasonType` is `"classifier"`. Both strings are the
   *  CLI's own, shown and never parsed; either may be `null`. Absent on every call nobody refused. */
  denied?: ToolCallDenial;
  /** How the input the CLI's own prompt for this call carried differs from the one the gate judged
   *  for the same `toolUseId` (Rust's `ToolCallRecord.changed_input`, set by a `tool_input_changed`
   *  event that Rust derives; the page compares nothing): something after the gate, such as a loaded
   *  settings tier's hook, changed it. Absent when the two agree and whenever no comparison could be
   *  made -- absence never says "same". */
  changedInput?: InputChange;
};
/** Rust's `agent::InputChange`: only the top-level fields that differ, with the CLI's values, already
 *  bounded by Rust (`removed` and `clipped` are absent when empty / false). */
export type InputChange = { fields: Record<string, unknown>; removed?: string[]; clipped?: boolean };
export type ToolCallDenial = { reasonType: string | null; reason: string | null };
/** `toolUseId` is the link back to the `ToolCallRecord` this request gates -- the same id that
 * call is keyed on.
 *
 * Genuinely nullable rather than optional. Both backends do send a real id on the paths that
 * actually gate tools -- the sidecar's proto `PermissionRequested` since 2026-09-10, and the
 * legacy backend's `PreToolUse` hook relay since 2026-09-15 -- so `null` is the uncommon case
 * rather than the normal one. It is still a case: the Rust side forwards only an id its source
 * message really carried and substitutes nothing when there is none (`agent/src/session.rs`'s
 * `permission_requested_event`), and an empty string is normalised to absent before it gets here.
 *
 * So `null` means "this request arrived with no usable link", never "this backend cannot supply
 * one". Render it honestly -- never default it, never guess at the most recent call. */
export type PermissionRequestRecord = {
  seq: Seq;
  permissionId: string;
  toolUseId: string | null;
  toolName: string;
  input: unknown;
  /** A `Write` whose file did not exist when the card was raised (Rust looked).
   *  Absent when it did, and for every other tool. */
  createsFile?: boolean;
  /** Present when the CLI itself asked about this call, after the gate had already answered it
   *  (Verdandi's PERMISSION_ORIGIN_PROVIDER_PROMPT; Rust's `ProviderPrompt`). Absent on the gate's
   *  own request. Answered like any card, with the same permission id. */
  providerPrompt?: ProviderPrompt;
  /** Set on a card Eitri's own tool broker raised (`editorCards`), never on a provider's request: the title and
   *  lines it shows. `toolName` is the broker tool's name and `input` is `null`; there is no provider behind it. */
  editorCard?: EditorCardView;
};
/** One line of a broker card by what it is. Every kind is plain text: the lines may come from a repository or the
 *  agent, so they are drawn through `revealHidden`, never as Markdown. */
export type EditorCardLine = { kind: "plain" | "code" | "agent" | "warning"; text: string };
/** The proposal a card would apply: its id in the tab, and the project-relative path of each file it changes, in the
 *  order `editor_card_view` numbers them. */
export type EditorCardProposal = { id: string; files: string[] };
export type EditorCardView = { title: string; body: EditorCardLine[]; proposal?: EditorCardProposal };
/** A broker card as Rust serialises it (`EditorCardWire`): `id` is the card's own id (`ec-<hex>-<n>`), `seq` the
 *  tab's revision when it was raised. */
export type EditorCardWire = {
  id: string;
  seq: Seq;
  tool: string;
  toolUseId: string | null;
  title: string;
  body: EditorCardLine[];
  /** Present on a card that would apply a proposal (`apply_proposal`). */
  proposal?: EditorCardProposal;
};
/** The input of a Codex `commandExecution` card, as the Codex provider writes it (`approvals.rs`,
 *  `CardKind::input`). `command` and `cwd` are `null` for a network request, which names a host and no
 *  command. `escalation` is Rust's own wording of how far the command reaches; `network` is set for the
 *  network form. `subagent` is the task name of a subagent's thread, absent for the tab's own. */
export type CodexCommandInput = {
  command: string | null;
  cwd: string | null;
  escalation: string;
  network: { host: string; protocol: string } | null;
  reason: string | null;
  subagent?: string;
};
/** One path of a Codex `fileChange` card or row. `diff` is a unified diff for `update`, and the whole
 *  content for `add` and `delete`; `kind` is Codex's own word for a kind this build does not know. */
export type CodexFileChange = { path: string; kind: string; diff: string; movePath?: string };
export type CodexFileChangeInput = { changes: CodexFileChange[]; reason?: string | null; subagent?: string };
export type CodexMcpInput = { server: string; tool: string; arguments: unknown; subagent?: string };
/** The input of an ACP agent's card (`acp:<kind>`): what the agent called the call, and its own permission
 *  options with their `kind` (`allow_once`, `reject_once`, ...) as the agent claimed them. */
export type AcpCardInput = {
  title?: string | null;
  rawInput?: unknown;
  locations?: Array<{ path: string }>;
  diffs?: Array<{ path: string; oldText?: string | null; newText?: string }>;
  options?: Array<{ name: string; kind: string }>;
  /** Set by Rust when the card cannot show the whole call; it then leaves `allow_once` out of `options`. */
  inputCut?: boolean;
  reason?: string | null;
  subagent?: string;
};
/** The CLI's own words about why it asked, verbatim; any of them may be `null`. `reason` is English
 *  prose from the CLI ("... which is a sensitive file.") -- shown, never parsed. */
export type ProviderPrompt = {
  reason: string | null;
  description: string | null;
  blockedPath: string | null;
  /** The user's own `permissions.ask` rule that forced this prompt, when one did. */
  matchedAskRule: MatchedAskRule | null;
  /** The raw `origin` a sidecar newer than this build sent ; `null` for a known one.
   *  Such a prompt is a card in every mode and is called neutrally. */
  unrecognizedOrigin: number | null;
};
export type MatchedAskRule = { source: string; toolName: string; ruleContent: string | null };
/** `ProviderPrompt` as the `permission_requested` EVENT carries it: Rust's own snake_case. */
export type WireProviderPrompt = {
  reason: string | null;
  description: string | null;
  blocked_path: string | null;
  matched_ask_rule: { source: string; tool_name: string; rule_content: string | null } | null;
  /** Omitted by Rust for every origin this build knows. */
  unrecognized_origin?: number;
};
export type SessionStatus =
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "unavailable"; reason: string }
  | { kind: "closed"; reason: string };

/** Which Claude backend a session runs on; `external` for a tab running another agent (a snapshot only:
 * the handshake names the window's Claude backend). */
export type BackendKind = "legacy" | "sidecar" | "external";

/** A previous conversation offered for continuation. The key is the CLAUDE session id -- the only
 * identity that survives a resume. Resuming mints a new Verdandi session id, so storing that one
 * would point at something that stops existing the moment it is used.
 *
 * **This is the whole of it.** There is no title, no first prompt, no message or turn count, and no
 * model -- Rust's `agent::ConversationRecord` has never stored any of them, and the one file on disk
 * that could supply a subject line (the Claude CLI's own transcript) is deliberately never read for
 * its content. A picker built on this can say WHICH session and WHEN. Anything that renders a row
 * must not manufacture a label the data cannot support.
 *
 * **Correction (2026-09-19): there is a title now** (`title` below), recorded by this project when
 * the session's first prompt is sent -- still not read out of anyone else's file. Everything else
 * above stands, and so does the last sentence, for the rows that have none. */
export type ResumableSession = {
  provider: string;
  providerSessionId: string;
  /** Epoch milliseconds as a string. When this conversation FIRST started, preserved across
   * resumes. */
  createdAt: string;
  /** Epoch milliseconds as a string. When this session was last STARTED OR RESUMED -- not when it
   * last had activity. Nothing rewrites a record during a conversation, so a session used for an
   * hour and one opened and abandoned carry the same stamp. Never label this "last active". */
  updatedAt: string;
  /** The first line of the prompt that began this session, as typed, cut to 80 characters (Rust's
   *  `agent::persistence::title_from_prompt`). `null` -- or absent, from a build before this field
   *  -- for a session recorded before titles were kept; such a row still shows only its id and time,
   *  and nothing may be made up in its place. */
  title?: string | null;
  /** The tab's rename, kept on the record so it outlives the tab; the chooser shows it ahead of the title. */
  name?: string | null;
};
/** A tab's permission mode, Eitri's own (`SessionModeChoice` in Rust), in `Shift+Tab`'s order: manual
 *  (Eitri's policy answers, every file edit asks), auto, bypass. */
export type PermissionModeChoice = "manual" | "auto" | "bypass";

/** The command that continues a conversation Eitri has just closed, in the user's own terminal.
 *
 * Arrives once, in a `handoff` envelope, and only AFTER the real session shutdown has finished —
 * `panel/src/handoffs.rs`'s `collect_pending_handoffs` dispatches it there and nowhere else, so nobody can
 * be looking at this line while Eitri is still driving the session.
 *
 * `command` is a ready-to-paste POSIX-shell line (`cd <dir> && claude --resume <id>`), built by
 * Rust from the same argv the supported `eitri-claude-handoff` wrapper would `exec`. `cwd` and
 * `providerSessionId` are its own parts, sent so the panel can name them without re-parsing the
 * line it was given.
 *
 * **It carries no claim of exclusivity, and none may be added here.** Nothing was spawned and no
 * lease was taken on this path. */
export type HandoffCommand = {
  command: string;
  cwd: string;
  providerSessionId: string;
};

/** What the provider said about a resume. `attached` is the late verdict -- the provider only
 *  reports its session id at the start of a turn -- while the two failures arrive promptly. */
export type ResumeStatus = "attached" | "rejected" | "initialization_failed";

/** What the provider can actually do, as the server advertised it, intersected with what this
 * client implements. Never inferred from a name or from an enum member existing. */
export type Capabilities = {
  resume: boolean;
  fork: boolean;
  interrupt: boolean;
  bypassPermissionMode: boolean;
  /** Enter while a turn runs puts the message into that turn (the agent reads it at its next step);
   *  without it the message is held for the turn's end. Optional only for callers written before it. */
  steer?: boolean;
  backgroundTasks?: boolean;
  taskControl?: boolean;
  taskOutput?: boolean;
  taskOutputRange?: boolean;
  /** The model and the effort change directly, also during a turn; without it, only by the CLI's slash commands. */
  directSettings?: boolean;
  workflowProgress?: boolean;
};

/** What the CLI says it will use. Each is empty when it did not say. */
export type AppliedSettings = { model: string; effort: string };

/** One effort level an agent that lists them apart from its models offers (an ACP agent). Every word is the agent's. */
export type EffortChoice = { value: string; displayName: string; description: string };

/** One model the session's CLI offers. Every word is the CLI's. */
export type ModelOption = {
  value: string;
  resolvedModel: string;
  displayName: string;
  description: string;
  supportsEffort: boolean;
  supportedEffortLevels: string[];
  supportsAutoMode: boolean;
};

/** Descriptive only -- for display and diagnostics, never for deciding whether a control is
 * available. That decision belongs to `Capabilities`. */
export type ProviderInfo = {
  sidecarVersion: string;
  claudeAgentSdkVersion: string;
  claudeCodeVersion: string;
  protocol: string;
  /** Which build of the provider this is (Verdandi checkout and revision). Descriptive and
   * essentially always present -- deliberately NOT a diagnostic. */
  buildDescription: string | null;
  /** Only things worth warning about: an untested CLI version, a condition the CLI reports about
   * itself. **Empty is the normal case**, which is what makes non-empty a real
   * signal worth putting a glyph on. */
  startupDiagnostics: string[];
};

/** Which of the two records a restored history came from. The panel says which, because the two
 * can genuinely differ: Claude's own transcript is what `claude --resume` in a terminal would show,
 * while Eitri's copy is this side's unilateral record of the same session. */
export type HistorySource = "claude_transcript" | "eitri_copy";

/** One statement about the history a resumed session was seeded with.
 *
 * **Not a timeline item**: it holds no `seq`, never reaches `buildTimeline`, and must never be
 * rendered as a conversation row -- row indices are counted across the whole panel, so an extra row
 * would shift every cursor index. It is one fixed line at the top of the list.
 *
 * `uptoSeq` is the one field here that is not about the notice at all: it is the boundary between
 * what was restored and what this session produced. A tool call below it with no result can never
 * complete, so rendering it as `Running…` would be a spinner on a process that ended days ago. */
export type HistoryNotice = {
  source: HistorySource;
  /** Rows this restore put in the list: prompts, assistant messages and tool calls, counted in Rust
   * AFTER the projection folded them, so it is what the panel draws rather than what the reader
   * parsed. Usually well below the 400-item ceiling, because the character budget binds first about
   * as often -- which is exactly why the notice prints this number and no longer prints the cap. */
  restoredItems: number;
  /** Items dropped by TRUNCATION, or `null` when records were certainly omitted and cannot be
   * counted (the scan started partway into a file too large to read whole). Never counts skipped
   * line types: those are not omitted conversation and stay in the Rust log. */
  omittedItems: number | null;
  /** Exclusive upper bound of the restored `seq` range. Every live item sits at or above it. */
  uptoSeq: Seq;
  /** The file this history was read from. Always non-empty -- a notice exists only when history was
   * restored, and history can only come from one file. */
  sourcePath: string;
  /** The Claude transcript that was looked for and not used. `null` when `source` is
   * `claude_transcript`, and when no path could be built at all. */
  attemptedTranscriptPath: string | null;
  /** Why Claude's own transcript was not used. Non-null exactly when `source` is `eitri_copy`. */
  fallbackReason: string | null;
  /** The CLI release that wrote the transcript. Never a schema version, and nothing branches on
   * it. `null` on the `eitri_copy` path, which has no such concept. */
  writerVersion: string | null;
};

/** One message typed while a turn ran as it reaches the panel: what the
 * user typed, when (epoch ms), and whether it was sent into the running turn and the agent has not
 * read it yet (`steered`, a `↪` line) or waits for the turn's end (a `⧗` line). The wire text composed
 * against the editor context, and the id it was sent under, are Rust's own concern and never reach here.
 * `steered` is optional only for callers written before it existed; Rust always sends it. */
export type QueueItem = { text: string; queuedAt: number; steered?: boolean };
/** V1's editor-context line the file relative to the project root (or
 * absolute outside it), and the selected line range when there is a selection. */
/** Where the panel stands with the editor beside it, in a companion window (`editor_link`,
 *  `serialize_editor_link_for_js`, `core/src/agent_bridge.rs`). `text` is the band's own words and is
 *  empty when `state` is `attached`. */
export type EditorLinkState = "none" | "attaching" | "attached" | "detached" | "failed";
export type EditorLink = { state: EditorLinkState; text: string };

export type ContextSummary = { file: string | null; lines: [number, number] | null };

export type AgentUiState = {
  backend: BackendKind;
  /** Three identities, deliberately never collapsed into one field.
   *  conversationId    -- Eitri's own, stable for a workspace (null on the legacy backend)
   *  sessionId         -- Verdandi's, what every RPC is addressed with
   *  providerSessionId -- Claude's, what `claude --resume <id>` takes (null until the provider
   *                       reports it, which is why a session that never took a turn has none) */
  conversationId: string | null;
  sessionId: string | null;
  providerSessionId: string | null;
  model: string | null; cwd: string | null;
  /** The effort the agent last said this session runs at (`effort_reported`). `null` is not known, which on a Claude
   *  tab is the usual state: its wire carries none, and only the CLI's own `/model` and `/effort` replies say it. */
  effort: string | null;
  userPrompts: UserPromptRecord[];
  sentFrom?: Readonly<Record<string, SentFrom>>;
  stoppedFrom?: Readonly<Record<string, SentFrom>>;
  transcript: TranscriptMessage[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  activeTurnId: string | null; pendingPermissions: PermissionRequestRecord[];
  /** The cards Eitri's own tool broker is waiting on, as records the card view draws like a provider's. A snapshot
   *  carries them (`editorCards`, wire shape) and the `editor_cards` envelope replaces them whole. They are never
   *  in `pendingPermissions` and never move `nextSeq`: Rust numbers them from the tab's revision without taking one. */
  editorCards: PermissionRequestRecord[];
  capabilities: Capabilities;
  provider: ProviderInfo | null;
  /** The last usage this tab's provider reported, or `null` before it has (what the band's usage
   * segment reads). A turn that reports one replaces it whole -- a running total, so a lower figure is
   * the truth after `/clear` -- and a turn that reports none leaves it (`reducer.ts`). On the wire: a
   * snapshot carries it (an explicit `null` when nothing was reported), so a tab switch or a panel
   * reload shows the figure without waiting for the next turn. A resumed tab shows none until its
   * first turn after the resume, because history seeding reports no usage. */
  usage: UsageInfo | null;
  /** The account limit the agent last reported, or `null` before it has. Folded from `account_limits`
   *  events, and carried by a snapshot (an explicit `null` before any report), so a reload or a tab
   *  switch shows it without waiting for the next report. */
  accountLimits: AccountLimits | null;
  /** How many commands the agent left running after its turns ended (`background_calls`): the band counts
   *  them while there are any, and `<leader>k` stops them with the session. `0` for every agent that keeps
   *  no such count. A snapshot carries it. */
  backgroundCalls: number;
  /** The slash commands the tab's agent listed (`commands_available`; an ACP agent's
   *  `available_commands_update`), or `null` before any list. The `/` menu offers them in place of
   *  Claude's built-in table (`completionSource`). Untrusted text from the agent: bounded on the way in
   *  and drawn as plain text. A snapshot carries it. */
  availableCommands: CompletionCommand[] | null;
  /** The turns that ended without completing, oldest first: failed, hit a limit, interrupted, or cut off
   *  by the session ending. Each is a muted row where the turn stopped. A turn that completed adds none. */
  turnEndings: TurnEnding[];
  /** How the latest turn ended while it is still the latest news -- the band says it until the next turn
   *  starts. `null` before any turn ends badly, after a completed one's start, and from `turn_started` on. */
  lastTurnEnding: TurnEndingKind | null;
  /** What this session was seeded with before it produced anything of its own, or `null` when it
   * restored nothing -- which is every fresh session. On the wire, so it survives a resync and a
   * panel reload exactly as the four collections do. */
  history: HistoryNotice | null;
  /** True while the last folded event was assistant text, so the next chunk continues the same
   * message. See `reducer.ts`'s `content_delta` case.
   *
   * **On the wire since the fix of 2026-09-27.** It used to be reducer-internal and
   * `applySnapshot` forced it to `false` regardless of what a snapshot said, on the theory that a
   * snapshot always meant "start fresh" -- but Rust's own `AgentSessionProjection` keeps this bit
   * (`assistant_message_open`) and keeps appending to the open message underneath a snapshot taken
   * mid-reply, so the two sides disagreed about whether the last message was still open. Forcing
   * `false` split a streaming reply into two transcript rows -- with broken markdown at the seam --
   * on every snapshot landing mid-reply: a tab switch back to a streaming reply, `prefix r`, and a
   * bounded-queue Resync. `applySnapshot` now takes this from the snapshot like every other field. */
  assistantMessageOpen: boolean;
  /** The agent's thinking, oldest first (Rust's `thinking`). A snapshot carries it like the other
   * collections. */
  thinking: ThinkingBlock[];
  /** The starts of turns the user did not send, each where it started (Rust's `turn_starts`). A snapshot
   *  carries them while there are any. */
  turnStarts: TurnStartRecord[];
  /** The CLI's notice rows (Rust's `notices`). A snapshot carries them while there are any. */
  notices: NoticeRecord[];
  apiRetry: NoticeRecord | null;
  compacting: boolean;
  rateLimitWarning: NoticeRecord | null;
  fallbackModel: string | null;
  notification: NoticeRecord | null;
  backgroundTasks: TaskRoster;
  /** Every question set Claude asked, waiting or settled (Rust's `questions`). A snapshot carries them
   *  while there are any. */
  questions: QuestionRecord[];
  /** Claude Code is working where this panel cannot show it (Rust's `cli_unseen_busy`): set by
   *  `cli_work_unseen`, cleared by the next `turn_completed` whichever turn it names or by the
   *  session's end (`session_closed`, `session_unavailable`, a `resume_outcome` that did not attach).
   *  A snapshot carries it only while it is set. */
  cliUnseenBusy: boolean;
  /** Page-only: the number of the stretch of unseen work under way while `cliUnseenBusy` is set (of the last one
   *  otherwise). The reducer raises it when `cliUnseenBusy` goes from clear to set and `applySnapshot` on every
   *  snapshot (a snapshot cannot say whether the work it shows is the stretch the page knew). Every state the page
   *  model puts in place keeps it at least as high as the one it replaces (`keepUnseenEpisode` in `page_model.ts`:
   *  a tab switch, a restart in place, a handoff, an error), so it never goes down while the page lives and a stop
   *  question asked about one stretch (`workToStop`) does not answer for a later one. */
  cliUnseenEpisode: number;
  /** The `seq` of the prompt the running turn opened with (or the turn about to start will), while it
   *  is still a row of the conversation (Rust's `opening_prompt`): `cli_cycle_started` moves that row out.
   *  A snapshot carries it only while it is set. */
  openingPromptSeq: Seq | null;
  /** The running turn is one Claude Code started by itself (Rust's `running_turn_cli`). A snapshot
   *  carries it only while it is set. */
  runningTurnCli: boolean;
  /** True while the last thinking block is still being written: the next thinking delta continues it.
   * Closed by assistant text and by everything that closes `assistantMessageOpen`. On the wire, as
   * `assistantMessageOpen` is and for the same reason: a snapshot taken mid-thought must neither split
   * the thought nor draw it as finished. */
  thinkingOpen: boolean;
  /** Reducer-internal, never on the wire: the `seq` the next locally-folded item will take.
   *
   * Seeded from a snapshot's own `throughRevision`, which Rust guarantees is strictly greater than
   * every `seq` inside that snapshot -- so an item folded after a snapshot sorts after everything
   * the snapshot carried, and cannot collide with one of them. Incremented once per `applyEvent`
   * call, exactly as `AgentSessionProjection::apply` bumps `last_revision`, so on the healthy path
   * the numbers are the same numbers Rust would have assigned rather than a parallel scheme.
   *
   * Every event advances it, `permission_mode_changed` included (it changes nothing else here, but Rust
   * bumps `last_revision` for it): a turn's footer is placed by a seq Rust names, so a counter one
   * behind would put it under the wrong row. */
  nextSeq: Seq;
  /** Reducer-internal, never on the wire: true from a `content_delta` with `kind: "thinking"` until
   * the next event of any other kind. The one ephemeral bit `turnPhase.ts`'s `phaseOf` reads that is
   * not already sitting in the four collections above. **Invariant: can only ever be LOST, never invented.** Every way of losing it (a resync, a
   * reload, any other event) degrades the phase it feeds toward `sent`/`replying`/`tool` -- less
   * specific, never wrong -- because it is cleared by the ABSENCE of a special case in `applyEvent`
   * rather than by a maintained list of event types that ought to clear it. Nothing may set it to
   * `true` anywhere but that one arm. */
  turnThinking: boolean;
  /** Reducer-internal, never on the wire (Rust's `denials_before_their_call`): refusals by the CLI
   * whose tool call has not been folded yet, oldest first, attached when its `tool_call_started`
   * arrives. Emptied when the turn or the session ends, and capped at `MAX_HELD_DENIALS`. A snapshot
   * resets it: Rust does not serialize its own held refusals, so there is nothing to seed it from. */
  denialsBeforeTheirCall: ReadonlyArray<{ toolUseId: string; denied: ToolCallDenial }>;
  /** Reducer-internal (Rust's `changed_inputs_before_their_call`): a changed input whose call has not
   * started yet, attached when `tool_call_started` arrives. */
  changedInputsBeforeTheirCall: ReadonlyArray<{ toolUseId: string; change: InputChange }>;
};

/** A snapshot as it ACTUALLY arrives from Rust, which is not an `AgentUiState`.
 *
 * `serialize_snapshot_for_js` emits neither `nextSeq` nor `turnThinking` -- both are
 * reducer-internal and Rust has no key for either. Typing the inbound envelope as a full
 * `AgentUiState` asserted both were present and `number`/`boolean` when both are `undefined` at
 * runtime; `applySnapshot` overrides them immediately so nothing broke, but anything that read
 * `payload.state.nextSeq` before that -- a resync-diffing path, say, which is exactly the kind of
 * thing this area attracts -- would have got `undefined` with the compiler insisting on a number.
 * Subtracting them is the honest shape: what is missing is now missing in the type too.
 *
 * `assistantMessageOpen` is deliberately NOT in this `Omit` (2026-09-27):
 * it used to be, on the theory that it was reducer-internal like the other two, but Rust's own
 * projection carries the same bit and disagreeing about it split a mid-stream reply across a
 * snapshot -- see that field's own doc comment on `AgentUiState`. */
export type AgentUiSnapshot = Omit<
  AgentUiState,
  | "nextSeq"
  | "turnThinking"
  | "cliUnseenEpisode"
  | "denialsBeforeTheirCall"
  | "changedInputsBeforeTheirCall"
  | "thinking"
  | "thinkingOpen"
  | "turnStarts"
  | "notices"
  | "apiRetry"
  | "compacting"
  | "rateLimitWarning"
  | "fallbackModel"
  | "effort"
  | "notification"
  | "backgroundTasks"
  | "questions"
  | "cliUnseenBusy"
  | "openingPromptSeq"
  | "runningTurnCli"
  | "editorCards"
> & {
  /** Sent only while the tab holds any: a payload without it is a tab with none. Rust sends the wire shape; records
   *  are accepted too, so a caller holding a folded state can hand it to `applySnapshot` unchanged. */
  editorCards?: Array<EditorCardWire | PermissionRequestRecord>;
  /** Sent only while there are any: a payload without them is a conversation with none. */
  turnStarts?: AgentUiState["turnStarts"];
  notices?: AgentUiState["notices"];
  apiRetry?: NoticeRecord | null;
  compacting?: boolean;
  rateLimitWarning?: NoticeRecord | null;
  fallbackModel?: string | null;
  effort?: string | null;
  notification?: NoticeRecord | null;
  backgroundTasks?: TaskRoster;
  questions?: AgentUiState["questions"];
  /** Sent only while it is set. */
  cliUnseenBusy?: boolean;
  /** Sent only while set; a page's own state carries `null` for none. */
  openingPromptSeq?: Seq | null;
  /** Sent only while set. */
  runningTurnCli?: boolean;
  /** Present only while Rust holds a change whose call has not started: it is the one held list
   *  Rust sends, so a rebuilt page attaches it when the call starts, as Rust does. */
  changedInputsBeforeTheirCall?: AgentUiState["changedInputsBeforeTheirCall"];
  /** Rust always sends both; optional so a payload recorded before thinking was kept still reads, as a
   *  conversation with none. */
  thinking?: AgentUiState["thinking"];
  thinkingOpen?: boolean;
};

/** How long a turn has been running, tracked in `App.tsx` alongside `AgentUiState` rather than
 * inside it: it is a UI-local clock, not part of the projection Rust serializes, and it does not
 * survive a page reload the way `AgentUiState` (rebuilt from a snapshot) does.
 *
 * `exact` is `true` only when this record was minted from a real `turn_started` event; a turn id
 * first observed inside a `snapshot` envelope (a page reload, or a resync mid-turn) gets
 * `exact: false`, because this panel cannot know how long the turn had already been running before
 * it first saw it. Rendered `12s` when exact, `12s+` -- "at least this long" -- when not. */
export type TurnClock = { turnId: string; since: number; exact: boolean };

/** The handshake reply, before any session exists: which backend is behind the bridge and what it
 * genuinely offers. The start screen renders from this rather than hardcoding either backend's
 * shape. */
/** What the launch dashboard may bring back: the tabs the last window on this project had open, in
 *  tab-bar order, and how many of those were in bypass. Rust sends it only while this window has
 *  started nothing and at least one saved tab can still be resumed, so its presence is the whole
 *  reason to draw the line. */
export type RestoreOffer = {
  labels: string[];
  bypass: number;
  /** What became of a turn running when the window ended, while it is offered; Rust words it. */
  cutNote?: string | null;
};

export type Hello = {
  canChooseProject?: boolean;
  backend: BackendKind;
  projectDir: string;
  /** Only modes with genuinely distinct runtime behavior, and deliberately NOT narrowed by which
   * backend is behind the bridge -- both have a real, separately verified interactive gate, so both
   * currently offer the same two. (`verdandi_rules` is absent because it is confirmed to behave
   * identically to `interactive` in the current sidecar; a third button that does nothing different
   * would be a worse lie than a missing one.) */
  permissionModes: PermissionModeChoice[];
  /** Every previous conversation in this workspace worth offering, newest first.
   *
   * An entry exists only when ALL THREE hold for it: the provider advertised resume, this client
   * implements it, and this workspace has that persisted provider session id. The conversation
   * picker renders on exactly this array and nothing else, so no row can appear for a workspace
   * with nothing to continue. Empty is the normal case for a fresh workspace, and is also what the
   * legacy backend always sends -- it has no resume, and writes no records either.
   *
   * Rust ranks this; the frontend renders it in array order and sorts nothing. */
  resumableSessions: ResumableSession[];
  /** `agent::account`'s configured name, `null` when none is set (`serialize_hello_for_js`'s
   *  own `"account"` field). Read by the empty tab's dashboard
   *  (its cwd/backend/account line) and by nothing else yet. */
  account: string | null;
  /** The saved tabs the dashboard offers back (`restore_last`), or `null` when there is nothing to
   *  offer. Optional only so that a sender that predates it reads as "nothing". */
  restore?: RestoreOffer | null;
};

export type TurnOutcome = "completed" | "interrupted" | "failed" | "limit_reached";

/** What a turn that did not complete carries beyond its outcome, as `turn_completed.detail` (snake_case,
 *  as serde derives it). `message` is the provider's own words, already trimmed and capped by Rust, and is
 *  the only place a reset time the CLI wrote into its text can appear: nothing here parses it. */
export type TurnEndDetail = { origin_kind?: string; num_turns?: number; reason: string | null; api_error_status: number | null; message: string | null };

/** How a turn ended without completing: its outcome, or `lost` when the session ended under a turn that
 *  was still running (no `turn_completed` was ever coming). */
export type TurnEndingKind = "interrupted" | "failed" | "limit_reached" | "lost";

/** One turn-ending row of the conversation. Ordered by `seq` with the other items; on the wire in a
 *  snapshot's `turnEndings` (camelCase, like the other collections). */
export type TurnEnding = {
  originKind?: string;
  numTurns?: number;
  seq: Seq;
  turnId: string;
  kind: TurnEndingKind;
  reason: string | null;
  apiErrorStatus: number | null;
  message: string | null;
  /** When the account's usage window resets as of this ending (Unix seconds): the latest limit report's when the
   *  turn ended, else the first report's before the next turn starts. Kept with the row, so a later report never
   *  changes it. Absent from a build that predates it. */
  resetsAt?: number | null;
};

/** How long one finished turn ran, as Rust's tab set stamped it (`turnTimes` on a snapshot or events
 *  envelope; `core/src/agent_bridge.rs`'s `TurnTime`). Times are `Date.now()`'s clock. `anchorSeq` is the
 *  `seq` of the newest row when the end was seen, or `null` when Rust could not tell which row that was. */
export type TurnTimeWire = { turnId: string; startedAtMs: number; endedAtMs: number; anchorSeq: Seq | null };

/** The token counts of one usage report, as Verdandi's `TurnUsage` carries them (Rust's
 *  `agent::TokenUsage`, snake_case as serde derives it). The prompt is three figures, not one: Claude
 *  Code caches aggressively, so on a large prompt `input` is typically single digits and nearly all of
 *  it is `cache_creation` or `cache_read`. What the session moved is the sum of all four. */
export type TokenUsage = { input: number; output: number; cache_creation: number; cache_read: number };

/** What a provider reported about a session's spend so far. Mirrors Rust's `agent::UsageInfo`, and the
 *  snake_case field names are deliberate -- `AgentDomainEvent` reaches the frontend through serde's own
 *  derive, and a snapshot's `usage` (`AgentUiState.usage`) carries this same object rather than
 *  `serialize_snapshot_for_js`'s camelCase re-shaping, so a live report and a reload read one type.
 *
 *  **A running total, not one turn's spend**: the SDK's result carries the session so far, a
 *  mid-session `/clear` resets it, and a resumed session starts fresh -- so a later report replaces an
 *  earlier one whole, even a lower one, and is never added to it (`reducer.ts`).
 *
 *  Each backend fills the half it really has. The sidecar (Verdandi's `TurnUsage`, capability
 *  `turn_usage`) reports `tokens` and `model` and has no turn count; legacy's terminal `result` line
 *  reports `num_turns` and reads no token fields. A `null` slot is unknown, never zero.
 *
 *  `turn_completed.usage` is itself `null` when the provider reported nothing for that turn -- an
 *  interrupted or synthesized one, or a sidecar that predates `TurnUsage` -- which is NOT a turn that
 *  cost nothing: it leaves the last report standing (`applyEvent`). Anything that renders this must show
 *  `null` as unknown. Showing it as $0.00 would reinstate the bug that made the field nullable (this
 *  event once arrived with `total_cost_usd: 0, num_turns: 0` hardcoded on the sidecar, so a cost readout
 *  was right on `legacy` and confidently, permanently wrong there). `band.ts`'s `usageSegment` is the one
 *  reader, and draws nothing for `null`. */
export type UsageInfo = {
  total_cost_usd: number;
  /** `false` when the provider reported tokens but no cost (Codex): `total_cost_usd` is then a
   *  placeholder and a cost readout must show nothing rather than $0.00. Rust leaves the key out while
   *  it is true, so absent means a cost was reported. */
  cost_reported?: boolean;
  num_turns: number | null;
  tokens: TokenUsage | null;
  model: string | null;
};

/** The account's usage limit as the agent last reported it (`AgentDomainEvent::AccountLimits`, folded
 *  by the reducer into `AgentUiState.accountLimits`). Account-wide, not per turn. `resetsAt` is Unix
 *  seconds. A `null` slot is something the agent did not say. */
export type AccountLimits = { usedPercent: number; windowMinutes: number | null; resetsAt: number | null; plan: string | null };
export type TaskKind = "shell" | "monitor" | "local_agent" | "workflow" | "other";
export type TaskStatus = "running" | "paused" | "stopping" | "completed" | "failed" | "stopped" | "ended";
export type TaskEdge = "started" | "progress" | "updated" | "notified";
export type BackgroundTask = {
  task_id: string; run_id: string | null; kind: TaskKind; task_type: string; description: string;
  subagent_type: string | null; parent_task_id: string | null; tool_use_id: string | null; ambient: boolean; workflow_name: string | null;
};
export type TaskRoster = { rows: BackgroundTaskRecord[]; query_start: boolean };
export type BackgroundTaskRecord = BackgroundTask & {
  status: TaskStatus; live: boolean; started_at: number; ended_at: number | null; finished_seq: number | null;
  summary: string | null; last_tool_name: string | null; usage: TaskUsage | null; error: string | null;
  is_backgrounded: boolean | null; reason: string | null; output_file: string | null;
  workflow_progress: WorkflowProgress | null;
};
export type TaskUsage = { total_tokens: number; tool_uses: number; duration_ms: number };
export type WorkflowProgress = {
  phases: { index: number; title: string; kind: string }[];
  agents: { index: number; label: string; phase_index: number; state: string; model: string | null; agent_type: string | null;
    tokens: number | null; tool_calls: number | null; duration_ms: number | null; error: string | null }[];
};
export type SubagentContent = Extract<AgentDomainEvent, { type: "tool_call_started" | "tool_call_completed" | "content_delta" | "assistant_message_boundary" | "subagent_frame" }>;

export type AgentDomainEvent =
  | { type: "turn_cause_updated"; turn_id: string; cause: WireTurnCause | null }
  | { type: "cli_cycle_started"; turn_id: string; cause: WireTurnCause | null }
  | { type: "background_tasks_changed"; occurred_at?: number; tasks: BackgroundTask[] }
  | { type: "task_event"; occurred_at?: number; edge: TaskEdge; task_id: string; run_id: string | null; task: BackgroundTask | null; status: TaskStatus | null;
      summary: string | null; last_tool_name: string | null; usage: TaskUsage | null; end_time_ms: number | null; error: string | null;
      is_backgrounded: boolean | null; reason: string | null; output_file: string | null; workflow_progress: WorkflowProgress | null }
  | { type: "conversation_reset"; new_conversation_id: string; trigger: string | null }
  /** The host's own records of a tool it serves. Rust keeps them from every page; only a recorded twin
   *  holds them, and the reducer ignores them. */
  | { type: "session_servers"; servers: { name: string; status: string; source: string }[] }
  | { type: "client_tool_call"; call_id: string; tool_use_id: string | null; tool: string; arguments_json: string; sequence: number }
  | { type: "client_tool_call_settled"; call_id: string }
  | { type: "subagent_frame"; parent_tool_use_id: string; frame: SubagentContent }
  | { type: "session_opened"; session_id: string; provider_session_id: string; model: string; cwd: string }
  | { type: "effort_reported"; effort: string | null }
  | { type: "model_reported"; model: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "user_prompt_submitted"; text: string }
  /** A message put into the running turn was read by the agent: a user prompt at this point of the
   *  conversation (`AgentDomainEvent::UserMessageDelivered`). */
  | { type: "user_message_delivered"; client_message_id: string; turn_id: string; text: string }
  /** A message put into the running turn did not reach the agent; Rust holds it again and the queue
   *  envelope says why. Nothing in the conversation changes (`AgentDomainEvent::UserMessageDropped`). */
  | { type: "user_message_dropped"; client_message_id: string; reason: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  /** The streaming assistant message is over; the next text starts a new one. Mirrors
   *  `AgentDomainEvent::AssistantMessageBoundary`. Reachable on **both** backends: legacy emits it
   *  directly (`session.rs`), and the sidecar translates a *change* of a present
   *  `TextDelta.message_id` into one -- no id on the wire still means today's
   *  concatenation, and a replayed duplicate after a reconnect never re-splits. */
  | { type: "assistant_message_boundary"; turn_id: string }
  | { type: "tool_call_started"; turn_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_call_completed"; turn_id: string; tool_use_id: string; content: unknown; is_error: boolean }
  | {
      type: "permission_requested";
      permission_id: string;
      tool_use_id: string | null;
      tool_name: string;
      input: unknown;
      /** The CLI's own prompt; omitted by Rust on the gate's own request. */
      provider_prompt?: WireProviderPrompt;
    }
  | { type: "permission_resolved"; permission_id: string; outcome: "allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close" | "provider_failed" | "expired" | "deferred" }
  | {
      type: "turn_completed";
      turn_id: string;
      outcome: TurnOutcome;
      result_text: string;
      stop_reason: string | null;
      usage: UsageInfo | null;
      /** What the provider said about how the turn ended; Rust always sends it, every field `null`
       *  when there is nothing to say (see `TurnEndDetail`). */
      detail: TurnEndDetail;
    }
  /** The provider's verdict on a resume, stated once for a session that asked for one. Mirrors the
   *  Rust `AgentDomainEvent::ResumeOutcome`; the reducer must fold it the same way
   *  `AgentSessionProjection` does, or the two states diverge on a failed resume. */
  | {
      type: "resume_outcome";
      requested_provider_session_id: string;
      status: ResumeStatus;
      attached_provider_session_id: string | null;
      forked: boolean;
      detail: string | null;
    }
  | { type: "session_unavailable"; reason: string }
  | { type: "session_closed"; reason: string }
  /** The sidecar's own acknowledgement of a `set_permission_mode` RPC (a synchronous unary call issued on the GTK
   *  thread, never optimistic). `mode` is Eitri's own `PermissionModeChoice`; `provider_mode` is Verdandi's raw
   *  string (diagnostics only); `floor_applied` is `true` exactly when Verdandi's bypass floor
   *  (`usesDefaultBypassDeny`) still restricted the session (a deny list) despite `unrestricted: true` -- logged
   *  loudly, never silently. **The reducer ignores this event on purpose**: the mode a tab is in lives on the `tabs`
   *  envelope (`TabInfo.mode`), not on this per-session projection, so folding it here would just be a second,
   *  driftable copy. */
  | { type: "permission_mode_changed"; mode: PermissionModeChoice; provider_mode: string; floor_applied: boolean }
  /** What the CLI says its permission mode is, on a session that asked for its own auto mode
   *  (`AgentDomainEvent::CliPermissionMode`): read by Rust's answer path, nothing for the view. */
  | { type: "cli_permission_mode"; reported: string }
  /** The CLI refused a tool call on its own, before asking anyone (`AgentDomainEvent::PermissionDenied`).
   *  Lands on that call's row as `ToolCallRecord.denied`. */
  | {
      type: "permission_denied";
      tool_use_id: string | null;
      tool_name: string;
      reason_type: string | null;
      reason: string | null;
    }
  /** An agent that is not the Claude sidecar reported a sandbox, approval policy or reviewer other than
   *  the one Eitri asked for (`AgentDomainEvent::PolicyMismatch`). **The reducer ignores it on purpose**:
   *  Rust fails the tab, and the failure reaches the page as the tab's own state. */
  | { type: "policy_mismatch"; requested: string; reported: string }
  /** The account's usage limit changed (`AgentDomainEvent::AccountLimits`). The latest wins. */
  | { type: "account_limits"; used_percent: number; window_minutes: number | null; resets_at: number | null; plan: string | null }
  /** How many commands the agent left running after its turns ended (`AgentDomainEvent::BackgroundCalls`),
   *  whenever that changes. The latest wins. */
  | { type: "background_calls"; running: number }
  /** The slash commands the agent says the session accepts, whole (`AgentDomainEvent::CommandsAvailable`).
   *  The latest list replaces the earlier one. */
  | { type: "commands_available"; commands: CompletionCommand[] }
  /** The CLI's own prompt for a call carried another input than the gate was asked about. Rust
   *  derives it when it folds the prompt and sends it right after, even when the host answered the
   *  gate request itself and so never delivered it: the page applies it and compares nothing.
   *  Lands on the call's row as `ToolCallRecord.changedInput`, or is held until the call starts. */
  | { type: "tool_input_changed"; tool_use_id: string; change: InputChange }
  /** Who started the turn the `turn_started` before it named, and why (`AgentDomainEvent::TurnAttributed`).
   *  A row only for a turn whose origin is not `client`. */
  | { type: "turn_attributed"; turn_id: string; origin: TurnOrigin; cause: WireTurnCause | null }
  /** A notice the CLI's terminal shows as a row (`AgentDomainEvent::SessionNotice`). */
  | {
      type: "session_notice";
      kind: SessionNoticeKind;
      level: NoticeLevel;
      content: string | null;
      scalars: Record<string, unknown>;
      task_id: string | null;
      tool_use_id: string | null;
      task_status: string | null;
    }
  /** Claude asked a set of questions (`AgentDomainEvent::QuestionAsked`); it waits until its settlement. */
  | {
      type: "question_asked";
      interaction_id: string;
      tool_use_id: string | null;
      questions: WireQuestionSpec[];
      unsupported_reason: string | null;
    }
  /** A question set ended (`AgentDomainEvent::QuestionSettled`): the only thing that settles one. */
  | { type: "question_settled"; interaction_id: string; outcome: QuestionOutcome; answers: WireShownAnswer[]; reason: string | null }
  /** Claude Code is working where this panel cannot show it (`AgentDomainEvent::CliWorkUnseen`): sets
   *  `cliUnseenBusy` until the next `turn_completed` or the session's end. Never makes a turn run here. */
  | { type: "cli_work_unseen" }
  /** The newest prompt not put into a running turn is taken back (`AgentDomainEvent::UserPromptWithdrawn`):
   *  its send was refused and the message reaches the agent another way, where it is folded again. */
  | { type: "user_prompt_withdrawn" };

/** A session tab's identity for its whole life, and the bridge's (`eitri_core::tabs::TabId`). */
export type TabId = number;
/** `eitri_core::agent_bridge::TabStateWire`. `awaiting_trust`: a start is put off until the workspace-trust
 *  question is answered; no session is being opened, so it is not `starting`. */
export type TabState = "not_started" | "starting" | "awaiting_trust" | "live" | "ended" | "failed";
/** `eitri_core::tabs::Marker::wire()`. Precedence is Rust's: ⚑ > ✕ > working > •. */
export type TabMarker = "needs_input" | "ended" | "working" | "unread";
export type TabInfo = {
  following?: boolean;
  /** Opaque host mirrors, keyed by interaction id; the dock checks each draft before using it. */
  questionDraft?: Record<string, unknown>;
  id: TabId;
  number: number;
  /** `<n> <name>`, worded by Rust (`tabs::label`). */
  label: string;
  name: string | null;
  state: TabState;
  mode: PermissionModeChoice;
  marker: TabMarker | null;
  pending: number;
  /** False on legacy: "not resumable". */
  resumable: boolean;
  failure: string | null;
  /** True for a `failed` tab whose session had been open (a tripwire, a fatal command): its failure row
   *  says the session stopped rather than that it did not start (`TabView::failed_after_start`). Rust
   *  always sends it; absent reads as false. */
  failedAfterStart?: boolean;
  /** True for an `ended` tab whose agent stopped its session with the conversation whole: `r` continues that
   *  session in the same tab instead of emptying it to a start screen (`TabView::resumes_on_r`). Rust always
   *  sends it; absent reads as false. */
  resumesOnR?: boolean;
  /** The record's title: the first prompt's title, or a resumed record's display title. `null`
   *  before one exists. Read by the chooser's open-tab rows (their second line) once they join a
   *  `ChooserTab` to its `TabInfo` here. */
  title: string | null;
  /** The agent the tab runs, as records name it: `claude`, `codex`, `acp:<name>` (`TabView::agent`). Rust
   *  always sends it; a tab without it reads as Claude's. */
  agent?: string;
};
export type TabsEnvelope = {
  active: TabId;
  tabs: TabInfo[];
  /** `TabSet::default_mode`: the mode a fresh tab,
   *  or a resume into a new tab, takes. The chooser derives its mode line from this rather than
   *  re-deriving it, so a cycle never needs the chooser re-sent. */
  defaultMode: PermissionModeChoice;
};
export type DetailRow = { label: string; value: string };
export type ChooserTab = { tab: TabId; label: string; marker: TabMarker | null; pending: number; resumable: boolean };
export type ChooserRecord = {
  providerSessionId: string;
  name: string | null;
  title: string | null;
  createdAt: string;
  updatedAt: string;
  heldElsewhere: boolean;
  heldByPartner?: boolean;
  partnerWindow?: number | null;
};
export type ChooserEnvelope = {
  open: ChooserTab[];
  records: ChooserRecord[];
  /** How many sessions a project keeps, present once the project holds that many: the oldest are dropped as new ones start. */
  limit?: number;
};

/** Turn review (the `review`, `review_diff` and `review_hint` envelopes; `core/src/agent_bridge.rs`).
 *  Every attribution, count and state is decided in Rust; this side only draws what it is told. */
export type ReviewScope = "turn" | "session";
export type ReviewHintLevel = "off" | "key" | "full";
/** The window's turn review settings (the `review_settings` envelope). `enabled`: a review is installed,
 *  so `c` can open one. `hint`: no pointer, just the key, or the key with finished and running counts.
 *  With no envelope the page stays disabled with the hint off. */
export type ReviewSettings = { enabled: boolean; hint: ReviewHintLevel };
export const REVIEW_SETTINGS_OFF: ReviewSettings = { enabled: false, hint: "off" };
/** `no_baseline`: no snapshot of the turn's start exists, so nothing can be compared. `unfinished`: Eitri
 *  exited mid-turn, so the review compares the base with the disk as it is now. `pending`: a snapshot
 *  of this turn is still being taken. */
export type ReviewTurnState = "ok" | "no_baseline" | "unfinished" | "pending";
/** `agent`: a call of this tab named the file and it changed (`✓`). `agent_only`: named, unchanged
 *  (`·`). `workspace`: changed but no call of this tab named it (`?`). */
export type ReviewOrigin = "agent" | "agent_only" | "workspace";
export type ReviewTurn = {
  n: number;
  turnId: string;
  /** Epoch milliseconds. */
  startedAt: number;
  /** `null` while the turn has not ended. */
  endedAt: number | null;
  state: ReviewTurnState;
  /** A tool call reached the host before the turn's baseline snapshot was done. */
  late: boolean;
  overlappedNext: boolean;
  overlappedTab: boolean;
  /** Why `state` is `no_baseline` or `unfinished`. */
  reason: string | null;
};
export type ReviewFile = {
  path: string;
  added: number;
  removed: number;
  origin: ReviewOrigin;
  binary: boolean;
  tooLarge: boolean;
  nested: boolean;
};
export type ReviewEnvelope = {
  requestId: string;
  tab: TabId;
  scope: ReviewScope;
  /** The turn number shown (the newest one, in session scope). */
  current: number;
  turns: ReviewTurn[];
  files: ReviewFile[];
  /** `false`: nothing was compared (no baseline, or one still being taken), so an empty `files` says
   *  nothing about what changed and the list is not drawn. */
  compared: boolean;
  /** File-changing calls of this turn that have no result. */
  pendingNoResult: number;
  /** Every line the review says about itself (its heading, a late or missing baseline, overlaps, a turn
   *  still running), worded by Rust. The overlay draws these and derives none from the turn flags. */
  notes: string[];
  /** The tab's draft of comments and reverts. The only source the overlay draws it from, with
   *  `review_draft`: nothing is kept in the page across a reload. Optional only so an envelope recorded
   *  before the draft existed still draws, as an empty one. */
  draft?: ReviewDraft;
};
/** One comment of the draft, by hunk-independent new-side line range; `anchor` is the quoted text. */
export type ReviewComment = { id: number; turn: number; path: string; from: number; to: number; anchor: string[]; text: string };
/** One revert of the draft. A whole-file revert has no `hunk`/`header`/`lines`. `source` says where it was
 *  made (`panel` or `editor`); `undone` that the change is back. */
export type ReviewRevert = {
  id: number;
  turn: number;
  path: string;
  hunk: number | null;
  header: string | null;
  what: "hunk" | "deleted" | "restored" | "file";
  lines: [number, number] | null;
  source: string;
  undone: boolean;
};
export type ReviewDraft = { comments: ReviewComment[]; reverts: ReviewRevert[]; canUndo: boolean };
/** The reply to `review_comment_add`/`review_comment_remove`, and pushed unasked (`requestId: null`) after a
 *  change the panel did not ask for. */
export type ReviewDraftEnvelope = { requestId: string | null; tab: TabId; draft: ReviewDraft };
/** One revert the draft records that is not on disk as it says. `lines` is set only for a hunk; a
 *  whole-file revert (`deleted`, `restored`, `file`) has none. */
export type ReviewNotOnDisk = { id: number; path: string; what: ReviewRevert["what"]; lines: [number, number] | null; why: string };
/** The reply to `review_send` with `confirm: null`: the message as it would go, and what it would leave out. */
export type ReviewSendPreviewEnvelope = {
  requestId: string;
  tab: TabId;
  digest: string;
  text: string;
  notOnDisk: ReviewNotOnDisk[];
  /** A turn is running, so the message waits behind it. */
  queued: boolean;
};
/** One interrupted revert the shell found in the journal; window-level. `at` is epoch milliseconds. */
export type ReviewRecoveryEntry = { id: string; path: string; at: number };
export type ReviewRecoveryEnvelope = { entries: ReviewRecoveryEntry[] };
export type ReviewLine = {
  kind: "context" | "added" | "removed" | "no_newline";
  text: string;
  oldNo: number | null;
  newNo: number | null;
};
export type ReviewHunk = { id: number; header: string; lines: ReviewLine[] };
export type ReviewDiffEnvelope = {
  requestId: string;
  tab: TabId;
  turn: number;
  path: string;
  added: number;
  removed: number;
  /** `null`: the patch is over the display cap. `added`/`removed` are still given. */
  hunks: ReviewHunk[] | null;
  /** A binary file: it reverts only whole. Optional for the same reason as `ReviewEnvelope.draft`. */
  binary?: boolean;
  /** The turn created the file / deleted it. */
  newFile?: boolean;
  deletedFile?: boolean;
};
/** A finished turn changed `files` files on disk; `0` clears the hint. */
export type ReviewHintEnvelope = { tab: TabId; turn: number; files: number };
