import { forwardRef, useEffect, useRef } from "react";
import type { DetailRow } from "../types";

type Props = {
  rows: DetailRow[];
  /** The index `j`/`k`/`y` act on, held by `App.tsx` -- this component draws the row but owns no
   *  key handling of its own (the same split `KeymapOverlay` makes: `App.tsx` owns `j`/`k`/`y`/
   *  `Esc`/`q` while this is open). */
  current: number;
  /** Requested by a backdrop click only, the same rule `KeymapOverlay`'s own `onClose` follows:
   *  `event.target === event.currentTarget` is exactly a click that landed on this element and not
   *  on anything it contains, so a click inside the table never fires it. */
  onClose: () => void;
};

/** This session's details (session tabs spec §3.3), opened by `prefix i` or `Enter` on the status
 *  row: a two-column table of `rows`, shaped like `KeymapOverlay`. */
export const DetailPopover = forwardRef<HTMLDivElement, Props>(function DetailPopover({ rows, current, onClose }, ref) {
  // The row `j`/`k` sit on stays in view: once phase 3's rows made the table taller than a short
  // panel, the highlight walked off the bottom with nothing following it (the phase-3 GUI pass).
  const currentRef = useRef<HTMLTableRowElement | null>(null);
  useEffect(() => {
    // `?.()`: jsdom has no `scrollIntoView`.
    currentRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [current]);
  return (
    <div
      className="detail-popover"
      role="dialog"
      aria-label="Session details"
      ref={ref}
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <table>
        <tbody>
          {rows.map((row, index) => (
            <tr key={row.label} ref={index === current ? currentRef : undefined} className={index === current ? "current" : undefined} aria-current={index === current ? "true" : undefined}>
              <td>{row.label}</td>
              <td>{row.value}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
});
