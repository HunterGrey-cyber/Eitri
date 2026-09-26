import { forwardRef } from "react";
import { BROWSE_KEYS, INPUT_KEYS } from "../keymap";
import type { KeyHelp } from "../keymap";

type Props = {
  /** Requested by a backdrop click only -- `App.tsx` owns `?`/`Escape`/`q`, which it intercepts
   *  before `resolveKey` ever runs (spec §3.1, §3.3), and calls this the same way. */
  onClose: () => void;
  /** "Anywhere in the window" and "After <prefix>": `shell`'s keys, from its `keymap` envelope. */
  windowKeys: KeyHelp[];
  prefixKeys: KeyHelp[];
  /** The prefix as a person reads it (`Ctrl+b`), for the last heading. */
  prefixLabel: string;
};

/** One of the four groups (spec §3.2), rendered from the same tables `keymap.test.ts` binds to
 *  `resolveKey` both ways -- this component adds no keys of its own, only a title and a layout. */
function Section({ title, rows }: { title: string; rows: KeyHelp[] }) {
  return (
    <section>
      <h2>{title}</h2>
      <table>
        <tbody>
          {rows.map((row) => (
            <tr key={row.keys}>
              <td>
                <kbd className="keycap">{row.keys}</kbd>
              </td>
              <td>{row.what}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

/**
 * The full `?` keymap (spec §3): four groups, each a two-column table. The first two come from the
 * one source of truth `BROWSE_KEYS`/`INPUT_KEYS` in `./keymap` -- so this list can neither promise a
 * key the panel does not have (§3.3's `resolveKey` -> table direction) nor omit one it does (the
 * reverse direction). The last two -- "Anywhere in the window" and "After <prefix>" -- come from
 * `shell`'s own `keymap` envelope (keymap spec §2.9), generated from `neovibe_core::keymap`: nothing
 * on this page can read what GTK binds. It draws only; `App.tsx` decides when it is open, swallows
 * every key while it is (so `a`/`d` cannot reach a card hidden underneath -- spec §3.1) and scrolls
 * it on `j`/`k` through the forwarded ref. The one thing this component decides for itself is a
 * click on its own backdrop, which spec §3.1 also calls a close ("点击表外"): `event.target ===
 * event.currentTarget` is exactly a click that landed on this element and not on anything it
 * contains, so a click inside a table (reading a row, selecting text) never fires it.
 */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay(
  { onClose, windowKeys, prefixKeys, prefixLabel },
  ref,
) {
  return (
    <div
      className="keymap-overlay"
      role="dialog"
      aria-label="Keys"
      ref={ref}
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <Section title="This panel" rows={BROWSE_KEYS} />
      <Section
        title="Typing"
        rows={[
          ...INPUT_KEYS,
          { keys: "Ctrl+h / j / k / l", what: "Move between panes (neovibe keeps these)" },
          { keys: prefixLabel, what: "The prefix (neovibe keeps it)" },
        ]}
      />
      <Section title="Anywhere in the window" rows={windowKeys} />
      <Section title={`After ${prefixLabel}`} rows={prefixKeys} />
    </div>
  );
});
