import type { QueueItem } from "../types";

function firstLine(text: string): string {
  const [first, ...rest] = text.split("\n");
  return rest.length > 0 ? `${first} …` : first;
}

/** Queued messages marked `⧗`, sent when the turn ends, oldest first.
 *  A failed flush shows why it failed and how to retry. */
export function QueueLines({ items, error, onEdit }: { items: QueueItem[]; error: string | null; onEdit?: () => void }) {
  if (items.length === 0 && error === null) return null;
  return (
    <div className="queue-lines" aria-label="Queued messages">
      {items.length > 0 && (
        <>
          <div className="queue-heading">
            <span>{items.length} queued</span>
            {onEdit && <button type="button" aria-label="Edit queued messages" onClick={onEdit}>Edit queued</button>}
          </div>
          <div className="queue-items">
            {items.map((item) => (
              <div key={item.queuedAt + item.text} className="queue-line" title={item.text}>
                ⧗ {firstLine(item.text)}
              </div>
            ))}
          </div>
        </>
      )}
      {error !== null && (
        <div className="queue-error" role="alert">
          ⚠ not sent: {error} — Enter or Ctrl+Enter tries again
        </div>
      )}
    </div>
  );
}
