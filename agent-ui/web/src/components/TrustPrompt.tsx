import { Fragment, forwardRef, useMemo } from "react";
import { countEscapes, revealHidden, revealPath } from "../revealHidden";
import type { RevealedPiece } from "../revealHidden";
import { groupItemsByFile, trustFooter } from "../trust";
import type { TrustItem, TrustPromptEnvelope, TrustText } from "../trust";
import { DiffRows } from "./EditDiff";
import { HiddenWarning, Revealed } from "./Revealed";

/** Text from a repository, drawn as text nodes with each hidden or direction-changing character shown as an
 *  escape, so a file name or a command cannot read as something else than what it is. */
function Shown({ text }: { text: string }) {
  const pieces = useMemo(() => revealHidden(text), [text]);
  return <Revealed pieces={pieces} />;
}

function ShownPath({ text }: { text: string }) {
  const pieces = useMemo(() => revealPath(text), [text]);
  return <Revealed pieces={pieces} />;
}

function ItemRow({ item }: { item: TrustItem }) {
  // An unreadable file is named for what it is, whatever label Rust gave it: nothing about it was read.
  const label = item.what === "unreadable" ? "cannot be checked" : item.label;
  const hidden = useMemo(
    () => countEscapes(revealHidden(label)) + countEscapes(revealHidden(item.value)), [label, item.value],
  );
  return (
    <li className="trust-item" data-what={item.what}>
      <div className="trust-item-text">
        <Shown text={`${label}: ${item.value}`} />
      </div>
      {item.outside && <div className="trust-outside">points outside the repository</div>}
      <HiddenWarning count={hidden} what="text" />
    </li>
  );
}

function PathList({ title, paths }: { title: string; paths: string[] }) {
  if (paths.length === 0) return null;
  return (
    <>
      <h4>{title}</h4>
      <ul className="trust-paths">
        {paths.map((path, i) => (
          <li key={i}>
            <ShownPath text={path} />
          </li>
        ))}
      </ul>
    </>
  );
}

type Props = { envelope: TrustPromptEnvelope };

function DiffGap({ count }: { count: number }) {
  return count > 0 ? <div className="trust-diff-gap">{count} unchanged {count === 1 ? "line" : "lines"}</div> : null;
}

/** Escapes one question may draw, as elements of their own. The rows are counted on the worker, but a row can hold
 *  hundreds of thousands of characters that each become one, and what is hidden is decided here. Twice the rows, so a
 *  file with CRLF on every line is well inside it. */
export const MAX_QUESTION_ESCAPES = 4_000;

/** Rust's words for a change the question cannot draw. */
const TOO_LONG_LINE = "too long to show in this question";

type DiffEntry = Extract<TrustText, { kind: "diff" }>;
type DrawnText =
  | { kind: "plain"; path: string; line: string }
  | { kind: "diff"; entry: DiffEntry; pieces: RevealedPiece[][][]; hidden: number };

/** Each entry as it will be drawn, the escapes a budget for the whole question in the files' order. A file past it is
 *  not cut: it gets the plain line, and later files can still fit. No element is made for what does not fit. */
function revealTexts(texts: readonly TrustText[]): DrawnText[] {
  let left = MAX_QUESTION_ESCAPES;
  return texts.map((text) => {
    if (text.kind === "plain") return text;
    const pieces = text.hunks.map((hunk) => hunk.lines.map((line) => revealHidden(line.text)));
    const hidden = pieces.reduce((sum, hunk) => sum + hunk.reduce((count, line) => count + countEscapes(line), 0), 0);
    if (hidden > left) return { kind: "plain", path: text.path, line: TOO_LONG_LINE };
    left -= hidden;
    return { kind: "diff", entry: text, pieces, hidden };
  });
}

function TrustTextDiff({ entry, pieces, hidden }: Extract<DrawnText, { kind: "diff" }>) {
  const { path, added, removed, hunks, skippedAfter } = entry;
  const pathPieces = useMemo(() => revealPath(path), [path]);
  const pathHidden = useMemo(() => countEscapes(pathPieces), [pathPieces]);
  return (
    <div className="trust-text-diff">
      <div className="trust-text-diff-head">
        <span className="trust-text-path"><Revealed pieces={pathPieces} /></span>
        <span className="permission-card-edit-counts">
          <span className="edit-count-added"><span className="edit-sign">+</span>{added}</span>{" "}
          <span className="edit-count-removed"><span className="edit-sign">−</span>{removed}</span>
        </span>
      </div>
      <HiddenWarning count={pathHidden + hidden} what="change" />
      {hunks.map((hunk, i) => (
        <Fragment key={i}>
          <DiffGap count={hunk.skipped} />
          <DiffRows lines={hunk.lines} pieces={pieces[i]} />
        </Fragment>
      ))}
      <DiffGap count={skippedAfter} />
    </div>
  );
}

/** The question before a project's own configuration for the tab's agent is loaded (hooks, MCP servers, rules),
 *  with what it would run. It names the agent the answer is about: a `y` trusts that agent's configuration and
 *  no other's. Drawn only: `App.tsx` owns every key while it is open and scrolls this box. Nothing here is set
 *  as HTML. */
export const TrustPrompt = forwardRef<HTMLDivElement, Props>(function TrustPrompt({ envelope }, ref) {
  const groups = groupItemsByFile(envelope.items);
  // Revealing every line and counting its escapes happens once for these texts, however often the open overlay renders.
  const sentTexts = envelope.changed?.texts;
  const texts = useMemo(() => revealTexts(sentTexts ?? []), [sentTexts]);
  // An agent asked about with none of its files present loads whatever appears later, so the question is
  // whether to start it at all, not whether to trust a configuration.
  const question =
    envelope.state === "nothing"
      ? `${envelope.agent} loads whatever project files it finds; there are none now. Start it here?`
      : envelope.state === "too_large"
        ? `This project's ${envelope.agent} configuration is too large to show.`
        : `Trust this project's ${envelope.agent} configuration?`;
  return (
    <div className="trust-prompt" ref={ref} role="dialog" aria-label={question}>
      <h2>
        <Shown text={question} />
      </h2>
      <p className="trust-root">
        <span className="trust-key">root</span> <ShownPath text={envelope.root} />
      </p>
      {envelope.top !== envelope.root && (
        <p className="trust-root">
          <span className="trust-key">top</span> <ShownPath text={envelope.top} />
        </p>
      )}
      {envelope.changed !== null && (
        <section className="trust-changed">
          <h3>changed since you trusted it</h3>
          <PathList title="added" paths={envelope.changed.added} />
          <PathList title="removed" paths={envelope.changed.removed} />
          <PathList title="changed" paths={envelope.changed.changed} />
        </section>
      )}
      {groups.map((group, i) => (
        <section key={i} className="trust-file">
          <h3>
            <ShownPath text={group.file} />
          </h3>
          <ul className="trust-items">
            {group.items.map((item, j) => (
              <ItemRow key={j} item={item} />
            ))}
          </ul>
        </section>
      ))}
      {envelope.items.some((item) => item.what === "allow") && (
        // Claude Code keeps its own per-folder trust and drops a project's allow rules without it. Eitri never
        // sets that trust for the user, so the prompt says what a `y` here does not reach.
        <p className="trust-note trust-allow-note">
          Claude Code applies these permissions.allow rules only if you have also trusted this folder in terminal claude;
          Eitri does not set that. Hooks, MCP servers and deny or ask rules load with y either way.
        </p>
      )}
      {envelope.agent === "Claude" && (envelope.state === "untrusted" || envelope.state === "changed") && (
        // Claude Code loads a subdirectory's CLAUDE.md the first time it reads a file there, in a trusted session
        // only. Discovery does not walk the whole tree, so the prompt says what a `y` reaches beyond this list.
        <p className="trust-note trust-subdir-note">
          With y, Claude Code also reads the CLAUDE.md of a subdirectory once it reads a file there; those are not listed
          here.
        </p>
      )}
      {envelope.remember !== "yes" && envelope.rememberNote !== null && (
        <p className="trust-note trust-remember-note">
          <Shown text={envelope.rememberNote} />
        </p>
      )}
      {texts.length > 0 && (
        <section className="trust-texts">
          <h3>what changed in the instruction files</h3>
          {texts.map((text, i) => text.kind === "plain" ? (
            <p key={i} className="trust-text-line"><ShownPath text={`${text.path}: ${text.line}`} /></p>
          ) : (
            <TrustTextDiff key={i} {...text} />
          ))}
        </section>
      )}
      <p className="trust-footer">{trustFooter(envelope.remember, envelope.needsYes === true, envelope.state)}</p>
    </div>
  );
});
