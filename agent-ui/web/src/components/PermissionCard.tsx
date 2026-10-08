import { useEffect, useRef, useState } from "react";
import type {
  AcpCardInput,
  CodexCommandInput,
  CodexFileChange,
  CodexFileChangeInput,
  CodexMcpInput,
  MatchedAskRule,
  PermissionRequestRecord,
  ProviderPrompt,
} from "../types";
import type { PermissionDecision } from "../bridge";
import { codexChangePreview, editPreview, lineDiff } from "../diff";
import type { EditPreview } from "../diff";
import { isExternalTool, subagentOf } from "../externalTools";
import { isUsableLink } from "../timeline";
import { EditDiff } from "./EditDiff";
import { countEscapes, revealHidden } from "../revealHidden";
import { HiddenWarning, Revealed } from "./Revealed";
import { NO_TEXT_ASSIST } from "../textField";

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
  const external = isExternalTool(request.toolName);

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
      {/* A deny from another agent carries no reason (Codex's decline has no field for one), so there is
          no box to type into; `D` says so (`App.tsx`). */}
      {!external && (
        <input
          type="text"
          {...NO_TEXT_ASSIST}
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
      )}
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

/** Show what the call would do so the user can decide whether to allow it.
 * File-changing tools use `EditDiff`, which conversation rows can also use for folded previews.
 * Other inputs render as JSON, with `Bash` commands shown as shell text when available.
 * `PreToolUse` already carries the complete input object; these previews need no extra wire fields. */
function ToolInput({ toolName, input, createsFile }: { toolName: string; input: unknown; createsFile?: boolean }) {
  if (isExternalTool(toolName)) return <ExternalInput toolName={toolName} input={input} />;
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

/** Agent-written text, drawn through `revealHidden` with its own warning line when it holds anything hidden. */
function Shown({ text, what }: { text: string; what: string }) {
  const pieces = revealHidden(text);
  return (
    <>
      <Revealed pieces={pieces} />
      <HiddenWarning count={countEscapes(pieces)} what={what} />
    </>
  );
}

/** The agent's explanation of why it asks, labelled and muted: it is the agent's claim, not Eitri's. */
function AgentSays({ who, reason }: { who: string; reason: unknown }) {
  if (typeof reason !== "string" || reason.trim() === "") return null;
  return (
    <div className="permission-card-description permission-card-says">
      {who} says: <Shown text={reason} what="reason" />
    </div>
  );
}

/** Whose subagent a call came from, when it did not come from the tab's own thread. */
function SubagentLine({ name }: { name: string | null }) {
  if (name === null) return null;
  return (
    <div className="permission-card-provider">
      <span className="permission-card-provider-label">subagent</span>
      <Shown text={name} what="subagent name" />
    </div>
  );
}

/** The cards of another agent: a Codex command, file change or tool call, or an ACP agent's call. What each
 *  card shows is what the provider holds for the request, never more: a file change is the diff Codex
 *  announced for that item, and a command is the exact string Codex will run. */
function ExternalInput({ toolName, input }: { toolName: string; input: unknown }) {
  const subagent = subagentOf(toolName, input);
  return (
    <>
      <SubagentLine name={subagent} />
      {toolName === "commandExecution" ? (
        <CodexCommand input={input as Partial<CodexCommandInput>} />
      ) : toolName === "fileChange" ? (
        <CodexFileChanges input={input as Partial<CodexFileChangeInput>} />
      ) : toolName === "mcpToolCall" ? (
        <CodexMcp input={input as Partial<CodexMcpInput>} />
      ) : (
        <AcpCall input={(input ?? {}) as AcpCardInput} />
      )}
    </>
  );
}

function CodexCommand({ input }: { input: Partial<CodexCommandInput> }) {
  const network = input.network ?? null;
  const command = typeof input.command === "string" ? revealHidden(input.command) : null;
  const wording =
    typeof input.escalation === "string" && input.escalation !== ""
      ? input.escalation
      : network !== null
        ? `asks for network access to ${network.host} (${network.protocol})`
        : "";
  return (
    <>
      {wording !== "" && (
        <div className="permission-card-wording">
          <Shown text={wording} what="wording" />
        </div>
      )}
      {command !== null && (
        <>
          <pre className="permission-card-command">
            {"$ "}
            <Revealed pieces={command} />
          </pre>
          <HiddenWarning count={countEscapes(command)} what="command" />
        </>
      )}
      {typeof input.cwd === "string" && input.cwd !== "" && (
        <div className="permission-card-description">
          in <Shown text={input.cwd} what="directory" />
        </div>
      )}
      <AgentSays who="Codex" reason={input.reason} />
    </>
  );
}

function CodexFileChanges({ input }: { input: Partial<CodexFileChangeInput> }) {
  const changes: CodexFileChange[] = Array.isArray(input.changes) ? input.changes : [];
  return (
    <>
      {changes.length === 0 && <div className="permission-card-description">(no files named)</div>}
      {changes.map((change, i) => (
        <div key={i} className="permission-card-change">
          <ChangeNote change={change} />
          <EditDiff preview={codexChangePreview(change)} />
        </div>
      ))}
      <AgentSays who="Codex" reason={input.reason} />
    </>
  );
}

/** What the change does to the path beyond the lines: a new file, a removed one, a move. */
function ChangeNote({ change }: { change: CodexFileChange }) {
  const note =
    change.kind === "add"
      ? "Creates a new file."
      : change.kind === "delete"
        ? "Deletes the file."
        : change.kind === "update"
          ? null
          : `Change of kind ${change.kind}.`;
  const moved = typeof change.movePath === "string" && change.movePath !== "";
  if (note === null && !moved) return null;
  return (
    <div className="permission-card-edit-note">
      {note}
      {note !== null && moved ? " " : ""}
      {moved && (
        <>
          Moves it to <Shown text={change.movePath!} what="path" />.
        </>
      )}
    </div>
  );
}

function CodexMcp({ input }: { input: Partial<CodexMcpInput> }) {
  const pieces = revealHidden(JSON.stringify(input.arguments ?? null, null, 2) ?? "");
  return (
    <>
      <div className="permission-card-wording">
        Tool <Shown text={String(input.tool ?? "")} what="tool name" /> on server{" "}
        <Shown text={String(input.server ?? "")} what="server name" />
      </div>
      <pre className="permission-card-input">
        <Revealed pieces={pieces} />
      </pre>
      <HiddenWarning count={countEscapes(pieces)} what="arguments" />
    </>
  );
}

/** The preview of one ACP diff: the old text against the new, as the agent sent them. */
function acpPreview(diff: NonNullable<AcpCardInput["diffs"]>[number]): EditPreview {
  const before = typeof diff.oldText === "string" ? diff.oldText : "";
  const after = typeof diff.newText === "string" ? diff.newText : "";
  const shown = lineDiff(before, after);
  const count = (kind: "added" | "removed") => shown?.filter((l) => l.kind === kind).length ?? 0;
  return { filePath: diff.path, diff: shown, added: count("added"), removed: count("removed"), replaceAll: false, wholeFile: false };
}

/** An ACP agent's call: the title it gave, what it names, and the agent's own option names, drawn as it wrote
 *  them because a reject may end the turn. Approve takes the first `allow_once`, Deny the first `reject_once`;
 *  `allow_always` and `reject_always` are never offered. */
function AcpCall({ input }: { input: AcpCardInput }) {
  const options = Array.isArray(input.options) ? input.options : [];
  const approve = options.find((o) => o.kind === "allow_once");
  const deny = options.find((o) => o.kind === "reject_once");
  const approvePieces = revealHidden(approve?.name ?? "");
  const denyPieces = revealHidden(deny?.name ?? "");
  const locations = Array.isArray(input.locations) ? input.locations : [];
  const diffs = Array.isArray(input.diffs) ? input.diffs : [];
  const raw = input.rawInput === undefined || input.rawInput === null ? null : revealHidden(JSON.stringify(input.rawInput, null, 2) ?? "");
  return (
    <>
      {typeof input.title === "string" && input.title !== "" && (
        <div className="permission-card-wording">
          <Shown text={input.title} what="title" />
        </div>
      )}
      {locations.map((location, i) => (
        <div key={i} className="permission-card-description">
          <Shown text={String(location.path)} what="path" />
        </div>
      ))}
      {diffs.map((diff, i) => (
        <EditDiff key={i} preview={acpPreview(diff)} />
      ))}
      {raw !== null && (
        <>
          <pre className="permission-card-input">
            <Revealed pieces={raw} />
          </pre>
          <HiddenWarning count={countEscapes(raw)} what="input" />
        </>
      )}
      <AgentSays who="The agent" reason={input.reason} />
      <div className="permission-card-description permission-card-options">
        Approve answers {approve ? <>“<Revealed pieces={approvePieces} />”</> : "nothing: the agent offered no allow-once option"}.
        Deny answers {deny ? <>“<Revealed pieces={denyPieces} />”</> : "nothing: the agent offered no reject-once option"}.
      </div>
      <HiddenWarning count={countEscapes(approvePieces) + countEscapes(denyPieces)} what="option name" />
    </>
  );
}
