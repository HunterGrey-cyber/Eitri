import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, PlanMark, PlanningState } from "../types";

/** The newest unsettled plan is the one the host accepts a decision for. */
export function waitingPlan(state: Pick<AgentUiState, "plans">) {
  return (state.plans ?? []).filter((record) => record.state.kind !== "settled").sort((a, b) => b.seq - a.seq)[0];
}

/** Plan records from the provider's events. MUST stay identical to the records Rust's `planning::fold` keeps
 *  (`fixtures/plans.json` holds both to the same answers). The phase is not folded here: it is the host's to
 *  decide (`planningChanged`), since it moves on reports and timers the page never sees. */
export function fold(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  if (event.type === "plan_proposed") {
    return {
      ...state, assistantMessageOpen: false, thinkingOpen: false,
      // A replayed request replaces its earlier copy and takes its new arrival position.
      plans: [...state.plans.filter((record) => record.interactionId !== event.interaction_id), {
        seq: state.nextSeq - 1,
        interactionId: event.interaction_id,
        toolUseId: event.tool_use_id ?? null,
        turnId: state.activeTurnId,
        plan: event.plan ?? "",
        planSha256: event.plan_sha256 ?? "",
        allowedPrompts: (event.allowed_prompts ?? []).map((prompt) => ({ tool: prompt.tool, prompt: prompt.prompt })),
        state: { kind: "waiting" },
        unsupportedReason: event.unsupported_reason ?? null,
      }],
    };
  }
  if (event.type === "plan_settled") {
    let done = false;
    return { ...state, plans: state.plans.map((record) => {
      if (done || record.interactionId !== event.interaction_id || record.state.kind === "settled") return record;
      done = true;
      const approved = event.outcome === "approved";
      return { ...record, state: {
        kind: "settled", outcome: event.outcome,
        modeAfter: approved ? event.mode_after ?? null : null,
        feedback: event.outcome === "kept_planning" ? event.feedback ?? null : null,
        reason: event.reason ?? null,
      } };
    }) };
  }
  return state;
}

/** A call to `toolUseId` began in turn `turnId`: a plan proposed before the frames announcing its turn takes that
 *  turn and the call's place, and the list stays in `seq` order. MUST stay identical to Rust's
 *  `planning::on_call_started`. */
export function onCallStarted(state: AgentUiState, event: AgentDomainEvent, seq: number): AgentUiState {
  if (event.type !== "tool_call_started" || event.tool_use_id === "") return state;
  const moves = (record: AgentUiState["plans"][number]) =>
    record.toolUseId === event.tool_use_id && record.turnId !== event.turn_id;
  if (!state.plans.some(moves)) return state;
  return { ...state, plans: state.plans
    .map((record) => moves(record) ? { ...record, turnId: event.turn_id, seq } : record)
    .sort((a, b) => a.seq - b.seq) };
}

/** The session ended: every plan still waiting ends with it, and so does plan mode (Rust's
 *  `planning::on_session_end`). */
export function onSessionEnd(state: AgentUiState): AgentUiState {
  const plans = state.plans.map((record) => record.state.kind === "settled" ? record : {
    ...record, state: { kind: "settled" as const, outcome: "session_closed" as const, modeAfter: null, feedback: null, reason: null },
  });
  return { ...state, plans, planning: state.planning === null ? null : { phase: "executing", origin: null } };
}

export function fromSnapshot(state: AgentUiState, snapshot: AgentUiSnapshot): AgentUiState {
  return { ...state, plans: snapshot.plans ?? [], planning: planningOf(snapshot.planning), planMarks: [] };
}

function planningOf(raw: unknown): PlanningState | null {
  if (raw === null || typeof raw !== "object") return null;
  const { phase, origin } = raw as { phase?: unknown; origin?: unknown };
  if (phase !== "executing" && phase !== "entering" && phase !== "planning" && phase !== "leaving") return null;
  const mode = origin === "manual" || origin === "auto" || origin === "bypass" ? origin : null;
  return { phase, origin: mode };
}

/** The events envelope's `planningChanged`, after its events were folded: the host's reading of plan mode, and
 *  a row where the page saw plan mode start or end. `undefined` (absent) changes nothing; `null` is a session
 *  that no longer offers plan mode. */
export function withPlanning(state: AgentUiState, changed: unknown): AgentUiState {
  if (changed === undefined) return state;
  const next = planningOf(changed);
  const before = state.planning?.phase ?? "executing";
  const after = next?.phase ?? "executing";
  const kind: PlanMark["kind"] | null = after === "planning" && before !== "planning" && before !== "leaving" ? "entered"
    : after === "executing" && (before === "planning" || before === "leaving") ? "left" : null;
  // Between the last event folded and the next: the host has no record of these rows, so a snapshot drops them.
  const seq = state.nextSeq - 0.5;
  const marks = kind === null ? state.planMarks : [...state.planMarks, { seq, kind, key: `pm-${state.nextSeq}-${kind}` }];
  return { ...state, planning: next, planMarks: marks };
}

/** The host's refusals of a mode cycle while plan mode holds the tab (Rust's `PLAN_CYCLE_REFUSAL` and
 *  `PLAN_CHANGING_REFUSAL`): said as a flash where the key was pressed, not as the conversation's banner. */
export const PLAN_MODE_REFUSALS: readonly string[] = ["in plan mode: <leader>p leaves plan mode", "the mode is changing; try again"];
export function isPlanModeRefusal(error: string): boolean {
  return PLAN_MODE_REFUSALS.includes(error);
}

/** What `<leader>p` and `/plan` say where plan mode is not offered. */
export const PLAN_UNAVAILABLE = "plan mode needs a Claude tab on a sidecar that offers it";
