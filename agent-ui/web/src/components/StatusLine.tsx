import type { AgentUiState } from "../types";
import type { PanelMode } from "../keymap";

/* Data over the mode union, which is why `hint` has a label before `f` can reach it -- see
   `PanelMode`'s own doc comment in `../keymap` for why that mode is unreachable today. */
const MODE_LABEL: Record<PanelMode, string> = { browse: "BROWSE", input: "INPUT", hint: "HINT" };

type Props = {
  mode: PanelMode;
  /** Whether this pane has keyboard focus, from `shell` (`pane_focus`). When it is `false` the
   *  block still names the mode the panel will be in when focus returns, but it is drawn dim
   *  (`data-focused="false"` in index.css). A bright BROWSE is a claim that keys typed now go
   *  here. Optional and `false` by default, so a caller that never learns the answer shows the
   *  dim block, which claims nothing. */
  paneFocused?: boolean;
  state: AgentUiState;
  /** Where the cursor is in `buildTimeline(state)`, and how long that timeline currently is. A
   *  POSITION, never an identity: `permission_resolved` removes a card and every later index
   *  shifts, so the same number names a different row from one event to the next. Spec §9 keeps
   *  the stable-numbering fix deferred -- this must not be read, or later turned into, a durable
   *  name for an item. */
  position: { index: number; total: number };
  /** From the provider's advertised capabilities, never from the backend's name -- the same rule
   *  `Composer`'s own Stop button follows. */
  canInterrupt: boolean;
  onInterrupt: () => void;
};

export function StatusLine({ mode, paneFocused = false, state, position, canInterrupt, onInterrupt }: Props) {
  // Moved out of `SessionHeader.tsx` (panel-as-document task 6): a terminal status wins over
  // activeTurnId. `reducer.ts:177,184` (`session_unavailable`/`session_closed`) DO clear
  // activeTurnId -- an earlier note here argued the opposite, that nothing should clear it because
  // no provider event says "that turn is over", and that was wrong in its consequence: it left a
  // dead session reading "working" forever, in the very styling written for the status text it was
  // hiding, and kept `App.tsx`'s composer spinner and the supervisor dashboard's Working dot stuck
  // too. Clearing it invents no completion; the terminal status is still the thing being shown.
  // This guard is not load-bearing for THOSE two events any more, then -- it is what keeps a
  // session that dies some OTHER way (nothing here has found one) from reading "working" forever.
  const working = state.status.kind === "running" && state.activeTurnId !== null;
  return (
    <div className="status-line">
      <span
        className="mode-block"
        data-mode={mode}
        data-focused={paneFocused ? "true" : "false"}
        data-testid="mode-block"
        title={paneFocused ? undefined : "This pane does not have keyboard focus (Ctrl+l to focus it)"}
      >
        {MODE_LABEL[mode]}
      </span>
      <span
        className={`status status-${state.status.kind}`}
        title={
          state.status.kind === "unavailable" || state.status.kind === "closed"
            ? state.status.reason
            : undefined
        }
      >
        {working ? "working" : state.status.kind}
      </span>
      {/* A POSITION, never an identity: `permission_resolved` removes a card and every later index
          shifts. Spec §9 keeps the stable-numbering fix deferred, and this label must not read as a
          durable name for an item. */}
      <span className="position" data-testid="position">
        {position.total === 0 ? "—" : `${position.index + 1}/${position.total}`}
      </span>
      {canInterrupt && working && (
        <button type="button" className="stop" onClick={onInterrupt}>
          Stop
        </button>
      )}
    </div>
  );
}
