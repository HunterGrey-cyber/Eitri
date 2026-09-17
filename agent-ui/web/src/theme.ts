/** CSS custom properties Rust derived from the embedded nvim's highlight groups
 *  (`core/src/theme/tokens.rs`). The set is always complete, so nothing here supplies defaults. */
export type ThemeVars = Record<string, string>;

/** Anything with `setProperty` -- `document.documentElement.style` in the app, a recorder in tests. */
export type StyleTarget = { setProperty(name: string, value: string): void };

/** Only our own namespace. A name like `color` would not be a custom property at all, and a
 *  foreign `--x` would be a variable this app never reads -- either means the envelope is wrong. */
const VARIABLE_NAME = /^--nv-[a-z0-9-]+$/;

/** Applies `vars` and returns how many were set. */
export function applyTheme(vars: ThemeVars, target: StyleTarget = document.documentElement.style): number {
  let applied = 0;
  for (const [name, value] of Object.entries(vars)) {
    if (!VARIABLE_NAME.test(name) || typeof value !== "string") {
      console.warn("agent-ui: ignoring theme variable outside --nv-*", name);
      continue;
    }
    target.setProperty(name, value);
    applied += 1;
  }
  return applied;
}
