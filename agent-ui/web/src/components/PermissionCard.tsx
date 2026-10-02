import { useEffect, useRef, useState } from "react";
import type { MatchedAskRule, PermissionRequestRecord, ProviderPrompt } from "../types";
import type { PermissionDecision } from "../bridge";
import { editPreview } from "../diff";
import { isUsableLink } from "../timeline";
import { EditDiff } from "./EditDiff";
import { countEscapes, revealHidden, type RevealedPiece } from "../revealHidden";

type Props = {
  request: PermissionRequestRecord;
  /** The session this request belongs to has ended. */
  sessionEnded: boolean;
  /** The rule `permission_rules::offer` supplied for this exact tool call, or `null`/absent.
   *  Show Rust's words unchanged so `remember: true` is offered only for a request Rust can
   *  answer with that rule. The card never generates its own suggestion. */
  ruleOffer?: string | null;
  /** v1 hardening: the panel already answered this card -- by `a`/`d`, which answer by
   *  the card's id and never press these buttons, or by a click it has already sent. The panel's
   *  own guard (`App.tsx`'s `answerPermission`) refuses a second answer either way; this is what
   *  draws the card inert once it did. */
  alreadyAnswered?: boolean;
  onAnswer: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
  /** The reason box as it is typed into, and `""` when the card goes away: `d` sends the same
   *  reason this card's Deny would. */
  onReasonChange?: (permissionId: string, reason: string) => void;
};

export function PermissionCard({ request, sessionEnded, ruleOffer, alreadyAnswered = false, onAnswer, onReasonChange }: Props) {
  const [reason, setReason] = useState("");
  const [clicked, setClicked] = useState(false);
  const answered = clicked || alreadyAnswered;
  // When the panel withdraws an answer, clear this card's click guard so its buttons work again.
  // This can happen when Rust cannot save an Always allow rule and leaves the card pending.
  // The reset follows `alreadyAnswered` changing from true to false; a card without panel updates keeps its local guard.
  useEffect(() => {
    if (!alreadyAnswered) setClicked(false);
  }, [alreadyAnswered]);
  // A card that goes away takes its typed reason with it, so `d` never sends a reason from a box
  // that is no longer on screen. The latest callback, read at unmount, without re-running on it.
  const reasonChangeRef = useRef(onReasonChange);
  reasonChangeRef.current = onReasonChange;
  useEffect(() => () => reasonChangeRef.current?.(request.permissionId, ""), [request.permissionId]);

  /* A card belonging to a dead session must not be able to submit into it. The card is not removed
     when the session ends -- an unanswered request is real history, and making it vanish would read
     as a resolution nobody made -- so it is made inert and says why instead. `answered` covers the
     other direction: the SAME decision must not be sendable twice while the real PermissionResolved
     event is still in flight -- set here the moment a button is used (`clicked`), and by the panel
     (`alreadyAnswered`) when `a`/`d` answered it. Neither of these clears the card; only a provider
     event does that. */
  const inert = answered || sessionEnded;

  function handleAnswer(decision: PermissionDecision, remember = false) {
    if (inert) return;
    setClicked(true);
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
      {request.providerPrompt && <ProviderPromptLine prompt={request.providerPrompt} />}
      <ToolInput toolName={request.toolName} input={request.input} createsFile={request.createsFile} />
      <input
        type="text"
        data-nav-order={ruleOffer ? 4 : 3}
        placeholder="Reason (shown to the agent if you deny) — Enter denies"
        value={reason}
        onChange={(e) => {
          setReason(e.target.value);
          onReasonChange?.(request.permissionId, e.target.value);
        }}
        onKeyDown={(e) => {
          // Enter denies with the reason; leave IME Enter to the input method.
          if (e.key !== "Enter" || e.nativeEvent.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          handleAnswer("deny");
        }}
        disabled={inert}
      />
      <div className="permission-card-buttons">
        {/* `data-nav-order` puts Approve first for `l`, then Deny, then Always allow when offered,
            then the reason box. `a`/`d` answer through `answerPermission` by `permissionId`, using
            its shared guard against a second answer. `alreadyAnswered` then makes the card inert.
            `data-nav-action` lets `App.tsx` identify card buttons for its Enter/Space guard. */}
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

/** Label a CLI prompt raised after the gate answered as a safety check or a user ask rule.
 *  Show the CLI's reason verbatim without parsing its prose or changing its absolute path.
 *  The card uses the same buttons, keys and permission id as any other permission request. */
function ProviderPromptLine({ prompt }: { prompt: ProviderPrompt }) {
  const rule = prompt.matchedAskRule;
  return (
    <div className="permission-card-provider">
      <span className="permission-card-provider-label" title={rule ? `from ${rule.source}` : undefined}>
        {providerPromptLabel(prompt)}
      </span>
      {prompt.reason && <span className="permission-card-provider-reason">{prompt.reason}</span>}
    </div>
  );
}

/** Whose question it is (Rust's `ProviderPrompt::label`, which names the row notes too): the user's
 *  own ask rule; "Claude Code safety check" only when the CLI said why; otherwise -- no reason, or a
 *  kind of prompt this build does not know -- the neutral "Claude Code asked" (O3 review #5, #3). */
function providerPromptLabel(prompt: ProviderPrompt): string {
  if (prompt.matchedAskRule) return `your ask rule: ${askRuleText(prompt.matchedAskRule)}`;
  if (prompt.unrecognizedOrigin == null && prompt.reason) return "Claude Code safety check";
  return "Claude Code asked";
}

/** The rule as Claude Code's own settings spell it (Rust's `MatchedAskRule::display`). */
function askRuleText(rule: MatchedAskRule): string {
  return rule.ruleContent === null ? rule.toolName : `${rule.toolName}(${rule.ruleContent})`;
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
    // Draw characters that could reorder or hide text as escapes (`revealHidden`).
    const pieces = revealHidden(fields.command);
    return (
      <>
        <pre className="permission-card-command">
          {"$ "}
          <Revealed pieces={pieces} />
        </pre>
        <HiddenWarning count={countEscapes(pieces)} what="command" />
        {typeof fields.description === "string" && <div className="permission-card-description">{fields.description}</div>}
      </>
    );
  }
  // `JSON.stringify` escapes control characters but leaves bidi and zero-width ones raw, and this is
  // the only view of an MCP or WebFetch call, so it gets the same treatment.
  const pieces = revealHidden(JSON.stringify(input, null, 2) ?? "");
  return (
    <>
      <pre className="permission-card-input">
        <Revealed pieces={pieces} />
      </pre>
      <HiddenWarning count={countEscapes(pieces)} what="input" />
    </>
  );
}

function Revealed({ pieces }: { pieces: RevealedPiece[] }) {
  return (
    <>
      {pieces.map((piece, i) =>
        "text" in piece ? (
          piece.text
        ) : (
          <span key={i} className="permission-card-escape">
            {piece.escape}
          </span>
        ),
      )}
    </>
  );
}

/** One line under the command saying why it shows `⟨U+…⟩`, only when it does. */
function HiddenWarning({ count, what }: { count: number; what: string }) {
  if (count === 0) return null;
  const characters = count === 1 ? "character" : "characters";
  return (
    <div className="permission-card-warning">
      the {what} holds {count} invisible or direction-changing {characters}, shown as ⟨U+…⟩
    </div>
  );
}
