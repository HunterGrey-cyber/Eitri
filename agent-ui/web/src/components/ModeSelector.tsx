import type { Hello, PermissionModeChoice } from "../types";

type Props = {
  hello: Hello | null;
  connecting: boolean;
  onStart: (mode: PermissionModeChoice, resume?: string) => void;
};

/** One button per permission mode the backend GENUINELY offers, driven by `hello.permissionModes`
 * rather than hardcoded. The two backends differ for a real reason: the legacy backend has a
 * tested interactive permission gate, while the sidecar path ships BYPASS only in this milestone.
 * Rendering a fixed pair here would offer the sidecar user a choice between two modes that behave
 * identically. */
const MODE_LABELS: Record<PermissionModeChoice, { title: string; detail: string }> = {
  auto: { title: "Auto", detail: "Tool calls that could change things ask first." },
  bypass: { title: "Bypass", detail: "No permission prompts. Every tool call proceeds immediately." },
};

function shortId(id: string): string {
  return id.slice(0, 8);
}

export function ModeSelector({ hello, connecting, onStart }: Props) {
  if (connecting) {
    return (
      <div className="mode-selector">
        <p className="connecting">Starting the agent backend…</p>
        <p className="detail">
          The first start on a fresh Verdandi checkout also builds the sidecar, which can take a
          while. The window stays responsive.
        </p>
      </div>
    );
  }

  if (hello === null) {
    return <div className="mode-selector"><p className="connecting">Connecting to the shell…</p></div>;
  }

  const onlyBypass = hello.permissionModes.length === 1 && hello.permissionModes[0] === "bypass";
  // The default mode for a continued session: the single offered mode when there is only one,
  // otherwise the safer of the two. Resuming deliberately does not ask again -- the permission
  // policy is a property of the new run, not of the conversation being continued, and making the
  // user re-answer it turns a one-click "carry on" into a form.
  const resumeMode: PermissionModeChoice = hello.permissionModes.includes("auto") ? "auto" : "bypass";
  const resumable = hello.resumableSession;

  return (
    <div className="mode-selector">
      <p>
        Start a conversation in <code>{hello.projectDir}</code>
        {hello.permissionModes.length > 1
          ? " — choose a permission mode (it cannot be changed afterwards):"
          : ":"}
      </p>
      {hello.permissionModes.map((mode) => (
        <button key={mode} onClick={() => onStart(mode)}>
          <strong>{MODE_LABELS[mode].title}</strong>
          <span className="detail">{MODE_LABELS[mode].detail}</span>
        </button>
      ))}
      {resumable !== null && (
        // Rendered on `resumableSession` alone. That field is already the full condition -- server
        // advertised resume, this client implements it, and this workspace has a stored provider
        // session -- so there is no second check to forget here.
        <button className="resume" onClick={() => onStart(resumeMode, resumable.providerSessionId)}>
          <strong>Continue previous session</strong>
          <span className="detail">
            Claude {shortId(resumable.providerSessionId)}
            {resumable.updatedAt !== "" ? ` · last used ${formatWhen(resumable.updatedAt)}` : ""}
          </span>
        </button>
      )}
      {onlyBypass && (
        // Said plainly rather than buried: this backend runs every tool without asking, and that is
        // the only policy it currently implements. Calling it a "choice" would imply an alternative
        // exists.
        <p className="warning">
          This backend ({hello.backend}) currently supports <strong>Bypass only</strong>. Tool calls
          run without asking. Interactive permission approval is not implemented on this path yet.
        </p>
      )}
    </div>
  );
}

/** `updatedAt` is epoch milliseconds as a string (no date library is a dependency of the Rust side
 * that writes it). Anything unparseable renders as-is rather than as "Invalid Date". */
function formatWhen(updatedAt: string): string {
  const millis = Number(updatedAt);
  if (!Number.isFinite(millis) || millis <= 0) return updatedAt;
  return new Date(millis).toLocaleString();
}
