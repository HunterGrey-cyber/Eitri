import type { BoxEntry } from "../leader";

type Props = {
  /** The sequence typed so far, as a person reads it (`sequenceTitle`'s "Space b", or a fixed
   *  prefix's own literal char, `"g"`/`"z"`/`"["`/`"]"`) -- drawn on the box's own top border,
   *  which-key.nvim's convention for its popup's title. */
  title: string;
  /** The node's continuations, in the order `boxEntries`/`FIXED_PENDING_ENTRIES` already decided:
   *  leaves before groups, spec §2.5. */
  entries: BoxEntry[];
  /** A click on a row: the same key `App.tsx` would feed the sequence engine had it been typed
   *  next. Does nothing to a disabled entry's own semantics -- `App.tsx` decides that, this
   *  component only reports the click. */
  onPick: (key: string) => void;
};

/** The which-key popup (panel round 2 plan, Task 8; spec §2.4, which-key.nvim's own default popup):
 *  drawn `WHICH_KEY_DELAY_MS` after a leader/table sequence, or one of `resolveKey`'s own reserved
 *  `g`/`z`/`[`/`]` prefixes, is still pending with nothing else to show yet. Holds no keyboard
 *  handling of its own -- `App.tsx`'s `onKeyDown` reads the same `PanelTable` this box is drawn
 *  from and decides every key itself; a click here is the only input this component originates. */
export function WhichKeyBox({ title, entries, onPick }: Props) {
  return (
    <div className="which-key-box">
      <span className="wk-title">{title}</span>
      {entries.map((entry, index) => (
        <div className="wk-entry" key={`${entry.key}-${index}`} onClick={() => onPick(entry.key)}>
          <span className="wk-key">{entry.key}</span>
          <span className="wk-sep">➜</span>
          <span className={entry.group ? "wk-group" : entry.disabled ? "wk-disabled" : undefined}>{entry.label}</span>
        </div>
      ))}
      <div className="wk-foot">esc close · bs back</div>
    </div>
  );
}
