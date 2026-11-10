import { revealedText } from "../revealHidden";

/** `y` with several objects to copy (two code blocks, two paths): each entry's label after its letter, while the
 *  letters typed so far still lead to it. The labels are untrusted text, drawn through `revealedText`; what a letter
 *  copies is the entry's own text, never the label. */
export function CopyPick({ entries, labels, typed }: { entries: { label: string }[]; labels: string[]; typed: string }) {
  return (
    <div className="path-pick" role="status">
      {entries.map((entry, i) => labels[i].startsWith(typed)
        ? <span key={i}>{i > 0 ? " · " : ""}{labels[i]} {revealedText(entry.label)}</span> : null)}
    </div>
  );
}
