import type { ReactNode } from "react";
import type { PanelMode } from "../keymap";

/* Data over the mode union, which is why `hint` has a label before `f` can reach it -- see
   `PanelMode`'s own doc comment in `../keymap` for why that mode is unreachable today. */
const MODE_LABEL: Record<PanelMode, string> = { browse: "BROWSE", input: "INPUT", hint: "HINT" };

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
  /** The which-key strip, in BROWSE only (`App.tsx` gates it) -- absent everywhere else, including
   *  the start screen, which draws its own copy. */
  children?: ReactNode;
};

/** V2 (session tabs spec §3.3, ruling 8): always present. The mode block (moved from `StatusLine`
 *  unchanged), the mode pill, then the which-key strip. */
export function Footer({ mode, paneFocused = false, pill, children }: Props) {
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
      <span className="mode-pill">{pill}</span>
      {children}
    </div>
  );
}
