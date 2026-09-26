import { useEffect, useState } from "react";
import type { KeyboardEvent } from "react";
import type { ContextSummary, HandoffCommand, Hello, QueueItem, TabInfo } from "../types";
import type { PanelMode } from "../keymap";
import { Composer } from "./Composer";
import type { RestoredDraft } from "./Composer";
import { HandoffCommandCard } from "./TerminalHandoff";
import { QueueLines } from "./QueueLines";
import { ContextLine } from "./ContextLine";
import { Row } from "./Row";
import { SessionRowText } from "./SessionRow";
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
  /** Phase 3: this tab's own queue and prompt-history plumbing, threaded through to `Composer` --
   *  see that component's own props of the same names. `running` is this tab's own `starting`, not
   *  a turn: a `NotStarted`/`starting` tab has no turn yet, but starting is still not "idle" (the
   *  box queues behind the connect rather than lazily starting a second session). No `onInterrupt`
   *  here -- there is no live turn to interrupt while merely connecting, so `Composer`'s Ctrl+c
   *  branch for a running box is inert (the draft stays put, which is harmless).
   *
   *  `Composer` DOES need something wired to `onSendNow`, though (fix round 1, reviewer finding):
   *  its `submit()` clears the box on the `now` branch unconditionally, whether or not `onSendNow`
   *  did anything, so leaving it as the default no-op silently threw away whatever the user typed
   *  on Ctrl+Enter. A starting tab has no turn to send to "now" either, so it falls back to the same
   *  effect as `onQueue` below, guarded against an empty box (`queue_message` has no such guard of
   *  its own and would happily queue a blank entry). */
  onQueue?: (text: string) => void;
  history?: string[];
  queueCount?: number;
  /** The queue and V1's editor-context line, shown above the composer the same way the live
   *  conversation shows them (Task 9): a `starting` tab can already have queued behind its own
   *  connect (C1), and the editor context is window-wide, not gated on a session existing. */
  queue?: QueueItem[];
  queueError?: string | null;
  editorContext?: ContextSummary | null;
  onTakeBackQueue?: () => void;
  queueTaken?: { texts: string[]; seq: number } | null;
  onHistoryPush?: (text: string) => void;
  onEditInNvim?: (text: string) => void;
  editingInNvim?: boolean;
  onOpenKeymap?: () => void;
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
    // Once the composer is disabled (failed) there is nothing left to type into, so `mode` staying
    // "input" must not swallow the keys the disabled screen still offers -- `r` on a failed tab, `f`
    // for HINT, `y` to copy a handoff command -- the same reasoning `Composer`'s own doc comment
    // gives for a dead session's INPUT being an empty mode. A `starting` tab's composer is live now
    // (C1: it queues behind the connect), so it is no longer exempted here.
    if (mode === "input" && !failed) return;
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
      <QueueLines items={props.queue ?? []} error={props.queueError ?? null} />
      <ContextLine context={props.editorContext ?? null} />
      <Composer
        disabled={failed}
        sessionEnded={failed}
        closing={false}
        restoredDraft={props.restoredDraft}
        mode={mode}
        focusRequest={props.focusRequest}
        hintTarget={!failed}
        onModeChange={setMode}
        onSend={props.onSend}
        onDraftChange={props.onDraftChange}
        running={starting}
        onQueue={props.onQueue}
        onSendNow={(text) => {
          if (text.trim() !== "") props.onQueue?.(text);
        }}
        history={props.history}
        queueCount={props.queueCount}
        onTakeBackQueue={props.onTakeBackQueue}
        queueTaken={props.queueTaken}
        onHistoryPush={props.onHistoryPush}
        onEditInNvim={props.onEditInNvim}
        editingInNvim={props.editingInNvim}
        onOpenKeymap={props.onOpenKeymap}
      />
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
