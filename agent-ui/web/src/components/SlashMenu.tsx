import type { CompletionCommand } from "../slashCommands";
import { revealHidden } from "../revealHidden";
import { Revealed } from "./Revealed";

type Props = {
  /** The rows to draw (the window the composer chose), in order. */
  rows: readonly CompletionCommand[];
  /** The index in the whole match list of `rows[0]`, and the list's length: they tell a reader (and a
   *  screen reader) which part of a longer list this is. */
  firstIndex: number;
  total: number;
  /** The selected row's index in the whole match list. */
  selected: number;
  onPick: (index: number) => void;
  /** The wheel moved the selection by one row either way. */
  onWheel: (by: 1 | -1) => void;
};

/** The `/command` menu above the composer: `/name`, an argument hint when the agent gave one, and a muted
 *  line saying what the command does. Every string is an agent's or the panel's own text and is drawn as
 *  text -- never parsed as markup -- since an agent's names and descriptions are not trusted. A hint and a
 *  description go through `revealHidden` as an agent's labels do on a permission card, so an invisible or
 *  direction-changing character in one shows as `⟨U+…⟩` (a name holding one was dropped before it got
 *  here). A press on the menu does not take focus from the box, so the keys keep going to the draft. */
export function SlashMenu({ rows, firstIndex, total, selected, onPick, onWheel }: Props) {
  return (
    <div
      className="slash-menu"
      role="listbox"
      aria-label="Commands"
      onMouseDown={(e) => e.preventDefault()}
      onWheel={(e) => {
        if (e.deltaY !== 0) onWheel(e.deltaY > 0 ? 1 : -1);
      }}
    >
      {rows.map((command, i) => {
        const index = firstIndex + i;
        const hint = command.hint === undefined ? null : revealHidden(command.hint);
        const description = revealHidden(command.description);
        return (
          <div
            key={command.name}
            role="option"
            aria-selected={index === selected}
            aria-posinset={index + 1}
            aria-setsize={total}
            className={index === selected ? "slash-menu-row current" : "slash-menu-row"}
            onClick={() => onPick(index)}
          >
            <span className="slash-menu-name">
              /{command.name}
              {hint !== null && (
                <span className="slash-menu-hint">
                  {" "}
                  <Revealed pieces={hint} />
                </span>
              )}
            </span>
            <span className="slash-menu-desc">
              <Revealed pieces={description} />
            </span>
          </div>
        );
      })}
    </div>
  );
}
