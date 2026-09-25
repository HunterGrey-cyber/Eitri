import { useEffect, useState } from "react";
import type { KeyboardEvent } from "react";
import type { HandoffCommand, Hello, TabInfo } from "../types";
import type { PanelMode } from "../keymap";
import { Composer } from "./Composer";
import type { RestoredDraft } from "./Composer";
import { HandoffCommandCard } from "./TerminalHandoff";
import { Row } from "./Row";
import { SessionRowText } from "./SessionRow";
import { modePill } from "../tabs";
import { controlsOf, nextStop } from "../nav";

/** Resume rows under the fresh prompt (spec §3.6: "up to 8"). The chooser (`prefix w`) lists all. */
export const EMPTY_TAB_RESUME_ROWS = 8;

export type EmptyTabProps = {
  hello: Hello | null;
  tab: TabInfo;
  handoff: HandoffCommand | null;
  failure: string | null;
  paneFocused: boolean;
  focusRequest: number;
  restoredDraft: RestoredDraft | null;
  onSend: (text: string) => void;
  onResume: (providerSessionId: string) => void;
  onCycleMode: () => void;
  onReset: () => void;
  onHint: (repeat: boolean) => void;
  /** Mirrors `Composer`'s own prop of the same name (session tabs Task 11, ruling 24): this tab's
   *  draft is saved and restored across a switch like any other, so an empty tab's composer needs
   *  the same hook into it. */
  onDraftChange?: (text: string) => void;
  /** The window-close prompt (ruling 7), lifted out of `App.tsx` so both layouts' `onKeyDown`
   *  agree: called first, and if it returns `true` this component's own key handling stops there.
   *  Optional so a caller with no window (this component's own tests) needs no stand-in. */
  answerConfirm?: (event: KeyboardEvent<HTMLDivElement>) => boolean;
};

/** An empty session tab: Claude Code's fresh prompt (spec §3.6, F3). The composer is live in INPUT;
 *  the first send creates the session in Rust (ruling 4). `Shift+Tab` cycles the mode. Nothing here
 *  spawns a process. */
export function EmptyTab(props: EmptyTabProps) {
  const { hello, tab, handoff, failure } = props;
  const [mode, setMode] = useState<PanelMode>("input");
  const starting = tab.state === "starting";
  const failed = tab.state === "failed";
  const rows = (hello?.resumableSessions ?? []).slice(0, EMPTY_TAB_RESUME_ROWS);
  /* A request for the keys (`enter_input`, a chooser closing) is a request for INPUT, not only for
     focus: once anything took focus off the textarea (the chooser, a HINT, a click), `Composer`'s
     blur left this tab in BROWSE with no textarea to focus, and the keys landed on an ancestor that
     handles none (GUI pass, 2026-09-25). Refused where `i` is. */
  useEffect(() => {
    if (props.focusRequest > 0 && !starting && !failed) setMode("input");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.focusRequest]);

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (props.answerConfirm?.(event)) return;
    if (event.nativeEvent.isComposing) return;
    if (event.key === "Tab" && event.shiftKey && !starting && !failed) {
      event.preventDefault();
      props.onCycleMode();
      return;
    }
    // Once the composer is disabled (starting or failed) there is nothing left to type into, so
    // `mode` staying "input" must not swallow the keys the disabled screen still offers -- `r` on a
    // failed tab, `f` for HINT, `y` to copy a handoff command -- the same reasoning `Composer`'s own
    // doc comment gives for a dead session's INPUT being an empty mode.
    if (mode === "input" && !starting && !failed) return;
    const root = event.currentTarget;
    if (event.key === "j" || event.key === "k") {
      event.preventDefault();
      const target = nextStop(root, null, event.key === "j" ? 1 : -1);
      if (target !== null) controlsOf(target)[0]?.focus();
    } else if (event.key === "i" && !failed) {
      event.preventDefault();
      setMode("input");
    } else if (event.key === "r" && failed) {
      event.preventDefault();
      props.onReset();
    } else if (event.key === "f") {
      event.preventDefault();
      props.onHint(event.repeat);
    } else if (event.key === "y" && handoff !== null) {
      event.preventDefault();
      void navigator.clipboard?.writeText(handoff.command);
    }
  }

  return (
    <div className="empty-tab" tabIndex={0} onKeyDown={onKeyDown}>
      {handoff !== null && <HandoffCommandCard handoff={handoff} />}
      {failed && (
        <Row kind="error" sign="✗" role="alert">
          <strong>This tab's session did not start.</strong>
          <pre>{failure ?? "no reason was given"}</pre>
          <div className="row-hint">Press r to start a new session here.</div>
        </Row>
      )}
      {starting && (
        <p className="connecting">
          Starting the agent backend… The first start on a fresh Verdandi checkout also builds the sidecar.
        </p>
      )}
      <Composer
        disabled={starting || failed}
        sessionEnded={failed}
        closing={false}
        restoredDraft={props.restoredDraft}
        mode={mode}
        focusRequest={props.focusRequest}
        hintTarget={!(starting || failed)}
        onModeChange={setMode}
        onSend={props.onSend}
        onDraftChange={props.onDraftChange}
      />
      <div className="mode-pill" data-testid="mode-pill">
        {modePill(tab.mode, false)}
      </div>
      {rows.length > 0 && (
        <div className="empty-tab-resume" aria-label="Resume a session">
          {rows.map((session) => (
            <Row
              key={session.providerSessionId}
              as="button"
              kind="choice"
              sign="↺"
              navStop="resume"
              onClick={() => props.onResume(session.providerSessionId)}
            >
              <SessionRowText session={session} />
            </Row>
          ))}
        </div>
      )}
    </div>
  );
}
