import { forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { nextMirrorId, nextRequestId, postToRust } from "../bridge";
import type { InboundPayload, QuestionAnswerWire } from "../bridge";
import type { QuestionRecord, TabId } from "../types";
import { questionKey } from "../questionKeys";
import { revealedText } from "../revealHidden";
import { renderMarkdown } from "../markdown";
import { NO_TEXT_ASSIST } from "../textField";

export type QuestionDraft = {
  pendingRequest?: string;
  page: number;
  focus: number;
  answers: { options: number[]; other: boolean; text: string; notes: string; showNotes: boolean }[];
};
export type QuestionDockHandle = {
  key: (event: KeyboardEvent<HTMLElement>) => void;
  focus: () => void;
  /** Sends a draft still waiting out the mirror's delay now; nothing when none is pending. */
  flush: () => void;
  reply: (payload: InboundPayload) => boolean;
};
type Props = {
  tab: TabId;
  record: QuestionRecord;
  draft?: unknown;
  active: boolean;
  blocked: () => boolean;
  onActivate: () => void;
  /** Whether this question still waits for an answer in its tab, read from the page as it is now rather than
   *  from this dock's last render: a dock unmounts after its question settled or its tab closed as well as when a
   *  switch takes its tab off screen, and only the last keeps a draft worth storing. */
  stillWaiting: () => boolean;
};

function restored(record: QuestionRecord, raw: unknown): QuestionDraft {
  const source = raw && typeof raw === "object" ? raw as Partial<QuestionDraft> : {};
  return {
    ...(typeof source.pendingRequest === "string" && source.pendingRequest.length <= 512 ? { pendingRequest: source.pendingRequest } : {}),
    page: Number.isInteger(source.page) ? Math.max(0, Math.min(source.page!, record.questions.length)) : 0,
    focus: Number.isInteger(source.focus) ? Math.max(0, Math.min(source.focus!, 10)) : 0,
    answers: record.questions.map((q, i) => {
      const a = Array.isArray(source.answers) ? source.answers[i] : undefined;
      return {
        options: Array.isArray(a?.options) ? [...new Set(a.options.filter((n) => Number.isInteger(n) && n >= 0 && n < q.options.length))].slice(0, q.multiSelect ? q.options.length : 1) : [],
        other: a?.other === true,
        text: typeof a?.text === "string" ? a.text : "",
        notes: typeof a?.notes === "string" ? a.notes : "",
        showNotes: a?.showNotes === true,
      };
    }),
  };
}

export const QuestionDock = forwardRef<QuestionDockHandle, Props>(function QuestionDock({ tab, record, draft: initialDraft, active, blocked, onActivate, stillWaiting }, ref) {
  // A request restored from the mirror was sent by an earlier mount of this dock, and its reply may have come
  // while none was mounted to read it. The host marks a set answering as it takes an answer and puts it back to
  // waiting when the answer is refused, so a restored request on a set that waits has already had its reply.
  const [initial] = useState(() => restored(record, initialDraft));
  const restoredRequest = useRef<string | null>(initial.pendingRequest ?? null);
  const [draft, setDraft] = useState<QuestionDraft>(() => {
    if (record.state.kind !== "waiting") return initial;
    const { pendingRequest: _replied, ...kept } = initial;
    return kept;
  });
  const draftRef = useRef(draft);
  draftRef.current = draft;
  const root = useRef<HTMLDivElement>(null);
  const otherField = useRef<HTMLTextAreaElement>(null);
  const notesField = useRef<HTMLTextAreaElement>(null);
  const [error, setError] = useState("");
  const [sending, setSending] = useState<string | null>(draft.pendingRequest ?? null);
  const request = useRef<string | null>(draft.pendingRequest ?? null);
  const [sent, setSent] = useState(false);
  const [replySeen, setReplySeen] = useState(false);
  const busy = sending !== null || sent || (record.state.kind === "answering" && !replySeen);
  const reviewOffered = record.questions.length > 1 || record.questions.some((q) => q.multiSelect);
  const lastPage = reviewOffered || draft.page === record.questions.length ? record.questions.length : Math.max(0, record.questions.length - 1);
  const page = Math.min(draft.page, lastPage);
  const question = record.questions[page];
  const answer = draft.answers[page];
  const preview = question && !question.multiSelect && question.options.some((o) => o.preview !== null);
  const custom = question?.options.length ?? 0;
  const chat = custom + 1;
  const rowCount = question ? question.options.length + 2 + (question.multiSelect ? 1 : 0) : 2;
  const focus = Math.min(draft.focus, rowCount - 1);
  useLayoutEffect(() => {
    root.current?.querySelector<HTMLElement>('[data-focused="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [page, focus]);
  const mirrorTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const lastMirror = useRef(-Infinity);
  // Set once the host has taken this set's answer: the question is over and a draft of it has nothing left to
  // restore, so none is sent after it.
  const taken = useRef(false);
  const stillWaitingRef = useRef(stillWaiting);
  stillWaitingRef.current = stillWaiting;
  const mirror = () => {
    if (mirrorTimer.current !== null) clearTimeout(mirrorTimer.current);
    mirrorTimer.current = null;
    // The host keeps a draft only for a question that still waits, and refuses the rest; a draft for a question
    // already answered, interrupted or closed with its tab would only be refused.
    if (taken.current || !stillWaitingRef.current()) return;
    lastMirror.current = performance.now();
    postToRust({ type: "question_draft", request_id: nextMirrorId(), tab, interaction_id: record.interactionId, draft: draftRef.current });
  };
  const mirrorRef = useRef(mirror);
  mirrorRef.current = mirror;
  useEffect(() => {
    mirrorTimer.current = setTimeout(() => mirrorRef.current(), Math.max(0, 300 - (performance.now() - lastMirror.current)));
    return () => { if (mirrorTimer.current !== null) clearTimeout(mirrorTimer.current); };
  }, [draft]);
  useEffect(() => () => mirrorRef.current(), []);
  // The same for a set that goes back to waiting after this mount (the page's record was older than the host's).
  useEffect(() => {
    if (record.state.kind !== "waiting" || request.current === null || request.current !== restoredRequest.current) return;
    request.current = null;
    setSending(null);
    const { pendingRequest: _replied, ...kept } = draftRef.current;
    change(kept);
    mirror();
  }, [record.state.kind]);
  useLayoutEffect(() => {
    if (active) root.current?.focus({ preventScroll: true });
  }, [active]);

  function change(next: QuestionDraft, pageChange = false) {
    draftRef.current = next;
    setDraft(next);
    if (pageChange) mirror();
  }
  function updateAnswer(patch: Partial<QuestionDraft["answers"][number]>) {
    change({ ...draftRef.current, answers: draftRef.current.answers.map((a, i) => i === page ? { ...a, ...patch } : a) });
  }
  function answered(a: QuestionDraft["answers"][number]) {
    return a.options.length > 0 || (a.other && a.text.trim() !== "");
  }
  function wireAnswers(): QuestionAnswerWire[] {
    return draftRef.current.answers.flatMap((a, i) => answered(a) ? [{
      question_index: i, option_indexes: a.options,
      other_text: a.other && a.text.trim() !== "" ? a.text : null,
      notes: a.notes || null,
    }] : []);
  }
  function send(kind: "answer" | "cancel" | "chat") {
    if (busy || request.current !== null || blocked() || (record.unsupportedReason && kind !== "cancel")) return;
    const id = `question-${record.interactionId}-${nextRequestId()}`;
    change({ ...draftRef.current, pendingRequest: id });
    mirror();
    // From here only this request's reply is awaited; a late reply to a restored one must not end it.
    restoredRequest.current = null;
    request.current = id;
    setSending(id);
    setReplySeen(false);
    setError("");
    const base = { request_id: id, tab, interaction_id: record.interactionId };
    if (kind === "answer") postToRust({ type: "question_answer", ...base, answers: wireAnswers() });
    else postToRust({ type: "question_decline", ...base, kind, ...(kind === "chat" ? { so_far: wireAnswers() } : {}) });
  }
  function go(step: number) {
    const next = Math.max(0, Math.min(lastPage, page + step));
    if (next !== page) {
      change({ ...draftRef.current, page: next, focus: 0 }, true);
      root.current?.focus();
    }
  }
  function advance() {
    if (page === lastPage) send("answer");
    else go(1);
  }
  function pick(index: number, commit: boolean) {
    if (!question) {
      if (commit) send(index === 0 ? "answer" : "cancel");
      return;
    }
    change({ ...draftRef.current, focus: index });
    if (index === chat) { if (commit) send("chat"); return; }
    if (index > chat) {
      if (commit) {
        if (page === record.questions.length - 1) send("answer");
        else advance();
      }
      return;
    }
    if (index === custom) {
      if (commit) {
        otherField.current?.focus();
        setTimeout(() => otherField.current?.focus(), 0);
      }
      else updateAnswer({ other: question.multiSelect ? !answer.other : true, ...(!question.multiSelect ? { options: [] } : {}) });
      return;
    }
    const selected = draftRef.current.answers[page].options;
    updateAnswer({ options: question.multiSelect ? selected.includes(index) ? selected.filter((n) => n !== index) : [...selected, index].sort((a, b) => a - b) : [index], ...(!question.multiSelect ? { other: false } : {}) });
    if (commit && !question.multiSelect) advance();
  }
  function key(event: KeyboardEvent<HTMLElement>) {
    const input = event.target instanceof HTMLTextAreaElement;
    const zone = input ? "input" : question ? focus === custom ? "custom" : "option" : "option";
    const intent = questionKey(event.nativeEvent, { zone, count: question ? custom + 2 : 0, preview });
    // The window's prefix, interrupt, card approval and Super defences keep their own routes.
    if (event.metaKey || event.altKey || (event.ctrlKey && !["advance", "edit"].includes(intent.type))) return;
    event.stopPropagation();
    if (intent.type === "none") {
      if (!input) event.preventDefault();
      return;
    }
    if (intent.type === "leave" && event.key === "ArrowUp" && input) {
      const field = event.target as HTMLTextAreaElement;
      if (field.value.slice(0, field.selectionStart).includes("\n")) return;
    }
    event.preventDefault();
    if (blocked() || busy || event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (intent.type === "leave") { root.current?.focus(); return; }
    if (intent.type === "dismiss") { send("cancel"); return; }
    if (intent.type === "advance") {
      if (draftRef.current.answers.every(answered)) send("answer");
      else { change({ ...draftRef.current, page: record.questions.length, focus: 0 }, true); root.current?.focus(); }
    } else if (intent.type === "question") go(intent.step);
    else if (intent.type === "move") change({ ...draftRef.current, focus: (focus + intent.step + rowCount) % rowCount });
    else if (intent.type === "edge") change({ ...draftRef.current, focus: intent.to === "first" ? 0 : rowCount - 1 });
    else if (intent.type === "number") pick(intent.index, false);
    else if (intent.type === "activate" || intent.type === "commit") {
      if (input) { if (answered(draftRef.current.answers[page])) advance(); }
      else pick(focus, intent.type === "commit");
    } else if (intent.type === "type") {
      updateAnswer({ text: answer.text + event.key, other: true, ...(!question.multiSelect ? { options: [] } : {}) });
      otherField.current?.focus();
    } else if (intent.type === "notes") {
      updateAnswer({ showNotes: true });
      setTimeout(() => notesField.current?.focus(), 0);
    } else if (intent.type === "edit" && question && (input ? event.target === otherField.current : focus === custom)) {
      mirror();
      postToRust({ type: "edit_question_other", request_id: nextRequestId(), tab, interaction_id: record.interactionId, question_index: page, text: answer.text });
    }
  }
  useImperativeHandle(ref, () => ({
    key,
    focus: () => root.current?.focus({ preventScroll: true }),
    flush: () => { if (mirrorTimer.current !== null) mirror(); },
    reply: (payload) => {
      // A restored request dropped while the page's record still read waiting can be answered later: its reply
      // still ends the wait the host's answering state holds the dock in.
      if (payload.kind === "command_result" &&
          (payload.requestId === request.current || payload.requestId === restoredRequest.current)) {
        request.current = null;
        restoredRequest.current = null;
        if (payload.ok) taken.current = true;
        const { pendingRequest: _pending, ...kept } = draftRef.current;
        change(kept);
        mirror();
        setSending(null);
        setReplySeen(true);
        setSent(payload.ok);
        if (!payload.ok) setError(payload.error);
        return true;
      }
      if (payload.kind === "question_other_edited" && payload.tab === tab && payload.interaction_id === record.interactionId && draftRef.current.answers[payload.question_index]) {
        const q = record.questions[payload.question_index];
        change({ ...draftRef.current, answers: draftRef.current.answers.map((a, i) => i === payload.question_index ? { ...a, text: payload.text, other: true, ...(!q.multiSelect ? { options: [] } : {}) } : a) });
        return true;
      }
      return false;
    },
  }));

  const text = (value: string) => <span className="question-text">{revealedText(value)}</span>;
  const row = (index: number, label: string, selected = false, description = "") => (
    <button type="button" className="question-option" key={index} data-focused={index === focus} aria-pressed={selected}
      disabled={busy} onFocus={() => { onActivate(); change({ ...draftRef.current, focus: index }); }}
      onClick={() => { onActivate(); if (!blocked()) pick(index, false); }}>
      <span aria-hidden="true">{index === focus ? "▌" : " "}{question?.multiSelect && index <= custom ? selected ? "[x] " : "[ ] " : selected ? "✓ " : ""}{index + 1}. </span>
      <strong>{text(label)}</strong>{description && <span className="question-description">{text(description)}</span>}
    </button>
  );
  return <div ref={root} className="question-dock" role="group" aria-label="Claude has a question" tabIndex={0}
    data-active={active} onKeyDown={key} onFocus={onActivate}>
    <div className="question-head">{question ? text(Array.from(question.header).slice(0, 12).join("") + (Array.from(question.header).length > 12 ? "…" : "")) : "Review your answers"}
      <span>{question ? `${page + 1}/${record.questions.length}` : "Review"}{reviewOffered && question ? " · Review" : ""}</span></div>
    {record.unsupportedReason ? <p>{text(record.unsupportedReason)}</p> : question ? <>
      <p>{text(question.question)}</p>
      {question.multiSelect && <p className="question-hint">select all that apply</p>}
      {question.options.map((option, i) => row(i, option.label, answer.options.includes(i), option.description))}
      {row(custom, "Type something", answer.other)}
      {(focus === custom || answer.text !== "") && (
        <textarea
        {...NO_TEXT_ASSIST} ref={otherField} aria-label="Type something" value={answer.text} disabled={busy}
        onFocus={() => updateAnswer({ other: true, ...(!question.multiSelect ? { options: [] } : {}) })}
        maxLength={32768} onChange={(e) => updateAnswer({ text: e.target.value, other: true, ...(!question.multiSelect ? { options: [] } : {}) })} />)}
      <hr />{row(chat, "Chat about this")}
      {question.multiSelect && <button type="button" data-focused={focus === chat + 1} disabled={busy}
        onFocus={() => change({ ...draftRef.current, focus: chat + 1 })} onClick={() => { if (!blocked()) { if (page === record.questions.length - 1) send("answer"); else advance(); } }}>{page === record.questions.length - 1 ? "Submit" : "Next"}</button>}
      {preview && <>
        <div className="question-preview" dangerouslySetInnerHTML={{ __html: renderMarkdown(question.options[focus]?.preview ?? "") }} />
        <p className="question-hint">press n to add notes</p>
        {answer.showNotes && <label>Notes:
          <textarea
          {...NO_TEXT_ASSIST} ref={notesField} aria-label="Notes" value={answer.notes} disabled={busy} maxLength={8192}
          onChange={(e) => updateAnswer({ notes: e.target.value })} /></label>}
      </>}
    </> : <>
      {!draft.answers.every(answered) && <p>You have not answered all questions</p>}
      {record.questions.map((q, i) => <div key={i}>{text(q.question)}<div>→ {text([...draft.answers[i].options.map((n) => q.options[n].label), ...(draft.answers[i].other && draft.answers[i].text ? [draft.answers[i].text] : [])].map((value) => q.multiSelect && (value.includes(", ") || value.includes('"')) ? JSON.stringify(value) : value).join(", "))}</div>
        {draft.answers[i].notes && <div>→ notes: {text(draft.answers[i].notes)}</div>}</div>)}
      <p>Ready to submit your answers?</p>
      <button type="button" data-focused={focus === 0} disabled={busy} onFocus={() => change({ ...draftRef.current, focus: 0 })} onClick={() => send("answer")}>Submit answers</button>
      <button type="button" data-focused={focus === 1} disabled={busy} onFocus={() => change({ ...draftRef.current, focus: 1 })} onClick={() => send("cancel")}>Cancel</button>
    </>}
    {record.unsupportedReason && <button type="button" disabled={busy} onClick={() => send("cancel")}>Cancel</button>}
    <div className="question-hint">1–9 pick · j/k move · Enter select · Tab next question<br />Ctrl+G edit in nvim · Ctrl+k transcript · Esc cancel</div>
    <div className="question-footer" role="status">{sending !== null || (record.state.kind === "answering" && !replySeen) ? "sending…" : sent ? "Answer sent" : text(error)}</div>
  </div>;
});
