import { forwardRef, useEffect, useRef } from "react";
import type { DetailRow } from "../types";

type Props = {
  rows: DetailRow[];
  /** The index `j`/`k`/`y` act on, held by `App.tsx` -- this component draws the row but owns no
   *  key handling of its own (the same split `KeymapOverlay` makes: `App.tsx` owns `j`/`k`/`y`/
   *  `Esc`/`q` while this is open). Since `onHandoff` is present, `current === rows.length` is on
   *  the trailing row -- `App.tsx`'s own `j`/`k` bound `detailCursor` to `[0, rows.length]` rather
   *  than `[0, rows.length - 1]` for that to ever be reachable, and its `Enter` handling reads the
   *  same equality to decide whether to call `onHandoff` instead of nothing. */
  current: number;
  /** Requested by a backdrop click only, the same rule `KeymapOverlay`'s own `onClose` follows:
   *  `event.target === event.currentTarget` is exactly a click that landed on this element and not
   *  on anything it contains, so a click inside the table never fires it. */
  onClose: () => void;
  /** Called by Enter or a click on the trailing Continue in a terminal action row. Omit the
   *  callback to hide that row. `App.tsx` opens `ContinueInTerminal`'s confirmation here rather
   *  than handing off immediately, so a click cannot close a live conversation without
   *  confirmation. */
  onHandoff?: () => void;
};

/** This session's details, opened by `prefix i` or a click on the band: a
 *  two-column table of `rows`, shaped like `KeymapOverlay`, plus an optional trailing action row
 *. */
export const DetailPopover = forwardRef<HTMLDivElement, Props>(function DetailPopover({ rows, current, onClose, onHandoff }, ref) {
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
          {onHandoff !== undefined && (
            <tr
              className={current === rows.length ? "current detail-handoff" : "detail-handoff"}
              ref={current === rows.length ? currentRef : undefined}
              aria-current={current === rows.length ? "true" : undefined}
              onClick={onHandoff}
            >
              <td colSpan={2}>Continue in a terminal…</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
});
