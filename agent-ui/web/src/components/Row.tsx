import { createElement, Fragment } from "react";
import type { ReactNode } from "react";
import type { Problem } from "../problems";

/** A row on the shared sign-column grid.
 * Derive both `data-sign` and the visible `.row-sign` from one `sign` argument so they cannot drift.
 * Tests can inspect the data attribute without reading rendered text. The visible glyph is
 * `aria-hidden` because the row's text and role already convey its meaning.
 * `current` marks the conversation cursor so users can see which row `Enter` expands or `y` copies.
 * `--nv-cursorline` supplies a background highlight, as it does for choice-row hover.
 * `App.tsx` clamps the cursor when the timeline shrinks so those actions do not use a stale index. */
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
  /** `div` (the default) or `button`. The button shape exists for `EmptyTab`'s (and formerly
   *  `ModeSelector`'s) choice rows, which are real controls, and it switches the two cells from `<div>` to `<span>` as well: a
   *  `<button>`'s content model is phrasing content, so block-level children in one are invalid
   *  HTML. That is the whole reason this is an element prop rather than two components. */
  as?: "div" | "button";
  /** `alert` / `status` for the div shape, `radio` for the button shape. */
  role?: string;
  "aria-checked"?: boolean;
  onClick?: () => void;
  /** Makes this row a keyboard stop (`../nav`): `"row"` for a conversation row, which carries the
   *  cursor; any other name for a row that is reached by focusing it, like a start-screen choice. */
  navStop?: string;
  /** A startup failure's `classify()` result: a headline and remedy drawn above unchanged `children`.
   *  `null` or omission draws nothing extra, including for unrecognized failures.
   *  Callers perform classification; this component only renders the supplied result. */
  problem?: Problem | null;
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
  navStop,
  problem,
  children,
}: RowProps) {
  const classes = ["row", `row-${kind}`, current ? "row-current" : null, className ?? null]
    .filter((c): c is string => c !== null && c !== "")
    .join(" ");
  // `createElement` rather than two JSX branches: the tag is the only thing that varies between the
  // two shapes, and writing the cells out twice would put the glyph back in two places -- the exact
  // defect this component exists to make impossible.
  const cellTag = as === "button" ? "span" : "div";
  const bodyContent =
    problem == null ? (
      children
    ) : (
      <Fragment>
        <div className="row-problem">
          <strong>{problem.headline}</strong>
          <div className="row-problem-remedy">{problem.remedy}</div>
        </div>
        {children}
      </Fragment>
    );
  const cells = [
    createElement(cellTag, { key: "sign", className: "row-sign", "aria-hidden": true }, sign),
    createElement(
      cellTag,
      { key: "body", className: bodyClassName === undefined ? "row-body" : `row-body ${bodyClassName}` },
      bodyContent,
    ),
  ];

  if (as === "button") {
    return (
      <button type="button" className={classes} data-sign={sign} data-nav-stop={navStop} role={role} aria-checked={ariaChecked} onClick={onClick}>
        {cells}
      </button>
    );
  }
  return (
    <div className={classes} data-sign={sign} data-nav-stop={navStop} role={role} aria-current={current || undefined}>
      {cells}
    </div>
  );
}
