export const MERMAID_CONFIG = {
  // Diagrams are drawn only when the page asks, never by the library scanning the page on its own.
  startOnLoad: false,
  // No script, click handler or link from a diagram's text runs or is kept.
  securityLevel: "strict",
  // Labels as SVG text rather than HTML inside `foreignObject`, which the sanitiser would remove anyway.
  htmlLabels: false,
  flowchart: { htmlLabels: false },
  // A cap on the text one diagram may hold, so a huge block cannot stall the page.
  maxTextSize: 50000,
  // The one theme whose colours all come from the variables below, which follow the editor's.
  theme: "base",
  // A failed diagram is shown by the page; the library must not insert its own error drawing into the document.
  suppressErrorRendering: true,
} as const;

const THEME: readonly [string, string][] = [
  ["background", "--nv-bg"], ["textColor", "--nv-fg"], ["lineColor", "--nv-fg"],
  ["primaryColor", "--nv-surface"], ["primaryTextColor", "--nv-surface-fg"], ["primaryBorderColor", "--nv-border"],
  ["mainBkg", "--nv-surface"], ["nodeBorder", "--nv-border"],
  ["secondaryColor", "--nv-tint"], ["secondaryTextColor", "--nv-tint-fg"], ["secondaryBorderColor", "--nv-border"],
  ["tertiaryColor", "--nv-cursorline"], ["tertiaryTextColor", "--nv-fg"], ["tertiaryBorderColor", "--nv-border"],
  ["clusterBkg", "--nv-tint"], ["clusterBorder", "--nv-border"],
  ["noteBkgColor", "--nv-surface"], ["noteTextColor", "--nv-surface-fg"], ["noteBorderColor", "--nv-border"],
  ["errorBkgColor", "--nv-error-tint"], ["errorTextColor", "--nv-error-tint-fg"],
  ["titleColor", "--nv-fg"], ["edgeLabelBackground", "--nv-bg"],
];

// Pie slices and colour scales take the syntax colours, which a theme already keeps apart from each other.
const PALETTE = ["--nv-syn-function", "--nv-syn-string", "--nv-syn-keyword", "--nv-syn-type", "--nv-syn-number",
  "--nv-syn-constant"];

/** The diagram theme's variables from the page's live `--nv-*` values; a value the page lacks is left to the
 *  library's own default. */
export function themeVariables(vars: Record<string, string>): Record<string, string | boolean> {
  const out: Record<string, string | boolean> = { darkMode: vars["--nv-color-scheme"] !== "light" };
  for (const [key, name] of THEME) if (vars[name]) out[key] = vars[name];
  PALETTE.forEach((name, i) => {
    if (!vars[name]) return;
    out[`pie${i + 1}`] = vars[name];
    out[`cScale${i}`] = vars[name];
  });
  if (vars["--nv-font-prose"]) out.fontFamily = vars["--nv-font-prose"];
  return out;
}

