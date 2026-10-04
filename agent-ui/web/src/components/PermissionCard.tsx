import { useEffect, useRef, useState } from "react";
import type { MatchedAskRule, PermissionRequestRecord, ProviderPrompt } from "../types";
import type { PermissionDecision } from "../bridge";
import { editPreview } from "../diff";
import { isUsableLink } from "../timeline";
import { EditDiff } from "./EditDiff";
import { countEscapes, revealHidden } from "../revealHidden";
import { HiddenWarning, Revealed } from "./Revealed";

type Props = {
  request: PermissionRequestRecord;
  /** The session this request belongs to has ended. */
  sessionEnded: boolean;
  /** D7's third button: the rule Rust would apply to this exact tool call
   *  (`permission_rules::offer`), or `null`/absent when it offered none. The card never invents its
   *  own suggestion -- the words shown here are Rust's, byte for byte, so `remember: true` can only
   *  ever be sent for a request Rust itself is prepared to answer with a rule. */
  ruleOffer?: string | null;
  /** v1 hardening (ruling R2): the panel already answered this card -- by `a`/`d`, which answer by
   *  the card's id and never press these buttons, or by a click it has already sent. The panel's
   *  own guard (`App.tsx`'s `answerPermission`) refuses a second answer either way; this is what
   *  draws the card inert once it did. */
  alreadyAnswered?: boolean;
  onAnswer: (permissionId: string, decision: PermissionDecision, reason?: string, remember?: boolean) => void;
  /** The reason box as it is typed into, and `""` when the card goes away: `d` sends the same
   *  reason this card's Deny would (ruling R2). */
  onReasonChange?: (permissionId: string, reason: string) => void;
};

export function PermissionCard({ request, sessionEnded, ruleOffer, alreadyAnswered = false, onAnswer, onReasonChange }: Props) {
  const [reason, setReason] = useState("");
  const [clicked, setClicked] = useState(false);
  const answered = clicked || alreadyAnswered;
  // The panel withdrew its answer (Rust refused it and left the card waiting -- an "Always allow"
  // whose rule could not be saved, ruling 16): this card's own click goes with it, or its buttons
  // stayed disabled for good (Codex's whole-branch review). Only on the panel's true -> false; a
  // card rendered with no panel behind it (`alreadyAnswered` never set) keeps its own guard.
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
          the same id `MessageList` keys that call's own block on. Since v1 polish F21 it is a
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
          // P5: Enter here denies with the reason; an IME's Enter is its own (C4).
          if (e.key !== "Enter" || e.nativeEvent.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          handleAnswer("deny");
        }}
        disabled={inert}
      />
      <div className="permission-card-buttons">
        {/* `data-nav-order` puts Approve first for `l`, one keypress away, then Deny, then the third
            (Always allow) button when Rust offered one, then the reason box above them.
            `a`/`d` do NOT press these buttons (v1 hardening, ruling R2): they answer this card by
            its `permissionId` through the panel's `answerPermission`, which holds the one guard
            against a second answer, and `alreadyAnswered` draws the card inert after them.
            `data-nav-action` stays: it is how v1 S5's Enter/Space guard (`App.tsx`'s `onKeyDown`)
            knows a focused button answers a card. Nothing looks a button up through it any more. */}
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

/** Whose question this card is when the CLI itself asked (after the gate had already answered the
 *  call): a small label -- Claude Code asking, or the user's own ask rule when one forced the prompt -- and the CLI's own sentence, verbatim (it is prose, never parsed;
 *  its path is the CLI's, absolute). Answered like any card: same buttons, keys and permission id. */
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
 *  own ask rule; a prompt of a kind this build does not know is the neutral "Claude Code asked"; one
 *  that gave no reason (an empty or blank one is none; a blocked path is not one) could be the user's
 *  own content-scoped ask rule (the CLI does not name those), so it says so; anything else is the
 *  neutral "Claude Code asked" too, the reason shown beside it. */
function providerPromptLabel(prompt: ProviderPrompt): string {
  if (prompt.matchedAskRule) return `your ask rule: ${askRuleText(prompt.matchedAskRule)}`;
  if (prompt.unrecognizedOrigin != null) return "Claude Code asked";
  if (!prompt.reason?.trim()) return "Claude Code asked (maybe your ask rule)";
  return "Claude Code asked";
}

/** The rule as Claude Code's own settings spell it (Rust's `MatchedAskRule::display`). */
function askRuleText(rule: MatchedAskRule): string {
  return rule.ruleContent === null ? rule.toolName : `${rule.toolName}(${rule.ruleContent})`;
}

/** What the call would do, for a person deciding whether to allow it.
 *
 * A file-changing tool gets a real diff (`EditDiff`, moved out to its own component in Task 13 so
 * a conversation row can fold it too); everything else gets its input as JSON, which for a `Bash`
 * or an `mcp__*` call is the honest rendering. The data for both has always been in the request --
 * `PreToolUse` carries the whole tool-input object -- so this is a rendering change and nothing
 * more: no new event, no wire field, no protocol work.
 */
function ToolInput({ toolName, input, createsFile }: { toolName: string; input: unknown; createsFile?: boolean }) {
  const preview = editPreview(toolName, input);
  if (preview !== null) {
    // No cap here: a card shows the whole change, unlike the folded preview a conversation row gets
    // (`EditDiff`'s `maxLines`, Task 13's P3).
    return <EditDiff preview={preview} createsFile={createsFile} />;
  }
  const fields = input && typeof input === "object" ? (input as Record<string, unknown>) : null;
  if (toolName === "Bash" && fields && typeof fields.command === "string") {
    // P4: Claude Code's `Command:` line -- the command as the shell will read it, newlines and all.
    // Characters that would reorder or hide part of it are drawn as escapes (`revealHidden`).
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
