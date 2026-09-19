import { useEffect, useState } from "react";
import type { HandoffCommand } from "../types";
import { Row } from "./Row";

/** Why "continue in a terminal" cannot be used right now, or `null` when it can.
 *
 * Mirrors `shell/src/terminal_handoff.rs`'s `prepare_handoff`, and is deliberately a second
 * implementation of one rule rather than a value sent over the wire -- the same arrangement
 * `reducer.ts` has with the Rust projection. Rust is the enforcement: it refuses the command even
 * if this returns `null` wrongly. This is the affordance: it stops the control being offered at all,
 * and says why, so a user is never looking at a button that does nothing.
 *
 * The one state that is NOT here, on purpose: a session that has already ended. Continuing a dead
 * conversation elsewhere is arguably the case where this matters most, so ending is not a blocker
 * on either side. */
export function handoffBlockedReason(providerSessionId: string | null, turnInProgress: boolean): string | null {
  // `!providerSessionId` rather than `=== null`: the sidecar's id crosses proto3, where an unset
  // string arrives as "" rather than as an absent field, and "" would build `claude --resume `
  // with no session to resume.
  if (!providerSessionId) {
    return "This conversation has no Claude session id yet — one is issued when its first turn starts. Send a message first.";
  }
  if (turnInProgress) {
    return "A turn is still running. Let it finish, or press Stop, before continuing in a terminal.";
  }
  return null;
}

type Props = {
  /** Claude's own session id — the only identity `claude --resume` takes. */
  providerSessionId: string | null;
  turnInProgress: boolean;
  /** From the provider's advertised capabilities, never from the backend's name. False on the
   *  default backend, where closing this conversation genuinely ends it. */
  canResume: boolean;
  /** A handoff this panel already asked for is still closing the session.
   *
   *  NOT part of `handoffBlockedReason`, deliberately: that function mirrors the Rust rule, and this
   *  is a client-side in-flight state with no Rust term. Rust has its own refusal for a second
   *  handoff ("this conversation is already being handed off"); this stops the user reaching it by
   *  clicking a control that visibly does nothing. */
  handingOff: boolean;
  onHandoff: () => void;
};

/** The control, plus the confirmation it opens.
 *
 * It confirms rather than acting because the action closes the conversation — on a backend with no
 * resume, irreversibly. Stating that afterwards would be too late. */
export function ContinueInTerminal({
  providerSessionId,
  turnInProgress,
  canResume,
  handingOff,
  onHandoff,
}: Props) {
  const [confirming, setConfirming] = useState(false);
  const blocked = handingOff
    ? "This conversation is being closed — the command will appear here once that has finished."
    : handoffBlockedReason(providerSessionId, turnInProgress);

  /* A confirmation dialog must never appear without a deliberate click, and without this it could:
     `confirming` used to stay true while `blocked` was non-null, so a turn starting while the block
     was open replaced it with the disabled button and then brought it BACK by itself when the turn
     ended. Anything that blocks the action closes the confirmation instead. */
  useEffect(() => {
    if (blocked !== null) setConfirming(false);
  }, [blocked]);

  if (confirming && blocked === null) {
    return (
      <div className="handoff-confirm">
        <p>
          Neovibe will <strong>close this conversation here</strong> first, and then show you the
          command that continues it in your own terminal.
        </p>
        {!canResume && (
          <p className="warning">
            This backend cannot resume a session, so the conversation <strong>cannot be reopened</strong>{" "}
            in this panel afterwards.
          </p>
        )}
        <div className="handoff-confirm-buttons">
          <button
            onClick={() => {
              setConfirming(false);
              onHandoff();
            }}
          >
            Close it and show me the command
          </button>
          <button onClick={() => setConfirming(false)}>Cancel</button>
        </div>
      </div>
    );
  }

  return (
    <div className="handoff">
      <button
        className="handoff-open"
        disabled={blocked !== null}
        title={blocked ?? undefined}
        onClick={() => setConfirming(true)}
      >
        Continue in a terminal…
      </button>
      {blocked !== null && <span className="handoff-blocked">{blocked}</span>}
    </div>
  );
}

/** What the user is given once the session really has been closed.
 *
 * Every claim here is one the code can back. Neovibe started no terminal and took no lock on this
 * session: design doc §8.3's closing paragraph allows handing over the command instead of spawning
 * the supported handoff wrapper, on the condition that it is shown as the raw/manual path and
 * carries a concurrency warning, and §8.5/§17.7 refuse a stronger claim even for the path that does
 * hold a lease. So this says what is true and no more. */
export function HandoffCommandCard({ handoff }: { handoff: HandoffCommand }) {
  return (
    // Same two-cell shape as the rest of the panel-as-document grid (panel-as-document task 3):
    // a `→` sign beside the body, so this row's sign lines up with every other row's. `handoff-card`
    // stays on the BODY cell rather than being replaced -- it carries its own box (border,
    // background, spacing) that has nothing to do with the sign column, and every child selector
    // below (`.handoff-card p`, `.warning`, `.detail`) still matches unchanged since the nesting
    // relative to `.handoff-card` itself has not moved. `bodyClassName` is `Row`'s own way of
    // saying that (`./Row`), rather than this file re-writing the two-cell grid by hand.
    <Row kind="handoff" sign="→" role="status" bodyClassName="handoff-card">
      <strong>This conversation is closed in Neovibe.</strong>
      <p>Run this in your own terminal to continue it:</p>
      {/* <pre>, so it can be selected and copied character for character. */}
      <pre className="handoff-command">{handoff.command}</pre>
      {/* `y` here is handled by `App.tsx`'s `handleStartScreenKeyDown`, not by the conversation's
          `onKeyDown`/`resolveKey` table -- this card only ever renders on the start screen (once
          `sessionStarted` is false), a different render branch with its own tiny key handler,
          because this is the only key this screen offers and there is no cursor here to gate it
          on. */}
      <div className="row-hint">Press y to copy.</div>
      <p className="warning">
        Neovibe took <strong>no lock</strong> on this session and is not watching it. If anything
        else resumes the same session while your terminal has it open, both write into the{" "}
        <strong>same transcript</strong>.
      </p>
      <p className="detail">
        That terminal session is an ordinary <code>claude</code>: it uses{" "}
        <strong>your own Claude Code settings</strong>, not the permission gate this panel installs,
        so tool calls there will not appear here as permission cards.
      </p>
    </Row>
  );
}
