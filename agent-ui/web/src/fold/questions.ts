import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, QuestionState } from "../types";

export function waitingQuestions(state: AgentUiState) {
  return state.questions.filter((record) => record.state.kind !== "settled");
}

function changeSendState(state: AgentUiState, id: string, from: QuestionState["kind"], to: QuestionState): AgentUiState {
  return { ...state, questions: state.questions.map((record) =>
    record.interactionId === id && record.state.kind === from ? { ...record, state: to } : record) };
}

export function markQuestionAnswering(state: AgentUiState, id: string): AgentUiState {
  return changeSendState(state, id, "waiting", { kind: "answering" });
}

export function questionSendFailed(state: AgentUiState, id: string): AgentUiState {
  return changeSendState(state, id, "answering", { kind: "waiting" });
}

export function fold(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  if (event.type === "question_asked") {
    return {
      ...state, assistantMessageOpen: false, thinkingOpen: false,
      questions: [...state.questions.filter((record) => record.interactionId !== event.interaction_id), {
        seq: state.nextSeq - 1,
        interactionId: event.interaction_id, toolUseId: event.tool_use_id ?? null, turnId: state.activeTurnId,
        questions: (event.questions ?? []).map((question) => ({
          question: question.question, header: question.header ?? "", multiSelect: question.multi_select ?? false,
          options: (question.options ?? []).map((option) => ({
            label: option.label, description: option.description ?? "", preview: option.preview ?? null,
          })),
        })),
        state: { kind: "waiting" }, unsupportedReason: event.unsupported_reason ?? null,
      }],
    };
  }
  if (event.type === "question_settled") {
    return { ...state, questions: state.questions.map((record) =>
      record.interactionId === event.interaction_id && record.state.kind !== "settled" ? {
        ...record, state: { kind: "settled", outcome: event.outcome,
          answers: (event.answers ?? []).map((answer) => ({
            questionIndex: answer.question_index, answer: answer.answer, notes: answer.notes ?? null,
          })), reason: event.reason ?? null },
      } : record) };
  }
  return state;
}

/** A call to `toolUseId` began in turn `turnId`. The CLI can ask a question before the frames that announce
 *  the turn it woke itself for arrive, so the question was folded under the turn before (or under none); the
 *  call names the turn it belongs to. A question asked for that call takes the call's turn and its place in
 *  the record, and the list stays in `seq` order. MUST stay identical to the Rust `questions::on_call_started`. */
export function onCallStarted(state: AgentUiState, event: AgentDomainEvent, seq: number): AgentUiState {
  if (event.type !== "tool_call_started" || event.tool_use_id === "") return state;
  const moves = (record: AgentUiState["questions"][number]) =>
    record.toolUseId === event.tool_use_id && record.turnId !== event.turn_id;
  if (!state.questions.some(moves)) return state;
  return { ...state, questions: state.questions
    .map((record) => moves(record) ? { ...record, turnId: event.turn_id, seq } : record)
    .sort((a, b) => a.seq - b.seq) };
}

export function fromSnapshot(state: AgentUiState, snapshot: AgentUiSnapshot): AgentUiState {
  return { ...state, questions: snapshot.questions ?? [] };
}

export function onSessionEnd(state: AgentUiState, _event: AgentDomainEvent): AgentUiState {
  return { ...state, questions: state.questions.map((record) => record.state.kind === "settled" ? record : {
    ...record, state: { kind: "settled", outcome: "session_closed", answers: [], reason: null },
  }) };
}
