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
export type ToolCallRecord = { seq: Seq; toolUseId: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
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
export type PermissionRequestRecord = { seq: Seq; permissionId: string; toolUseId: string | null; toolName: string; input: unknown };
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

/** The command that continues a conversation Neovibe has just closed, in the user's own terminal.
 *
 * Arrives once, in a `handoff` envelope, and only AFTER the real session shutdown has finished —
 * `agent_panel.rs`'s `collect_pending_handoff` dispatches it there and nowhere else, so nobody can
 * be looking at this line while Neovibe is still driving the session.
 *
 * `command` is a ready-to-paste POSIX-shell line (`cd <dir> && claude --resume <id>`), built by
 * Rust from the same argv the supported `neovibe-claude-handoff` wrapper would `exec`. `cwd` and
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
 * while Neovibe's copy is this side's unilateral record of the same session. */
export type HistorySource = "claude_transcript" | "neovibe_copy";

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
  /** Why Claude's own transcript was not used. Non-null exactly when `source` is `neovibe_copy`. */
  fallbackReason: string | null;
  /** The CLI release that wrote the transcript. Never a schema version, and nothing branches on
   * it. `null` on the `neovibe_copy` path, which has no such concept. */
  writerVersion: string | null;
};

export type AgentUiState = {
  backend: BackendKind;
  /** Three identities, deliberately never collapsed into one field.
   *  conversationId    -- Neovibe's own, stable for a workspace (null on the legacy backend)
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
  /** What this session was seeded with before it produced anything of its own, or `null` when it
   * restored nothing -- which is every fresh session. On the wire, so it survives a resync and a
   * panel reload exactly as the four collections do. */
  history: HistoryNotice | null;
  /** Reducer-internal, never on the wire: true while the last folded event was assistant text, so
   * the next chunk continues the same message. See `reducer.ts`'s `content_delta` case. */
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
};

/** A snapshot as it ACTUALLY arrives from Rust, which is not an `AgentUiState`.
 *
 * `serialize_snapshot_for_js` emits neither of the three reducer-internal fields above -- they are
 * marked "never on the wire" for a reason and Rust has no key for any of them. Typing the inbound
 * envelope as a full `AgentUiState` asserted all three were present and `number`/`boolean` when all
 * three are `undefined` at runtime; `applySnapshot` overrides them immediately so nothing broke, but
 * anything that read `payload.state.nextSeq` before that -- a resync-diffing path, say, which is
 * exactly the kind of thing this area attracts -- would have got `undefined` with the compiler
 * insisting on a number. Subtracting them is the honest shape: what is missing is now missing in the
 * type too. */
export type AgentUiSnapshot = Omit<AgentUiState, "assistantMessageOpen" | "nextSeq" | "turnThinking">;

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
};

export type TurnOutcome = "completed" | "interrupted" | "failed" | "limit_reached";

/** What a provider reported about a turn's cost. Mirrors Rust's `agent::UsageInfo`, and the
 *  snake_case field names are deliberate -- `AgentDomainEvent` reaches the frontend through
 *  serde's own derive, not through `serialize_snapshot_for_js`'s camelCase re-shaping.
 *
 *  `turn_completed.usage` is `null` whenever the backend reported nothing, which on the `sidecar`
 *  backend is EVERY turn: `verdandi.claude.runtime.v1`'s `TurnCompleted` message carries no usage
 *  fields at all. Anything that renders this must show `null` as unknown. Showing it as $0.00 would
 *  reinstate the bug that made the field nullable: this event used to arrive with
 *  `total_cost_usd: 0, num_turns: 0` hardcoded on that backend, so a cost readout would have been
 *  correct on `legacy` and confidently, permanently wrong on `sidecar`. Nothing renders it today. */
export type UsageInfo = { total_cost_usd: number; num_turns: number };
export type AgentDomainEvent =
  | { type: "session_opened"; session_id: string; provider_session_id: string; model: string; cwd: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "user_prompt_submitted"; text: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  | { type: "tool_call_started"; turn_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_call_completed"; turn_id: string; tool_use_id: string; content: unknown; is_error: boolean }
  | { type: "permission_requested"; permission_id: string; tool_use_id: string | null; tool_name: string; input: unknown }
  | { type: "permission_resolved"; permission_id: string; outcome: "allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close" | "provider_failed" | "expired" }
  | { type: "turn_completed"; turn_id: string; outcome: TurnOutcome; result_text: string; stop_reason: string | null; usage: UsageInfo | null }
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
  | { type: "session_closed"; reason: string };

/** A session tab's identity for its whole life, and the bridge's (`neovibe_core::tabs::TabId`). */
export type TabId = number;
/** `neovibe_core::agent_bridge::TabStateWire`. */
export type TabState = "not_started" | "starting" | "live" | "ended" | "failed";
/** `neovibe_core::tabs::Marker::wire()`. Precedence is Rust's: ⚑ > ✕ > working > •. */
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
};
export type TabsEnvelope = { active: TabId; tabs: TabInfo[] };
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
export type ChooserEnvelope = { launch: boolean; open: ChooserTab[]; records: ChooserRecord[] };
