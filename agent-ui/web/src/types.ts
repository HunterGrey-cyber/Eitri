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
export type UserPromptRecord = { seq: Seq; text: string };
export type ToolCallRecord = {
  seq: Seq;
  toolUseId: string;
  name: string;
  input: unknown;
  result: { content: unknown; isError: boolean } | null;
  /** The turn the call started in (`tool_call_started`'s `turn_id`, and the snapshot's `turnId`). A
   *  result only ever arrives inside its own turn, so a `null` result whose turn is not the active
   *  one never will (`MessageList`'s `isAbandonedCall`). Absent only where nothing carried it. */
  turnId?: string;
  /** v1 polish F18: the saved prefix rule that answered this call's permission request
   *  (`Bash(git log *)`), when a rule and not the user did. Absent on every other call. */
  allowedByRule?: string;
  /** v1 polish F22: this `Write`'s card was raised over no file (see `PermissionRequestRecord`),
   *  OR (fix round finding 1) this `Write` was answered by the acceptEdits fast path with no card
   *  raised at all, over a path that did not exist when the call started. Either way: a completed
   *  row reads "Creates a new file" rather than the overwrite warning. */
  createsFile?: boolean;
  /** O3 review item 7: the CLI's own prompt for this call was answered without a card -- "Claude Code
   *  safety check — allowed in bypass" / "— allowed with your approval". Absent on every other call. */
  promptNote?: string;
  /** v1 trial item 7: this `Write`/`Edit`/`NotebookEdit` was answered by the acceptEdits fast path
   *  (`agent::permission_policy`) with no card. Absent on every other call, a carded or
   *  rule-answered one (`allowedByRule`) included -- the two are mutually exclusive, since no rule
   *  ever fires for these three tools at all. */
  allowedByAuto?: boolean;
  /** The CLI itself refused this call (Rust's `ToolCallDenial`, from a `permission_denied` event):
   *  its auto-mode classifier most often, whose `reasonType` is `"classifier"`. Both strings are the
   *  CLI's own, shown and never parsed; either may be `null`. Absent on every call nobody refused. */
  denied?: ToolCallDenial;
};
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
  /** v1 polish F22: a `Write` whose file did not exist when the card was raised (Rust looked).
   *  Absent when it did, and for every other tool. */
  createsFile?: boolean;
  /** O3: present when the CLI itself asked about this call, after the gate had already answered it
   *  (Verdandi's PERMISSION_ORIGIN_PROVIDER_PROMPT; Rust's `ProviderPrompt`). Absent on the gate's
   *  own request. Answered like any card, with the same permission id. */
  providerPrompt?: ProviderPrompt;
};
/** The CLI's own words about why it asked, verbatim; any of them may be `null`. `reason` is English
 *  prose from the CLI ("... which is a sensitive file.") -- shown, never parsed. */
export type ProviderPrompt = {
  reason: string | null;
  description: string | null;
  blockedPath: string | null;
  /** The user's own `permissions.ask` rule that forced this prompt, when one did. */
  matchedAskRule: MatchedAskRule | null;
  /** The raw `origin` a sidecar newer than this build sent (O3 review #3); `null` for a known one.
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

export type BackendKind = "legacy" | "sidecar";

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
  /** The tab's rename, kept on the record (spec §3.5); the chooser shows it ahead of the title. */
  name?: string | null;
};
export type PermissionModeChoice = "auto" | "bypass";

/** The command that continues a conversation Eitri has just closed, in the user's own terminal.
 *
 * Arrives once, in a `handoff` envelope, and only AFTER the real session shutdown has finished —
 * `agent_panel.rs`'s `collect_pending_handoff` dispatches it there and nowhere else, so nobody can
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
  /** Only things worth warning about: an untested CLI version, a Verdandi checkout that has drifted
   * from the verified baseline. **Empty is the normal case**, which is what makes non-empty a real
   * signal worth putting a glyph on. */
  startupDiagnostics: string[];
};

/** Which of the two records a restored history came from. The panel says which, because the two
 * can genuinely differ: Claude's own transcript is what `claude --resume` in a terminal would show,
 * while Eitri's copy is this side's unilateral record of the same session. */
export type HistorySource = "claude_transcript" | "eitri_copy";

/** One statement about the history a resumed session was seeded with (design §5.5).
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

/** One item waiting behind a running turn (phase 3 ruling 1), as it reaches the panel: what the
 * user typed, and when it was queued (epoch ms), for the composer's "queued" list. The wire text
 * composed against the editor context at queue time is Rust's own concern and never reaches here. */
export type QueueItem = { text: string; queuedAt: number };
/** V1's editor-context line (phase 3 ruling 32): the file relative to the project root (or
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
  userPrompts: UserPromptRecord[];
  transcript: TranscriptMessage[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  activeTurnId: string | null; pendingPermissions: PermissionRequestRecord[];
  capabilities: Capabilities;
  provider: ProviderInfo | null;
  /** The last usage this tab's provider reported, or `null` before it has (R5: what the band's usage
   * segment reads). A turn that reports one replaces it whole -- a running total, so a lower figure is
   * the truth after `/clear` -- and a turn that reports none leaves it (`reducer.ts`). On the wire: a
   * snapshot carries it (an explicit `null` when nothing was reported), so a tab switch or a panel
   * reload shows the figure without waiting for the next turn. A resumed tab shows none until its
   * first turn after the resume, because history seeding reports no usage. */
  usage: UsageInfo | null;
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
   * **On the wire since sw-panel-render-2's fix.** It used to be reducer-internal and
   * `applySnapshot` forced it to `false` regardless of what a snapshot said, on the theory that a
   * snapshot always meant "start fresh" -- but Rust's own `AgentSessionProjection` keeps this bit
   * (`assistant_message_open`) and keeps appending to the open message underneath a snapshot taken
   * mid-reply, so the two sides disagreed about whether the last message was still open. Forcing
   * `false` split a streaming reply into two transcript rows -- with broken markdown at the seam --
   * on every snapshot landing mid-reply: a tab switch back to a streaming reply, `prefix r`, and a
   * bounded-queue Resync. `applySnapshot` now takes this from the snapshot like every other field. */
  assistantMessageOpen: boolean;
  /** Reducer-internal, never on the wire: the `seq` the next locally-folded item will take.
   *
   * Seeded from a snapshot's own `throughRevision`, which Rust guarantees is strictly greater than
   * every `seq` inside that snapshot -- so an item folded after a snapshot sorts after everything
   * the snapshot carried, and cannot collide with one of them. Incremented once per `applyEvent`
   * call, exactly as `AgentSessionProjection::apply` bumps `last_revision`, so on the healthy path
   * the numbers are the same numbers Rust would have assigned rather than a parallel scheme.
   *
   * **One exception, since the in-flight motion change (2026-09-20):** a REPEATED thinking delta
   * returns the incoming state untouched and advances nothing, so this counter and Rust's
   * `last_revision` drift apart by one for each such delta. That is safe and is checked rather
   * than assumed -- the skipped number is assigned to no item, locally folded `seq`s stay strictly
   * increasing, every item in a snapshot sits below its `throughRevision`, and `applySnapshot`
   * re-seeds this field from that number, so no collision and no reordering is reachable. Nothing
   * in this frontend ever compares a `seq` here against a Rust-assigned one. See `applyEvent`'s
   * own early return for why that delta must not allocate a render, let alone a number. */
  nextSeq: Seq;
  /** Reducer-internal, never on the wire: true from a `content_delta` with `kind: "thinking"` until
   * the next event of any other kind. The one ephemeral bit `turnPhase.ts`'s `phaseOf` reads that is
   * not already sitting in the four collections above -- see `2026-09-20-in-flight-motion-design.md`
   * §8.2. **Invariant: can only ever be LOST, never invented.** Every way of losing it (a resync, a
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
 * `assistantMessageOpen` is deliberately NOT in this `Omit` (sw-panel-render-2's fix, 2026-09-27):
 * it used to be, on the theory that it was reducer-internal like the other two, but Rust's own
 * projection carries the same bit and disagreeing about it split a mid-stream reply across a
 * snapshot -- see that field's own doc comment on `AgentUiState`. */
export type AgentUiSnapshot = Omit<AgentUiState, "nextSeq" | "turnThinking" | "denialsBeforeTheirCall">;

/** How long a turn has been running, tracked in `App.tsx` alongside `AgentUiState` rather than
 * inside it: it is a UI-local clock, not part of the projection Rust serializes, and it does not
 * survive a page reload the way `AgentUiState` (rebuilt from a snapshot) does.
 *
 * `exact` is `true` only when this record was minted from a real `turn_started` event; a turn id
 * first observed inside a `snapshot` envelope (a page reload, or a resync mid-turn) gets
 * `exact: false`, because this panel cannot know how long the turn had already been running before
 * it first saw it. Rendered `12s` when exact, `12s+` -- "at least this long" -- when not. See
 * `2026-09-20-in-flight-motion-design.md` §8.4. */
export type TurnClock = { turnId: string; since: number; exact: boolean };

/** The handshake reply, before any session exists: which backend is behind the bridge and what it
 * genuinely offers. The start screen renders from this rather than hardcoding either backend's
 * shape. */
/** What the launch dashboard may bring back: the tabs the last window on this project had open, in
 *  tab-bar order, and how many of those were in bypass. Rust sends it only while this window has
 *  started nothing and at least one saved tab can still be resumed, so its presence is the whole
 *  reason to draw the line. */
export type RestoreOffer = { labels: string[]; bypass: number };

export type Hello = {
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
  expectedVerdandiRevision: string | null;
  /** `agent::account`'s configured name, `null` when none is set (panel round 2 plan's §7, Task
   *  5: `serialize_hello_for_js`'s own `"account"` field). Read by the empty tab's dashboard
   *  (§7's cwd/backend/account line) and by nothing else yet. */
  account: string | null;
  /** The saved tabs the dashboard offers back (`restore_last`), or `null` when there is nothing to
   *  offer. Optional only so that a sender that predates it reads as "nothing". */
  restore?: RestoreOffer | null;
};

export type TurnOutcome = "completed" | "interrupted" | "failed" | "limit_reached";

/** What a turn that did not complete carries beyond its outcome, as `turn_completed.detail` (snake_case,
 *  as serde derives it). `message` is the provider's own words, already trimmed and capped by Rust, and is
 *  the only place a reset time the CLI wrote into its text can appear: nothing here parses it. */
export type TurnEndDetail = { reason: string | null; api_error_status: number | null; message: string | null };

/** How a turn ended without completing: its outcome, or `lost` when the session ended under a turn that
 *  was still running (no `turn_completed` was ever coming). */
export type TurnEndingKind = "interrupted" | "failed" | "limit_reached" | "lost";

/** One turn-ending row of the conversation. Ordered by `seq` with the other items; on the wire in a
 *  snapshot's `turnEndings` (camelCase, like the other collections). */
export type TurnEnding = {
  seq: Seq;
  turnId: string;
  kind: TurnEndingKind;
  reason: string | null;
  apiErrorStatus: number | null;
  message: string | null;
};

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
 *  **A running total, not one turn's spend** (R5): the SDK's result carries the session so far, a
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
export type UsageInfo = { total_cost_usd: number; num_turns: number | null; tokens: TokenUsage | null; model: string | null };
export type AgentDomainEvent =
  | { type: "session_opened"; session_id: string; provider_session_id: string; model: string; cwd: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "user_prompt_submitted"; text: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  /** The streaming assistant message is over; the next text starts a new one. Mirrors
   *  `AgentDomainEvent::AssistantMessageBoundary`. Reachable on **both** backends: legacy emits it
   *  directly (`session.rs`), and the sidecar translates a *change* of a present
   *  `TextDelta.message_id` into one (Task 3) -- no id on the wire still means today's
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
      /** O3: the CLI's own prompt; omitted by Rust on the gate's own request. */
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
  /** Wave 5, Task 1/2: the sidecar's own acknowledgement of a `set_permission_mode` RPC (W1, a
   *  synchronous unary call issued on the GTK thread, never optimistic). `mode` is Eitri's own
   *  `PermissionModeChoice`; `provider_mode` is Verdandi's raw string (diagnostics only); `floor_applied`
   *  is `true` exactly when Verdandi's bypass floor (`usesDefaultBypassDeny`) still restricted
   *  the session (a deny list) despite `unrestricted: true` -- logged loudly, never silently. **The reducer ignores
   *  this event on purpose**: the mode a tab is in lives on the `tabs` envelope (`TabInfo.mode`), not
   *  on this per-session projection, so folding it here would just be a second, driftable copy. */
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
    };

/** A session tab's identity for its whole life, and the bridge's (`eitri_core::tabs::TabId`). */
export type TabId = number;
/** `eitri_core::agent_bridge::TabStateWire`. `awaiting_trust`: a start is put off until the workspace-trust
 *  question is answered; no session is being opened, so it is not `starting`. */
export type TabState = "not_started" | "starting" | "awaiting_trust" | "live" | "ended" | "failed";
/** `eitri_core::tabs::Marker::wire()`. Precedence is Rust's: ⚑ > ✕ > working > •. */
export type TabMarker = "needs_input" | "ended" | "working" | "unread";
export type TabInfo = {
  id: TabId;
  number: number;
  /** `<n> <name>`, worded by Rust (`tabs::label`). */
  label: string;
  name: string | null;
  state: TabState;
  mode: PermissionModeChoice;
  marker: TabMarker | null;
  pending: number;
  /** False on legacy (spec D13 A): "not resumable". */
  resumable: boolean;
  failure: string | null;
  /** The record's title (panel round 2 plan's Task 5, spec §10.1): the first prompt's title, or a
   *  resumed record's display title. `null` before one exists. Read by the chooser's open-tab rows
   *  (spec §6.1's line 2) once they join a `ChooserTab` to its `TabInfo` here. */
  title: string | null;
};
export type TabsEnvelope = {
  active: TabId;
  tabs: TabInfo[];
  /** `TabSet::default_mode` (panel round 2 plan's Task 5, spec §6.3/§10.1): the mode a fresh tab,
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
};
export type ChooserEnvelope = { open: ChooserTab[]; records: ChooserRecord[] };

/** Turn review (the `review`, `review_diff` and `review_hint` envelopes; `core/src/agent_bridge.rs`).
 *  Every attribution, count and state is decided in Rust; this side only draws what it is told. */
export type ReviewScope = "turn" | "session";
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
