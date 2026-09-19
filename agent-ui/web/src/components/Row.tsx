import { createElement } from "react";
import type { ReactNode } from "react";

/** One row on the sign-column grid every item in this document renders on (spec §3.2).
 *
 * **One component, not six.** This shape existed six times -- `MessageList`'s own module-private
 * copy plus five hand-written ones (`App.tsx`'s two session-ended rows, `TerminalHandoff`'s command
 * card, `ModeSelector`'s two choice rows) -- and each hand-written copy wrote its glyph TWICE, once
 * as `data-sign` and once as the visible `.row-sign` text. Tests assert on `data-sign`; the user
 * reads the span. Nothing checked that the two agreed, so editing one and not the other drifts past
 * a green suite. Here there is one `sign` argument and both readings are derived from it, which is
 * what makes the disagreement unrepresentable rather than merely unlikely.
 *
 * `data-sign` carries the glyph as data, separately from the `aria-hidden` presentational copy, so
 * a test can assert state without reading rendered text and a screen reader is not told it twice --
 * the glyph repeats what the row's own text and role already say, and announcing "rangle" before
 * every prompt is noise.
 *
 * `current` is the conversation's cursor: `Enter` expands "the current item" and `y` copies it
 * (`App.tsx`'s `onKeyDown`), and a cursor nobody can see is not a cursor, it is a trap -- there was
 * no way to know which row either key would act on. `--nv-cursorline` is a BACKGROUND (nvim's own
 * `Visual` highlight, already used this way by `.mode-selector button:hover`), so the "no signal
 * colour as text" guard `indexCss.test.ts` enforces does not apply to it. Scroll/resync/
 * shrinking-timeline behaviour beyond just showing where the cursor is stays out of scope (spec
 * §4.7, later sub-project) -- the cursor-clamping `App.tsx` already does when the timeline shrinks
 * is the one piece of that `y`/`Enter` themselves needed in order not to act on a stale index.
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
  /** Makes this row a keyboard stop (`../nav`): `"row"` for a conversation row, which carries the
   *  cursor; any other name for a row that is reached by focusing it, like a start-screen choice. */
  navStop?: string;
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
