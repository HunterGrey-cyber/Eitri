import { forwardRef, useEffect, useId, useImperativeHandle, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, RefObject } from "react";
import { nextMirrorId, nextRequestId, postToRust } from "../bridge";
import type { InboundPayload, PlanChoiceWire } from "../bridge";
import type { PermissionModeChoice, PlanRecord, TabId } from "../types";
import { planKey } from "../planKeys";
import { bypassYesCounts } from "../modeKey";
import { renderMarkdownRevealed } from "../markdown";
import { revealedText } from "../revealHidden";
import { NO_TEXT_ASSIST } from "../textField";

export type PlanDockHandle = {
  key: (event: KeyboardEvent<HTMLElement>) => void;
  focus: () => void;
  /** Sends a feedback draft still waiting out the mirror's delay now; nothing when none is pending. */
  flush: () => void;
  reply: (payload: InboundPayload) => boolean;
};

type Props = {
  tab: TabId;
  record: PlanRecord;
  /** The mode plan mode was entered from (`planning.origin`): only Bypass offers "Approve and keep Bypass". */
  origin: PermissionModeChoice | null;
  /** The feedback typed earlier for this plan (the tab row's `planFeedbackDraft`), restored on mount. */
  draft?: string;
  active: boolean;
  /** The page's arrival guard: a key pressed or held as the plan arrived decides nothing. */
  blocked: () => boolean;
  onActivate: () => void;
  /** Esc on the actions: the panel goes back to BROWSE, the plan still waiting. */
  onBrowse?: () => void;
  onHelp?: () => void;
  /** Says why a key did nothing (an Enter pressed too soon after another key). */
  onRefused?: (why: string) => void;
  /** Whether this plan still waits in its tab, read from the page as it is now (see `QuestionDock`). */
  stillWaiting: () => boolean;
};

type Action = { choice: PlanChoiceWire; label: string };

/** The dock's actions, in Claude Code's order. Keep planning comes first and has the focus; Approve and keep Bypass
 *  is offered only for a plan mode entered from Bypass, last, and never focused first. */
export function planActions(origin: PermissionModeChoice | null): Action[] {
  return [
    { choice: "keep_planning", label: "Keep planning" },
    { choice: "approve_manual", label: "Approve (Manual)" },
    { choice: "approve_auto", label: "Approve (Auto)" },
    ...(origin === "bypass" ? [{ choice: "approve_bypass" as const, label: "Approve and keep Bypass" }] : []),
  ];
}

/** How far a line and half a page scroll the plan, in pixels. */
const LINE_PX = 40;

/** Claude's plan, for a person to approve or send back. The plan is the model's text: it is drawn only through
 *  `renderMarkdown` (sanitized, remote images as links) with its hidden characters shown (`renderMarkdownRevealed`), and every other word the model wrote is a text child with
 *  its hidden characters shown. Nothing but an explicit action here decides the plan: no digit or letter, no key held
 *  or pressed as it arrived, and Enter only once the dock has been on screen and no other key was pressed for the
 *  bypass prompt's guard. */
export const PlanDock = forwardRef<PlanDockHandle, Props>(function PlanDock(
  { tab, record, origin, draft, active, blocked, onActivate, onBrowse, onHelp, onRefused, stillWaiting }, ref) {
  const actions = planActions(origin);
  const [focus, setFocus] = useState(0);
  const [feedbackOpen, setFeedbackOpen] = useState(() => (draft ?? "") !== "");
  const [feedback, setFeedback] = useState(draft ?? "");
  const feedbackTooLong = new TextEncoder().encode(feedback).length > 16384;
  const feedbackLimit = feedbackTooLong ? "Feedback is limited to 16 KiB of UTF-8; shorten it before sending" : "";
  const feedbackRef = useRef(feedback);
  feedbackRef.current = feedback;
  const [error, setError] = useState("");
  const [sending, setSending] = useState<string | null>(null);
  const request = useRef<string | null>(null);
  const [sent, setSent] = useState(false);
  const [waitingCalls, setWaitingCalls] = useState(0);
  const busy = sending !== null || sent || record.state.kind === "answering";
  const root = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLElement>(null);
  const field = useRef<HTMLTextAreaElement>(null);
  // When the dock was painted, and the last key pressed in it: Enter counts only when both are far enough back.
  const openedAt = useRef(Infinity);
  const lastKeyAt = useRef(-Infinity);
  /** The guard starts over whenever the dock takes the keys, not only when it first appears: an Enter that brought
   *  the keys here (Enter on the plan's row, `i`) and the one after it are not a decision on the plan. */
  const restart = () => { openedAt.current = lastKeyAt.current = performance.now(); };
  const pendingG = useRef(false);
  const ids = useId();
  useLayoutEffect(() => {
    const frame = requestAnimationFrame(() => { openedAt.current = performance.now(); });
    return () => cancelAnimationFrame(frame);
  }, []);
  useLayoutEffect(() => {
    if (active) restart();
    // Taken back to BROWSE, the dock comes back on Keep planning, never on an approval left focused.
    else setFocus(0);
    if (active && !root.current?.contains(document.activeElement)) root.current?.focus({ preventScroll: true });
  }, [active]);
  const html = useMemo(() => renderMarkdownRevealed(record.plan), [record.plan]);
  const plain = useMemo(() => html === null ? revealedText(record.plan) : "", [html, record.plan]);

  // The feedback is mirrored to the host as it is typed, so a switch or a reload brings it back.
  const taken = useRef(false);
  const stillWaitingRef = useRef(stillWaiting);
  stillWaitingRef.current = stillWaiting;
  const mirrorTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mirrored = useRef(draft ?? "");
  const mirror = () => {
    if (mirrorTimer.current !== null) clearTimeout(mirrorTimer.current);
    mirrorTimer.current = null;
    if (taken.current || !stillWaitingRef.current() || mirrored.current === feedbackRef.current || feedbackTooLong) return;
    mirrored.current = feedbackRef.current;
    postToRust({ type: "plan_feedback_draft", request_id: nextMirrorId(), tab, interaction_id: record.interactionId, text: feedbackRef.current });
  };
  const mirrorRef = useRef(mirror);
  mirrorRef.current = mirror;
  useEffect(() => {
    if (mirrored.current === feedback) return;
    mirrorTimer.current = setTimeout(() => mirrorRef.current(), 300);
    return () => { if (mirrorTimer.current !== null) clearTimeout(mirrorTimer.current); };
  }, [feedback]);
  useEffect(() => () => mirrorRef.current(), []);

  function send(choice: PlanChoiceWire) {
    if (busy || request.current !== null || blocked() || record.unsupportedReason || (choice === "keep_planning" && feedbackTooLong)) return;
    mirror();
    const id = `plan-${record.interactionId}-${nextRequestId()}`;
    request.current = id;
    setSending(id);
    setError("");
    setWaitingCalls(0);
    const base = { request_id: id, tab, interaction_id: record.interactionId, choice, plan_sha256: record.planSha256 };
    postToRust(choice === "keep_planning" ? { type: "plan_resolve", ...base, feedback: feedbackRef.current } : { type: "plan_resolve", ...base });
  }
  function activate(index: number) {
    const action = actions[index];
    if (action === undefined || record.unsupportedReason) return;
    if (action.choice !== "keep_planning") { send(action.choice); return; }
    setFeedbackOpen(true);
    setTimeout(() => field.current?.focus({ preventScroll: true }), 0);
  }
  function scroll(by: "line" | "half" | "edge", step: 1 | -1) {
    const region = body.current;
    if (region === null) return;
    if (by === "edge") region.scrollTop = step < 0 ? 0 : region.scrollHeight;
    else region.scrollTop += step * (by === "line" ? LINE_PX : Math.max(LINE_PX, Math.floor(region.clientHeight / 2)));
  }
  function key(event: KeyboardEvent<HTMLElement>) {
    const inField = event.target instanceof HTMLTextAreaElement;
    const intent = planKey(event.nativeEvent, inField ? "feedback" : "actions");
    const now = performance.now();
    const counts = bypassYesCounts({ now, openedAt: openedAt.current, lastKeyAt: lastKeyAt.current });
    if (!["Shift", "Control", "Alt", "Meta"].includes(event.key)) lastKeyAt.current = now;
    const afterG = pendingG.current;
    pendingG.current = intent.type === "g" && !afterG;
    // A modified Enter or Space is not the dock's, and must not press a button either.
    if (intent.type === "none") { if (!inField && (event.key === "Enter" || event.key === " ")) event.preventDefault(); return; }
    event.stopPropagation();
    event.preventDefault();
    switch (intent.type) {
      case "swallow":
        return;
      case "help":
        onHelp?.();
        return;
      case "browse":
        setFocus(0);
        onBrowse?.();
        return;
      case "g":
        // vim's `gg`.
        if (afterG) scroll("edge", -1);
        return;
      case "back":
        root.current?.focus({ preventScroll: true });
        return;
      case "focus":
        setFocus(Math.max(0, Math.min(actions.length - 1, focus + intent.step)));
        return;
      case "scroll":
        scroll(intent.by, intent.step);
        return;
      case "activate":
        if (blocked() || busy) return;
        if (!counts) { onRefused?.("Enter again after a pause: the plan takes only an Enter pressed on its own"); return; }
        activate(focus);
        return;
      case "send":
        if (blocked() || busy) return;
        send("keep_planning");
        return;
    }
  }
  useImperativeHandle(ref, () => ({
    key,
    focus: () => { restart(); root.current?.focus({ preventScroll: true }); },
    flush: () => { if (mirrorTimer.current !== null) mirror(); },
    reply: (payload) => {
      if (payload.kind === "plan_waiting" && payload.requestId === request.current) {
        setWaitingCalls(payload.calls);
        return true;
      }
      if (payload.kind === "command_result" && payload.requestId === request.current) {
        request.current = null;
        setSending(null);
        setWaitingCalls(0);
        if (payload.ok) {
          taken.current = true;
          setSent(true);
        } else {
          // The plan still waits: the dock is as it was, the feedback included, and the keys are back on it.
          setError(payload.error);
          if (feedbackOpen) setTimeout(() => field.current?.focus({ preventScroll: true }), 0);
          else root.current?.focus({ preventScroll: true });
        }
        return true;
      }
      return false;
    },
  }));

  const text = (value: string) => <span className="plan-text">{revealedText(value)}</span>;
  const status = waitingCalls > 0
    ? `waiting for ${waitingCalls} ${waitingCalls === 1 ? "call" : "calls"} to finish`
    : sending !== null || record.state.kind === "answering" ? "sending…" : sent ? "Answer sent" : feedbackLimit || error;
  return <div ref={root} className="plan-dock" role="group" aria-label="Plan for review" tabIndex={0}
    data-active={active} onKeyDown={key} aria-activedescendant={`${ids}a${focus}`}
    onFocus={(e) => { if (!root.current?.contains(e.relatedTarget as Node | null)) restart(); onActivate(); }}>
    <div className="plan-head">Plan for review</div>
    {record.unsupportedReason ? <p className="plan-unsupported">{text(record.unsupportedReason)}; interrupt the turn to end it</p> : <>
      {html === null ? <>
        <p className="plan-unsupported">Too many hidden characters to mark them in place: the plan is shown as plain text, each one escaped.</p>
        <pre ref={body as RefObject<HTMLPreElement>} className="plan-body plan-text">{plain}</pre>
      </> : <div ref={body as RefObject<HTMLDivElement>} className="plan-body markdown-body" dangerouslySetInnerHTML={{ __html: html }} />}
      {record.allowedPrompts.length > 0 && <div className="plan-prompts">
        <div className="plan-prompts-title">Claude says it will need</div>
        <ul>{record.allowedPrompts.map((prompt, i) => <li key={i}>{text(prompt.tool)}: {text(prompt.prompt)}</li>)}</ul>
      </div>}
    </>}
    <div className="plan-actions" role="toolbar" aria-label="Plan actions">
      {/* The buttons never hold the keys (the dock does), so no key reaches one past the dock's guard; a click still
          presses it. */}
      {actions.map((action, i) => <button type="button" key={action.choice} id={`${ids}a${i}`} className="plan-action"
        data-choice={action.choice} data-focused={i === focus} aria-current={i === focus} tabIndex={-1}
        disabled={busy || record.unsupportedReason !== null}
        onMouseDown={(e) => e.preventDefault()}
        onClick={() => { onActivate(); setFocus(i); if (!blocked()) activate(i); }}>
        <span aria-hidden="true">{i === focus ? "▌" : " "}</span>{action.label}
      </button>)}
    </div>
    {feedbackOpen && <div className="plan-feedback">
      <textarea {...NO_TEXT_ASSIST} ref={field} aria-label="What to change in the plan (optional)" value={feedback}
        disabled={busy} placeholder="What should Claude change? (optional)"
        onChange={(e) => setFeedback(e.target.value)} />
      <button type="button" className="plan-send" tabIndex={-1} onMouseDown={(e) => e.preventDefault()} disabled={busy || feedbackTooLong} onClick={() => { onActivate(); if (!blocked()) send("keep_planning"); }}>Send</button>
    </div>}
    <div className="plan-hint">Tab/h/l choose · Enter select · j/k Ctrl-d/u gg/G scroll · F1 keys{feedbackOpen ? " · Ctrl+Enter send feedback · Esc back" : ""}</div>
    <div className="plan-footer" role="status">{status && text(status)}</div>
  </div>;
});
