import { createElement } from "react";
import type { ReactNode } from "react";

/** A shared row on the sign-column grid.
 *
 * The `sign` argument supplies both `data-sign` and the visible `.row-sign` text, so tests and
 * the displayed glyph cannot disagree. The visible copy is `aria-hidden` because the row's
 * text and role already convey its meaning.
 *
 * `current` shows which row `Enter` expands and `y` copies. `--nv-cursorline` is a background
 * highlight, so the guard against signal colours as text does not apply. `App.tsx` clamps the
 * cursor when the timeline shrinks so these keys cannot act on a stale index.
 */
type RowProps = {
  /** The row's kind, as the `row-<kind>` class the stylesheet keys on. */
  kind: string;
  /** The glyph in the sign column. The ONLY source for both `data-sign` and the visible span. */
  sign: string;
  /** The conversation cursor sits on this row. Adds `row-current` and, on the `div` shape,
   *  `aria-current`. Unused by the choice rows, which carry `aria-checked` instead -- a radio's
   *  selection is not a cursor. */
  current?: boolean;
  /** Classes beyond `row row-<kind>`, for a row that is also something else (`resume`, `selected`).
   *  Deliberately not a way to restyle the grid: `display`/`grid-template-columns`/`gap` come from
   *  `.row` and nothing here should be re-declaring them. */
  className?: string;
  /** Extra classes on the BODY cell -- `.handoff-card`'s own box (border, background, spacing),
   *  which has nothing to do with the sign column and must stay on the body rather than replace
   *  the row, so every `.handoff-card p`/`.warning`/`.detail` child selector still matches. */
  bodyClassName?: string;
  /** `div` (the default) or `button`. The button shape exists for `ModeSelector`'s choice rows,
   *  which are real controls, and it switches the two cells from `<div>` to `<span>` as well: a
   *  `<button>`'s content model is phrasing content, so block-level children in one are invalid
   *  HTML. That is the whole reason this is an element prop rather than two components. */
  as?: "div" | "button";
  /** `alert` / `status` for the div shape, `radio` for the button shape. */
  role?: string;
  "aria-checked"?: boolean;
  onClick?: () => void;
  children: ReactNode;
};

export function Row({
  kind,
  sign,
  current = false,
  className,
  bodyClassName,
  as = "div",
  role,
  "aria-checked": ariaChecked,
  onClick,
  children,
}: RowProps) {
  const classes = ["row", `row-${kind}`, current ? "row-current" : null, className ?? null]
    .filter((c): c is string => c !== null && c !== "")
    .join(" ");
  // `createElement` rather than two JSX branches: the tag is the only thing that varies between the
  // two shapes, and writing the cells out twice would put the glyph back in two places -- the exact
  // defect this component exists to make impossible.
  const cellTag = as === "button" ? "span" : "div";
  const cells = [
    createElement(cellTag, { key: "sign", className: "row-sign", "aria-hidden": true }, sign),
    createElement(
      cellTag,
      { key: "body", className: bodyClassName === undefined ? "row-body" : `row-body ${bodyClassName}` },
      children,
    ),
  ];

  if (as === "button") {
    return (
      <button type="button" className={classes} data-sign={sign} role={role} aria-checked={ariaChecked} onClick={onClick}>
        {cells}
      </button>
    );
  }
  return (
    <div className={classes} data-sign={sign} role={role} aria-current={current || undefined}>
      {cells}
    </div>
  );
}
