import { forwardRef } from "react";
import { countEscapes, revealHidden } from "../revealHidden";
import { groupItemsByFile, trustFooter } from "../trust";
import type { TrustItem, TrustPromptEnvelope } from "../trust";
import { HiddenWarning, Revealed } from "./Revealed";

/** Text from a repository, drawn as text nodes with each hidden or direction-changing character shown as an
 *  escape, so a file name or a command cannot read as something else than what it is. */
function Shown({ text }: { text: string }) {
  return <Revealed pieces={revealHidden(text)} />;
}

function ItemRow({ item }: { item: TrustItem }) {
  // An unreadable file is named for what it is, whatever label Rust gave it: nothing about it was read.
  const label = item.what === "unreadable" ? "cannot be checked" : item.label;
  const hidden = countEscapes(revealHidden(label)) + countEscapes(revealHidden(item.value));
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
            <Shown text={path} />
          </li>
        ))}
      </ul>
    </>
  );
}

type Props = { envelope: TrustPromptEnvelope };

/** The question before a project's own Claude configuration is loaded (hooks, MCP servers, rules), with what
 *  it would run. Drawn only: `App.tsx` owns every key while it is open and scrolls this box. Nothing here is
 *  set as HTML. */
export const TrustPrompt = forwardRef<HTMLDivElement, Props>(function TrustPrompt({ envelope }, ref) {
  const groups = groupItemsByFile(envelope.items);
  return (
    <div className="trust-prompt" ref={ref} role="dialog" aria-label="Trust this project's Claude configuration?">
      <h2>Trust this project's Claude configuration?</h2>
      <p className="trust-root">
        <span className="trust-key">root</span> <Shown text={envelope.root} />
      </p>
      {envelope.top !== envelope.root && (
        <p className="trust-root">
          <span className="trust-key">top</span> <Shown text={envelope.top} />
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
            <Shown text={group.file} />
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
      {envelope.remember !== "yes" && envelope.rememberNote !== null && (
        <p className="trust-note">
          <Shown text={envelope.rememberNote} />
        </p>
      )}
      <p className="trust-footer">{trustFooter(envelope.remember)}</p>
    </div>
  );
});
