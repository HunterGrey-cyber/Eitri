import type { QueueItem } from "../types";

function firstLine(text: string): string {
  const [first, ...rest] = text.split("\n");
  return rest.length > 0 ? `${first} …` : first;
}

/** Queued messages above the input box, oldest first. `↪` marks a message sent into the running
 *  turn that the agent has not read yet; it leaves when its row appears in the conversation.
 *  `⧗` marks a message held until the turn ends. `note` quietly explains why messages are held;
 *  a refused flush explains why it failed and how to retry. */
export function QueueLines({ items, error, note = null }: { items: QueueItem[]; error: string | null; note?: string | null }) {
  if (items.length === 0 && error === null) return null;
  const held = items.some((item) => item.steered !== true);
  return (
    <div className="queue-lines" aria-label="Queued messages">
      {items.map((item) =>
        item.steered === true ? (
          <div key={item.queuedAt + item.text} className="queue-line queue-line-steered">
            <span className="queue-glyph">↪</span> <span className="queue-text">{firstLine(item.text)}</span>
          </div>
        ) : (
          <div key={item.queuedAt + item.text} className="queue-line">
            <span className="queue-glyph">⧗</span> <span className="queue-text">{firstLine(item.text)}</span>
          </div>
        ),
      )}
      {note !== null && held && <div className="queue-note">{note}</div>}
      {error !== null && (
        <div className="queue-error" role="alert">
          ⚠ not sent: {error} — Enter or Ctrl+Enter tries again
        </div>
      )}
    </div>
  );
}
