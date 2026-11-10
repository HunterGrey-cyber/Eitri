import { revealedText } from "../revealHidden";
import type { QueueItem } from "../types";

function firstLine(text: string): string {
  const [first, ...rest] = text.split("\n");
  return rest.length > 0 ? `${first} …` : first;
}

/** The lines above the box, oldest first (Claude Code lists queued messages "above the input box", "in
 *  gray"): `↪` for a message sent into the running turn that the agent has not read yet (it leaves when
 *  its row appears in the conversation), `⧗` for one held until the turn ends, a command among them while
 *  the messages around it go into the turn, and `⧗ after the turn:` for one sent with `Ctrl+x Enter`, which
 *  waits for the turn's end while what follows it goes in. `note` says once, quietly, why what is held is held; a refused
 *  flush says why and how to retry. */
export function QueueLines({ items, error, note = null }: { items: QueueItem[]; error: string | null; note?: string | null }) {
  if (items.length === 0 && error === null) return null;
  const held = items.some((item) => item.steered !== true);
  return (
    <div className="queue-lines" aria-label="Queued messages">
      {items.map((item) =>
        item.steered === true ? (
          <div key={item.queuedAt + item.text} className="queue-line queue-line-steered">
            <span className="queue-glyph">↪</span> <span className="queue-text">{revealedText(firstLine(item.text))}</span>
          </div>
        ) : item.afterTurn === true ? (
          <div key={item.queuedAt + item.text} className="queue-line queue-line-after">
            <span className="queue-glyph">⧗</span> <span className="queue-after">after the turn:</span>{" "}
            <span className="queue-text">{revealedText(firstLine(item.text))}</span>
          </div>
        ) : (
          <div key={item.queuedAt + item.text} className="queue-line">
            <span className="queue-glyph">⧗</span> <span className="queue-text">{revealedText(firstLine(item.text))}</span>
          </div>
        ),
      )}
      {note !== null && held && <div className="queue-note">{revealedText(note)}</div>}
      {error !== null && (
        <div className="queue-error" role="alert">
          ⚠ not sent: {revealedText(error)} — Enter or Ctrl+Enter tries again
        </div>
      )}
    </div>
  );
}
