import { metaLine, shownName } from "../attachments";
import { revealHidden } from "../revealHidden";
import type { DraftImage } from "../types";
import { Revealed } from "./Revealed";

type Props = {
  /** The draft's images, in the order they go. */
  items: readonly DraftImage[];
  /** Images this page is reading and the host has not named yet: drawn as importing, with nothing to remove. */
  pending?: number;
  onRemove: (id: string) => void;
  /** Back to the box (`i`, `a`, Enter, or `l`/→ past the last chip). */
  onLeave?: () => void;
};

/** The chip beside `chip` on `side`, if there is one. */
function besideChip(chip: HTMLElement, side: "before" | "after"): HTMLElement | null {
  let next = side === "before" ? chip.previousElementSibling : chip.nextElementSibling;
  while (next !== null && !(next instanceof HTMLElement && next.dataset.attachmentId !== undefined)) {
    next = side === "before" ? next.previousElementSibling : next.nextElementSibling;
  }
  return next as HTMLElement | null;
}

/** The keys a focused chip takes, as a row of items in vim: `h`/`l` (or ←/→) move along the chips and `l` past the
 *  last goes back to the box, as do `i`, `a` and Enter; `x` (or Delete, Backspace) takes the chip out and the keys
 *  stay on the chips. Nothing held: a chord is the panel's. */
function chipKey(e: React.KeyboardEvent<HTMLElement>, id: string, onRemove: (id: string) => void, onLeave?: () => void) {
  if (e.ctrlKey || e.altKey || e.metaKey) return;
  const chip = e.currentTarget;
  const leave = () => onLeave?.();
  switch (e.key) {
    case "x":
    case "Delete":
    case "Backspace": {
      if (e.repeat) break;
      const next = besideChip(chip, "after") ?? besideChip(chip, "before");
      onRemove(id);
      if (next !== null) next.focus();
      else leave();
      break;
    }
    case "h":
    case "ArrowLeft":
      besideChip(chip, "before")?.focus();
      break;
    case "l":
    case "ArrowRight": {
      const next = besideChip(chip, "after");
      if (next !== null) next.focus();
      else leave();
      break;
    }
    case "i":
    case "a":
    case "Enter":
      leave();
      break;
    default:
      return;
  }
  e.preventDefault();
  e.stopPropagation();
}

/** The composer's draft images, above its box: a thumbnail, the name and what the image is once ready; "importing"
 *  while it is prepared; the host's sentence once it failed. A chip takes the keys from the box's ← at its very start
 *  (or a click), and then `chipKey`'s keys; its own `×` takes it out too. Names and reasons are plain text with their hidden
 *  characters shown; the thumbnail is the host's own PNG. */
export function AttachmentChips({ items, pending = 0, onRemove, onLeave }: Props) {
  if (items.length === 0 && pending === 0) return null;
  return (
    <div className="attachment-chips" role="list" aria-label="Images to send">
      {items.map((item) => {
        const name = shownName(item.name);
        const meta = item.state === "ready" ? metaLine(item) : null;
        return (
          <div
            key={item.attachmentId}
            role="listitem"
            className={`attachment-chip attachment-chip-${item.state}`}
            data-attachment-id={item.attachmentId}
            tabIndex={0}
            aria-label={`image ${item.name}, ${item.state}`}
            onKeyDown={(e) => chipKey(e, item.attachmentId, onRemove, onLeave)}
          >
            {item.state === "ready" && item.thumbnail !== undefined && (
              <img className="attachment-thumb" src={item.thumbnail} alt="" draggable={false} />
            )}
            <span className="attachment-text">
              <span className="attachment-name">
                <Revealed pieces={revealHidden(name)} />
              </span>
              {meta !== null && <span className="attachment-meta">{meta}</span>}
              {item.state === "importing" && <span className="attachment-meta">importing…</span>}
              {item.state === "failed" && (
                <span className="attachment-reason" role="alert">
                  <Revealed pieces={revealHidden(item.reason ?? "this image could not be used")} />
                </span>
              )}
            </span>
            <button
              type="button"
              className="attachment-remove"
              tabIndex={-1}
              aria-label={`remove image ${item.name}`}
              onClick={() => onRemove(item.attachmentId)}
            >
              ×
            </button>
          </div>
        );
      })}
      {Array.from({ length: pending }, (_, i) => (
        <div key={`reading-${i}`} role="listitem" className="attachment-chip attachment-chip-importing">
          <span className="attachment-text">
            <span className="attachment-meta">importing…</span>
          </span>
        </div>
      ))}
    </div>
  );
}
