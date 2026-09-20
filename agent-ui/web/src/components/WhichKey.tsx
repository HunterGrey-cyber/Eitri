import type { StripEntry } from "../whichKey";

type Props = {
  /** The row's own keys, in the order `stripEntries` returns them. Ignored while `prefix` is set --
   *  a prefix chord's own continuation always wins the line (spec §2.3). */
  entries: StripEntry[];
  /** Same claim the mode block makes about `paneFocused` (`StatusLine`'s own doc comment): dim,
   *  never hidden, because a key that would do something irreversible must stay readable even while
   *  the editor has the keys (spec §2.1). */
  focused: boolean;
  /** `"g"` for the 400ms window after a lone `g` in BROWSE (`App.tsx`'s `gShown`), `null` otherwise
   *  (spec §2.3). Nothing else is a prefix today (`]`/`[` are not implemented, so they never page). */
  prefix: "g" | null;
};

/** The always-visible line above the status line: the keys the row under the cursor answers to,
 *  plus `? keys` at the far end (spec §2). Holds no control of its own -- no `data-nav-stop`. */
export function WhichKey({ entries, focused, prefix }: Props) {
  return (
    <div className="which-key" data-focused={focused ? "true" : "false"} aria-live="off">
      {prefix === "g" ? (
        <span className="which-key-prefix">
          <kbd className="keycap">g</kbd>… <kbd className="keycap">g</kbd> first row
        </span>
      ) : (
        entries.map((entry, index) => (
          <span className="which-key-entry" key={`${entry.key}-${index}`}>
            {index > 0 && " · "}
            <kbd className="keycap">{entry.key}</kbd> <span className="which-key-label">{entry.label}</span>
          </span>
        ))
      )}
      <span className="which-key-help">
        <kbd className="keycap">?</kbd> keys
      </span>
    </div>
  );
}
