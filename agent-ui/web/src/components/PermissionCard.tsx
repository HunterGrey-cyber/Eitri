import { useState } from "react";
import type { PermissionRequestRecord } from "../types";
import type { PermissionDecision } from "../bridge";
import { editPreview } from "../diff";
import { isUsableLink } from "../timeline";
import { EditDiff } from "./EditDiff";

type Props = {
  request: PermissionRequestRecord;
  /** The session this request belongs to has ended. */
  sessionEnded: boolean;
  /** The rule `permission_rules::offer` supplied for this exact tool call, or `null`/absent.
   *  Show Rust's words unchanged so `remember: true` is offered only for a request Rust can
   *  answer with that rule. The card never generates its own suggestion. */
  ruleOffer?: string | null;
  onAnswer: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
};

export function PermissionCard({ request, sessionEnded, ruleOffer, onAnswer }: Props) {
  const [reason, setReason] = useState("");
  const [answered, setAnswered] = useState(false);

  /* A card belonging to a dead session must not be able to submit into it. The card is not removed
     when the session ends -- an unanswered request is real history, and making it vanish would read
     as a resolution nobody made -- so it is made inert and says why instead. `answered` covers the
     other direction: the SAME decision must not be sendable twice while the real PermissionResolved
     event is still in flight. Neither of these clears the card; only a provider event does that. */
  const inert = answered || sessionEnded;

  function handleAnswer(decision: PermissionDecision, remember = false) {
    if (inert) return;
    setAnswered(true);
    onAnswer(
      request.permissionId,
      decision,
      decision === "deny" ? reason || undefined : undefined,
      ...(remember ? [true] : []),
    );
  }

  return (
    <div className="permission-card">
      {/* Which call, not just which tool: a turn can have several Bash calls in flight, and this is
          the same id `MessageList` keys that call's own block on. It is a
          tooltip, not a line: "for tool call toolu_01…" was noise to read on every card, and the
          card already sits beside the call it gates. Only a USABLE id (`isUsableLink`, which rules
          out `null` and the sidecar's proto3 `""`) -- a tooltip naming nothing is worse than none. */}
      <div
        className="permission-card-tool"
        title={isUsableLink(request.toolUseId) ? `tool call ${request.toolUseId}` : undefined}
        data-tool-use-id={isUsableLink(request.toolUseId) ? request.toolUseId : undefined}
      >
        Permission requested: {request.toolName}
      </div>
      <ToolInput toolName={request.toolName} input={request.input} createsFile={request.createsFile} />
      <input
        type="text"
        data-nav-order={ruleOffer ? 4 : 3}
        placeholder="Reason (shown to the agent if you deny) — Enter denies"
        value={reason}
        onChange={(e) => setReason(e.target.value)}
        onKeyDown={(e) => {
          // Enter denies with the reason; leave IME Enter to the input method.
          if (e.key !== "Enter" || e.nativeEvent.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          handleAnswer("deny");
        }}
        disabled={inert}
      />
      <div className="permission-card-buttons">
        {/* `data-nav-order` puts Approve first for `l`, one keypress away, then Deny, then the third
            (Always allow) button when Rust offered one, then the reason box above them.
            `data-nav-action` is how `a`/`d` press these very buttons, so the card's own
            `inert`/`answered` guard against a double answer applies to the keyboard too. */}
        <button data-nav-order={1} data-nav-action="allow" onClick={() => handleAnswer("allow")} disabled={inert}>Approve</button>
        <button data-nav-order={2} data-nav-action="deny" onClick={() => handleAnswer("deny")} disabled={inert}>Deny</button>
        {ruleOffer && (
          <button data-nav-order={3} data-nav-action="always" onClick={() => handleAnswer("allow", true)} disabled={inert}>
            Always allow {ruleOffer} in this project
          </button>
        )}
      </div>
      {sessionEnded && !answered && (
        <div className="permission-card-stale">
          This session ended before the request was answered, so it can no longer be allowed or denied.
        </div>
      )}
    </div>
  );
}

/** Show what the call would do so the user can decide whether to allow it.
 * File-changing tools use `EditDiff`, which conversation rows can also use for folded previews.
 * Other inputs render as JSON, with `Bash` commands shown as shell text when available.
 * `PreToolUse` already carries the complete input object; these previews need no extra wire fields. */
function ToolInput({ toolName, input, createsFile }: { toolName: string; input: unknown; createsFile?: boolean }) {
  const preview = editPreview(toolName, input);
  if (preview !== null) {
    // No cap here: a card shows the whole change, unlike the folded preview a conversation row gets
    //.
    return <EditDiff preview={preview} createsFile={createsFile} />;
  }
  const fields = input && typeof input === "object" ? (input as Record<string, unknown>) : null;
  if (toolName === "Bash" && fields && typeof fields.command === "string") {
    // Show the command as the shell will read it, including newlines.
    return (
      <>
        <pre className="permission-card-command">$ {fields.command}</pre>
        {typeof fields.description === "string" && <div className="permission-card-description">{fields.description}</div>}
      </>
    );
  }
  return <pre className="permission-card-input">{JSON.stringify(input, null, 2)}</pre>;
}
