import { editorMentionHint } from "../atMenu";
import { revealHidden } from "../revealHidden";
import { Revealed } from "./Revealed";

type Props = {
  rows: readonly string[];
  firstIndex: number;
  total: number;
  selected: number;
  onPick: (index: number) => void;
  onWheel: (by: 1 | -1) => void;
};

/** Names are repository text, including hidden characters, and never become markup. */
export function AtMenu({ rows, firstIndex, total, selected, onPick, onWheel }: Props) {
  return (
    <div
      className="slash-menu"
      role="listbox"
      aria-label="Files"
      onMouseDown={(e) => e.preventDefault()}
      onWheel={(e) => {
        if (e.deltaY !== 0) onWheel(e.deltaY > 0 ? 1 : -1);
      }}
    >
      {rows.map((path, i) => {
        const index = firstIndex + i;
        return (
          <div
            key={path}
            role="option"
            aria-selected={index === selected}
            aria-posinset={index + 1}
            aria-setsize={total}
            className={index === selected ? "slash-menu-row current" : "slash-menu-row"}
            onClick={() => onPick(index)}
          >
            <span className="slash-menu-name"><Revealed pieces={revealHidden(path)} /></span>
            {editorMentionHint(path) !== undefined && <span className="slash-menu-desc">{editorMentionHint(path)}</span>}
          </div>
        );
      })}
    </div>
  );
}
