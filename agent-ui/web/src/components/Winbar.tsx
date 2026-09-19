import type { AgentUiState, PermissionModeChoice } from "../types";

// Moved verbatim from `SessionHeader.tsx` (panel-as-document task 6) -- do not rewrite this.
function shortId(id: string | null): string {
  return id === null ? "—" : id.slice(0, 8);
}

type Props = {
  state: AgentUiState;
  /** The mode this session was actually started with, or `null` when this page never started one
   *  itself -- a panel reload's restored snapshot, say. `AgentUiState` carries no such field of its
   *  own (`Hello.permissionModes` is only the pre-session menu, gone once a session exists, and
   *  `Capabilities.bypassPermissionMode` is a capability flag, not the active mode), so `App.tsx`
   *  remembers what it asked for and passes it down. See its own doc comment on
   *  `startedPermissionMode` for why that remembered value is authoritative rather than a guess:
   *  the provider REFUSES a mode it cannot honour instead of substituting one. Optional so every
   *  existing caller/test that has no opinion about it does not have to name it. */
  permissionMode?: PermissionModeChoice | null;
};

/** Identity and model, in a bar above the conversation -- the panel's `winbar`. Panel *state*
 *  (mode, session status, position, Stop) is `StatusLine`'s job instead; see its own doc comment. */
export function Winbar({ state, permissionMode = null }: Props) {
  const provider = state.provider;
  return (
    <div className="winbar">
      <span className="model">{state.model ?? "no model yet"}</span>
      <span className="backend" title={`backend: ${state.backend}`}>
        {state.backend}
      </span>
      {permissionMode !== null && (
        <span className="permission-mode" title={`permission mode: ${permissionMode}`}>
          {permissionMode}
        </span>
      )}
      {/* Three identities where there really are three. The legacy backend's CLI never separated
          its own session id from Claude's, so `sessionId` and `providerSessionId` are the SAME
          value there -- printing both would invite a reader to conclude the two are distinct and
          happen to match, which is the opposite of true. Collapse it, and say whose id it is.

          A session that has started but has not yet taken a turn has BOTH ids null -- `sessionId
          === providerSessionId` is true there too (null === null), so that state must be checked
          first, or it falls into the "they differ" branch and names a sidecar/claude split that
          was never involved: on the default legacy backend the two ids are always one value, and
          nothing here has split into two processes. Say plainly that neither is assigned yet. */}
      <span
        className="identities"
        data-testid="identities"
        title={[
          `conversation (neovibe): ${state.conversationId ?? "n/a"}`,
          `session (verdandi):     ${state.sessionId ?? "not yet assigned"}`,
          `provider (claude):      ${state.providerSessionId ?? "not yet assigned"}`,
        ].join("\n")}
      >
        {state.conversationId !== null && <>conv {shortId(state.conversationId)} · </>}
        {state.sessionId === null && state.providerSessionId === null ? (
          <>session not yet assigned</>
        ) : state.sessionId !== null && state.sessionId === state.providerSessionId ? (
          <>session {shortId(state.sessionId)}</>
        ) : (
          <>
            verdandi {shortId(state.sessionId)} · claude {shortId(state.providerSessionId)}
          </>
        )}
      </span>
      {provider !== null && (
        // `provider-warn` carries the Verdandi skew signal on a border, never on this text --
        // `--nv-warn` is only guarded to 3:1 (see index.css's big comment near `.tool-result-error`
        // for the measured numbers), which is not enough for 12px text.
        <span
          className={`provider${provider.startupDiagnostics.length > 0 ? " provider-warn" : ""}`}
          title={[
            `sidecar ${provider.sidecarVersion} · protocol ${provider.protocol}`,
            `claude-agent-sdk ${provider.claudeAgentSdkVersion}`,
            `claude CLI ${provider.claudeCodeVersion}`,
            ...(provider.buildDescription !== null ? [provider.buildDescription] : []),
            ...provider.startupDiagnostics,
          ].join("\n")}
        >
          CLI {provider.claudeCodeVersion}
          {provider.startupDiagnostics.length > 0 ? " ⚠" : ""}
        </span>
      )}
    </div>
  );
}
