import type { AgentUiState } from "../types";
import { remedyNamesR } from "../problems";
import type { Problem } from "../problems";
import { Row } from "./Row";

export function SessionEndedBanner({ state, restartHint, sessionEndedProblem }: {
  state: AgentUiState;
  restartHint: string;
  sessionEndedProblem: Problem | null;
}) {
  return (
    state.status.kind === "unavailable" ? (
      <Row kind="error" sign="✗" role="alert" problem={sessionEndedProblem}>
        <strong>This session was lost. What is shown above may be incomplete.</strong>
        <pre>{state.status.reason}</pre>
        {/* `r` is offered only on this banner and the one below, for resetting this tab -- see
            `onKeyDown`'s "restart" arm above and `resolveKey`'s `case "r"` in `./keymap`, which is
            what actually enforces that. The mode this hint is read in is
            always BROWSE: `resolveKey` refuses `i` once the session has ended and the effect near
            the top of this component forces BROWSE if it died while INPUT was active, so this
            sentence cannot be on screen in a mode that drops `r`. */}
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">{restartHint}</div>}
      </Row>
    ) : state.status.kind === "closed" ? (
      // Deliberately NOT `row-error`/`✗`: an ordinary close (the host closed it, the provider
      // exited cleanly) is not an error, and styling it like the lost-session row above would
      // train the eye to ignore the one that matters. See `.row-ended` in index.css for the
      // fuller record of why these two were briefly unified and then split back apart.
      <Row kind="ended" sign="·" problem={sessionEndedProblem}>
        This session has ended ({state.status.reason}).
        {!remedyNamesR(sessionEndedProblem) && <div className="row-hint">{restartHint}</div>}
      </Row>
    ) : null
  );
}
