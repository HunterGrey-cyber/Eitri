/** The page's base text size, in CSS px. `markdown.css` sizes everything as a ratio of `--mdp-text`, which is the
 *  host's `--nv-text-size` (the editor's own text size, from a window with an editor of its own) or 14px without
 *  it. A `font_size` setting is stronger than either: it sits on the root element, where it outranks the style
 *  sheet's rule, and taking it away gives the style sheet's back. */
export const TEXT_PROPERTY = "--mdp-text";

export function applyFontSize(root: HTMLElement, px: number | null): void {
  if (px === null) root.style.removeProperty(TEXT_PROPERTY);
  else root.style.setProperty(TEXT_PROPERTY, `${px}px`);
}
