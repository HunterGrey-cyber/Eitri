import { Fragment, forwardRef, useMemo } from "react";
import { countEscapes, revealHidden, revealPath } from "../revealHidden";
import type { RevealedPiece } from "../revealHidden";
import { groupItemsByFile, trustFooter } from "../trust";
import type { TrustItem, TrustPromptEnvelope, TrustText } from "../trust";
import { DiffRows } from "./EditDiff";
import { HiddenWarning, Revealed } from "./Revealed";

/** Escapes one question draws as elements of their own. Past this they are drawn as text, in the value's one text
 *  node: the same `⟨U+XXXX⟩`, without the outline. A value can hold hundreds of thousands of characters that each
 *  would be an element, and which those are is decided here, so this cannot be a budget on the worker. Nothing is cut,
 *  so no value, row or file leaves the question because of it, and each keeps its warning with the true count. Twice
 *  the rows, so a file with CRLF on every line is well inside it. */
export const MAX_QUESTION_ESCAPES = 4_000;

type Pieces = RevealedPiece[];

/** A question's budget: `pieces` as they are while it lasts, and past it as one text node. */
function escapeBudget(): (pieces: Pieces) => Pieces {
  let left = MAX_QUESTION_ESCAPES;
  return (pieces) => {
    const escapes = countEscapes(pieces);
    if (escapes <= left) {
      left -= escapes;
      return pieces;
    }
    return [{ text: pieces.map((piece) => ("text" in piece ? piece.text : piece.escape)).join("") }];
  };
}

type DrawnItem = { item: TrustItem; pieces: Pieces; hidden: number };
type DrawnGroup = { file: Pieces; items: DrawnItem[] };
type DrawnText =
  | { kind: "plain"; pieces: Pieces }
  | { kind: "diff"; entry: Extract<TrustText, { kind: "diff" }>; path: Pieces; lines: Pieces[][]; hidden: number };

/** The question's question, as a person reads it: an agent asked about with none of its files present loads whatever
 *  appears later, so the question is whether to start it at all, not whether to trust a configuration. */
function questionOf(envelope: TrustPromptEnvelope): string {
  return envelope.state === "nothing"
    ? `${envelope.agent} loads whatever project files it finds; there are none now. Start it here?`
    : envelope.state === "too_large"
      ? `This project's ${envelope.agent} configuration is too large to show.`
      : `Trust this project's ${envelope.agent} configuration?`;
}

/** All the question's text from outside as it will be drawn, every string through one escape budget in the order it
 *  appears. Text from a repository is drawn as text nodes with each hidden or direction-changing character shown as an
 *  escape, so a file name or a command cannot read as something else than what it is; a path, being one name, escapes
 *  line breaks too. Done once for an envelope, however often the open overlay renders. */
function revealQuestion(envelope: TrustPromptEnvelope) {
  const fit = escapeBudget();
  const path = (text: string) => fit(revealPath(text));
  const question = fit(revealHidden(questionOf(envelope)));
  const root = path(envelope.root);
  const top = path(envelope.top);
  const changed = envelope.changed === null ? null : {
    added: envelope.changed.added.map(path),
    removed: envelope.changed.removed.map(path),
    changed: envelope.changed.changed.map(path),
  };
  const groups: DrawnGroup[] = groupItemsByFile(envelope.items).map((group) => ({
    file: path(group.file),
    items: group.items.map((item) => {
      // An unreadable file is named for what it is, whatever label Rust gave it: nothing about it was read.
      const label = item.what === "unreadable" ? "cannot be checked" : item.label;
      const pieces = revealHidden(`${label}: ${item.value}`);
      return { item, hidden: countEscapes(pieces), pieces: fit(pieces) };
    }),
  }));
  const note = envelope.rememberNote === null ? null : fit(revealHidden(envelope.rememberNote));
  const texts: DrawnText[] = (envelope.changed?.texts ?? []).map((text) => {
    if (text.kind === "plain") return { kind: "plain", pieces: path(`${text.path}: ${text.line}`) };
    const pathPieces = revealPath(text.path);
    let hidden = countEscapes(pathPieces);
    const lines = text.hunks.map((hunk) => hunk.lines.map((line) => {
      const pieces = revealHidden(line.text);
      hidden += countEscapes(pieces);
      return fit(pieces);
    }));
    return { kind: "diff", entry: text, path: fit(pathPieces), lines, hidden };
  });
  return { question, root, top, changed, groups, note, texts };
}

function ItemRow({ item, pieces, hidden }: DrawnItem) {
  return (
    <li className="trust-item" data-what={item.what}>
      <div className="trust-item-text">
        <Revealed pieces={pieces} />
      </div>
      {item.outside && <div className="trust-outside">points outside the repository</div>}
      <HiddenWarning count={hidden} what="text" />
    </li>
  );
}

function PathList({ title, paths }: { title: string; paths: Pieces[] }) {
  if (paths.length === 0) return null;
  return (
    <>
      <h4>{title}</h4>
      <ul className="trust-paths">
        {paths.map((path, i) => (
          <li key={i}>
            <Revealed pieces={path} />
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

function TrustTextDiff({ entry, path, lines, hidden }: Extract<DrawnText, { kind: "diff" }>) {
  const { added, removed, hunks, skippedAfter } = entry;
  return (
    <div className="trust-text-diff">
      <div className="trust-text-diff-head">
        <span className="trust-text-path"><Revealed pieces={path} /></span>
        <span className="permission-card-edit-counts">
          <span className="edit-count-added"><span className="edit-sign">+</span>{added}</span>{" "}
          <span className="edit-count-removed"><span className="edit-sign">−</span>{removed}</span>
        </span>
      </div>
      <HiddenWarning count={hidden} what="change" />
      {hunks.map((hunk, i) => (
        <Fragment key={i}>
          <DiffGap count={hunk.skipped} />
          <DiffRows lines={hunk.lines} pieces={lines[i]} />
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
  const drawn = useMemo(() => revealQuestion(envelope), [envelope]);
  return (
    <div className="trust-prompt" ref={ref} role="dialog" aria-label={questionOf(envelope)}>
      <h2>
        <Revealed pieces={drawn.question} />
      </h2>
      <p className="trust-root">
        <span className="trust-key">root</span> <Revealed pieces={drawn.root} />
      </p>
      {envelope.top !== envelope.root && (
        <p className="trust-root">
          <span className="trust-key">top</span> <Revealed pieces={drawn.top} />
        </p>
      )}
      {drawn.changed !== null && (
        <section className="trust-changed">
          <h3>changed since you trusted it</h3>
          <PathList title="added" paths={drawn.changed.added} />
          <PathList title="removed" paths={drawn.changed.removed} />
          <PathList title="changed" paths={drawn.changed.changed} />
        </section>
      )}
      {drawn.groups.map((group, i) => (
        <section key={i} className="trust-file">
          <h3>
            <Revealed pieces={group.file} />
          </h3>
          <ul className="trust-items">
            {group.items.map((item, j) => (
              <ItemRow key={j} {...item} />
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
      {envelope.remember !== "yes" && drawn.note !== null && (
        <p className="trust-note trust-remember-note">
          <Revealed pieces={drawn.note} />
        </p>
      )}
      {drawn.texts.length > 0 && (
        <section className="trust-texts">
          <h3>what changed in the instruction files</h3>
          {drawn.texts.map((text, i) => text.kind === "plain" ? (
            <p key={i} className="trust-text-line"><Revealed pieces={text.pieces} /></p>
          ) : (
            <TrustTextDiff key={i} {...text} />
          ))}
        </section>
      )}
      <p className="trust-footer">{trustFooter(envelope.remember, envelope.needsYes === true, envelope.state)}</p>
    </div>
  );
});
