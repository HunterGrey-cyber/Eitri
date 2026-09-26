import type { ReactNode } from "react";
import type { PanelMode } from "../keymap";

/* Data over the mode union, which is why `hint` has a label before `f` can reach it -- see
   `PanelMode`'s own doc comment in `../keymap` for why that mode is unreachable today. */
const MODE_LABEL: Record<PanelMode, string> = { browse: "BROWSE", input: "INPUT", hint: "HINT" };

/** F4: the footer's INPUT hints, idle and while a turn runs. */
export const INPUT_IDLE_HINT = "Enter send · Shift+Enter newline · Esc browse · Ctrl+g nvim";
export const INPUT_RUNNING_HINT = "Enter queue · Ctrl+Enter now · Ctrl+c interrupt";

type Props = {
  mode: PanelMode;
  /** Whether this pane has keyboard focus AND the window is active, from `shell` (`pane_focus`;
   *  the window-active half since 2026-09-19 later -- before it, alt-tabbing away left this `true`
   *  and the block bright, which contradicted the sentence below). When it is `false` the
   *  block still names the mode the panel will be in when focus returns, but it is drawn dim
   *  (`data-focused="false"` in index.css). A bright BROWSE is a claim that keys typed now go
   *  here. Optional and `false` by default, so a caller that never learns the answer shows the
   *  dim block, which claims nothing. */
  paneFocused?: boolean;
  /** Claude Code's mode pill (`tabs.ts`'s `modePill`): `"⏵⏵ <mode> on"`, or "(shift+tab to cycle)"
   *  before a session starts. */
  pill: string;
  /** F4/ruling 29: the INPUT hint, shown only while nothing more urgent claims the third slot
   *  (`children`, then `flash`, then this). `undefined` in BROWSE and on the start screen, which
   *  draw their own copy of the third slot instead. */
  hint?: string;
  /** Ruling 29: a transient (2s) line -- `copied N chars`, `mode is fixed for this session`, a
   *  `notice{text}` from Rust -- that outranks the which-key strip and the INPUT hint but never
   *  the window-close prompt (`children`). `null`/`undefined` when nothing is flashing. */
  flash?: string | null;
  /** The third slot's own content when the caller wants to draw it directly -- the window-close
   *  prompt (ruling 7) or, in BROWSE, the which-key strip (`App.tsx` gates both). Wins over `flash`
   *  and `hint`: slot priority is children > flash > hint > nothing (ruling 29, F4). */
  children?: ReactNode;
};

/** V2 (session tabs spec §3.3, ruling 8): always present. The mode block (moved from `StatusLine`
 *  unchanged), the mode pill, then the third slot -- the close prompt, a flash, the INPUT hint, or
 *  nothing, in that priority. */
export function Footer({ mode, paneFocused = false, pill, hint, flash = null, children }: Props) {
  return (
    <div className="panel-footer">
      <span
        className="mode-block"
        data-mode={mode}
        data-focused={paneFocused ? "true" : "false"}
        data-testid="mode-block"
        title={paneFocused ? undefined : "This pane does not have keyboard focus (Ctrl+l to focus it)"}
      >
        {MODE_LABEL[mode]}
      </span>
      <span className="mode-pill" data-testid="mode-pill">
        {pill}
      </span>
      {children ??
        (flash !== null ? (
          <span className="footer-flash" role="status">
            {flash}
          </span>
        ) : hint !== undefined ? (
          <span className="footer-hint">{hint}</span>
        ) : null)}
    </div>
  );
}
