import { useState } from "react";
import type { PermissionRequestRecord } from "../types";
import type { PermissionDecision } from "../bridge";
import { editPreview } from "../diff";

type Props = {
  request: PermissionRequestRecord;
  /** The session this request belongs to has ended. */
  sessionEnded: boolean;
  onAnswer: (permissionId: string, decision: PermissionDecision, reason?: string) => void;
};

export function PermissionCard({ request, sessionEnded, onAnswer }: Props) {
  const [reason, setReason] = useState("");
  const [answered, setAnswered] = useState(false);

  /* A card belonging to a dead session must not be able to submit into it. The card is not removed
     when the session ends -- an unanswered request is real history, and making it vanish would read
     as a resolution nobody made -- so it is made inert and says why instead. `answered` covers the
     other direction: the SAME decision must not be sendable twice while the real PermissionResolved
     event is still in flight. Neither of these clears the card; only a provider event does that. */
  const inert = answered || sessionEnded;

  function handleAnswer(decision: PermissionDecision) {
    if (inert) return;
    setAnswered(true);
    onAnswer(request.permissionId, decision, decision === "deny" ? reason || undefined : undefined);
  }

  return (
    <div className="permission-card">
      <div className="permission-card-tool">Permission requested: {request.toolName}</div>
      {/* Which call, not just which tool: a turn can have several Bash calls in flight, and this is
          the same id `MessageList` keys that call's own block on, so the two can be read together.
          Rendered only when the request actually carries one -- a placeholder here would read as a
          lookup that failed rather than as an id that was never sent. Every permission path in both
          backends now forwards whatever id its own source message carried, so this is normally
          present; it can still be absent, and the Rust side deliberately does not invent one.
          Truthiness rather than `!== null` on purpose: the sidecar's `tool_use_id` crosses proto3,
          where an unset string arrives as "" rather than as an absent field, and "for tool call "
          with nothing after it is worse than saying nothing. */}
      {request.toolUseId && (
        <div className="permission-card-tool-use-id">for tool call {request.toolUseId}</div>
      )}
      <ToolInput toolName={request.toolName} input={request.input} />
      <input
        type="text"
        placeholder="Reason (shown to the agent if you deny)"
        value={reason}
        onChange={(e) => setReason(e.target.value)}
        disabled={inert}
      />
      <div className="permission-card-buttons">
        <button onClick={() => handleAnswer("allow")} disabled={inert}>Approve</button>
        <button onClick={() => handleAnswer("deny")} disabled={inert}>Deny</button>
      </div>
      {sessionEnded && !answered && (
        <div className="permission-card-stale">
          This session ended before the request was answered, so it can no longer be allowed or denied.
        </div>
      )}
    </div>
  );
}

/** What the call would do, for a person deciding whether to allow it.
 *
 * A file-changing tool gets a real diff; everything else gets its input as JSON, which for a `Bash`
 * or an `mcp__*` call is the honest rendering. The data for both has always been in the request --
 * `PreToolUse` carries the whole tool-input object -- so this is a rendering change and nothing
 * more: no new event, no wire field, no protocol work.
 *
 * **Signal colours are on the gutter, never on the text.** `--nv-ok`/`--nv-error` are guarded at
 * 3:1 for non-text UI, and drawing diff lines in them measured 2.05-3.84:1 on rose-pine dawn when
 * the same mistake was made in this file's banners. The `+`/`-` gutter carries the colour; the code
 * stays `--nv-fg`, and `indexCss.test.ts` enforces that.
 */
function ToolInput({ toolName, input }: { toolName: string; input: unknown }) {
  const preview = editPreview(toolName, input);
  if (preview === null) {
    return <pre className="permission-card-input">{JSON.stringify(input, null, 2)}</pre>;
  }
  return (
    <div className="permission-card-edit">
      <div className="permission-card-edit-head">
        {/* The path first: which file is the question a reader asks before what changed. */}
        <span className="permission-card-edit-path">{preview.filePath || "(no file named)"}</span>
        <span className="permission-card-edit-counts">
          +{preview.added} −{preview.removed}
        </span>
      </div>
      {preview.wholeFile && (
        /* Said rather than implied. A Write request carries only what the file WILL contain, so a
           patch-shaped rendering would suggest the rest of the file survives. It may not. */
        <div className="permission-card-edit-note">
          Writes the whole file. The request does not say what is there now.
        </div>
      )}
      {preview.replaceAll && (
        /* The diff looks identical with and without this, so a reader cannot infer it. */
        <div className="permission-card-edit-note">
          Replaces <strong>every</strong> occurrence in the file, not just the first.
        </div>
      )}
      {preview.diff === null ? (
        <div className="permission-card-edit-note">
          Too large to show here ({preview.added + preview.removed} lines). Shown as counts rather
          than as part of a diff, because a diff missing lines is one you would approve anyway.
        </div>
      ) : (
        <pre className="permission-card-diff">
          {preview.diff.map((line, i) => (
            <div key={i} className={`diff-line diff-${line.kind}`}>
              <span className="diff-gutter" aria-hidden="true">
                {line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "}
              </span>
              <span className="diff-text">{line.text}</span>
            </div>
          ))}
        </pre>
      )}
    </div>
  );
}
