import { forwardRef } from "react";
import { BROWSE_KEYS, INPUT_KEYS, PREFIX_KEYS, WINDOW_KEYS } from "../keymap";
import type { KeyHelp } from "../keymap";

type Props = {
  /** Requested by a backdrop click only -- `App.tsx` owns `?`/`Escape`/`q`, which it intercepts
   *  before `resolveKey` ever runs, and calls this the same way. */
  onClose: () => void;
};

/** One of the four groups, rendered from the same tables `keymap.test.ts` binds to
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

/** The full `?` keymap uses `BROWSE_KEYS`/`INPUT_KEYS`/`WINDOW_KEYS`/`PREFIX_KEYS` from
 * `./keymap` so displayed bindings match the available keys. `App.tsx` opens it, swallows keys
 * to protect cards underneath, and scrolls it on `j`/`k` through the forwarded ref. This
 * component closes on backdrop clicks only: `event.target === event.currentTarget` excludes
 * clicks inside tables and text selections. */
export const KeymapOverlay = forwardRef<HTMLDivElement, Props>(function KeymapOverlay({ onClose }, ref) {
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
      <Section title="Typing" rows={INPUT_KEYS} />
      <Section title="Anywhere in the window" rows={WINDOW_KEYS} />
      <Section title="After Ctrl+a" rows={PREFIX_KEYS} />
    </div>
  );
});
