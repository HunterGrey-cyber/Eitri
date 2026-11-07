import type { QuestionRecord } from "../types";
import { revealedText, revealHidden } from "../revealHidden";
import { Revealed } from "./Revealed";

/* What a page that may not answer (the phone's transcript) shows in place of the desk's answering controls. This
   module imports nothing that posts to the host or reads a key, so the page that uses it carries neither. */

/** The line a page without controls shows in their place: where the question is answered, and what Stop on the
 *  phone does to it (`stop`, a sentence, or nothing when Stop does not reach it). The location is the desk's names,
 *  drawn with hidden characters shown. */
export function AnswerAtDesk({ location, stop }: { location: string; stop?: string }) {
  return (
    <div className="answer-at-desk" role="note">
      {location === "" ? <strong>Answer at the desk.</strong> : <>
        <strong>Answer at the desk:</strong> <Revealed pieces={revealHidden(location)} />.
      </>}
      {stop !== undefined && <> {stop}</>}
    </div>
  );
}

/** A question set on a page that may not answer it (the phone's transcript): every question with its options, as
 *  the dock words them, and in place of the dock's controls one line saying where it is answered. Nothing here
 *  takes a key or a tap. */
export function QuestionAtDesk({ record, location }: { record: QuestionRecord; location: string }) {
  const text = (value: string) => <span className="question-text">{revealedText(value)}</span>;
  return <div className="question-dock question-at-desk" role="group" aria-label="Claude has a question">
    {record.unsupportedReason ? <p>{text(record.unsupportedReason)}</p> : record.questions.map((question, i) => (
      <div key={i} className="question-at-desk-one">
        <div className="question-head">{text(question.header)}<span>{`${i + 1}/${record.questions.length}`}</span></div>
        <p>{text(question.question)}</p>
        {question.multiSelect && <p className="question-hint">select all that apply</p>}
        <ol className="question-at-desk-options">
          {question.options.map((option, j) => <li key={j}>
            <strong>{text(option.label)}</strong>
            {option.description && <span className="question-description">{text(option.description)}</span>}
          </li>)}
        </ol>
      </div>
    ))}
    <AnswerAtDesk location={location} stop="Stop on this phone ends the turn and cancels this question." />
  </div>;
}

/** The trust or bypass question a tab waits on, on a page that may not answer it (the phone's transcript): what the
 *  desk asks, as the desk's own lines when it sent them, and one line saying where it is answered. A question of a
 *  kind this build does not know is named as such rather than left out. */
export function PromptAtDesk({ prompt, location }: { prompt: unknown; location: string }) {
  const kind = prompt !== null && typeof prompt === "object" ? (prompt as { kind?: unknown }).kind : undefined;
  const lines = kind === "bypass" ? (prompt as { lines?: unknown }).lines : undefined;
  return (
    <div className="prompt-at-desk" role="group" aria-label="The desk asks">
      {kind === "trust" ? (
        <p>This tab waits for the desk to trust this project&apos;s configuration before its session starts.</p>
      ) : kind === "bypass" ? (
        <>
          <p>The desk asks whether this tab enters Bypass.</p>
          {Array.isArray(lines) && lines.filter((line): line is string => typeof line === "string").map((line, i) => (
            <p key={i} className="prompt-at-desk-line"><Revealed pieces={revealHidden(line)} /></p>
          ))}
        </>
      ) : (
        <p>The desk asks a question this app does not show yet.</p>
      )}
      <AnswerAtDesk location={location} />
    </div>
  );
}
