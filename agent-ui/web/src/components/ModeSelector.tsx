import type { Hello, PermissionModeChoice } from "../types";

type Props = {
  hello: Hello | null;
  connecting: boolean;
  onStart: (mode: PermissionModeChoice) => void;
};

/** One button per permission mode the backend GENUINELY offers, driven by `hello.permissionModes`
 * rather than hardcoded. The two backends differ for a real reason: the legacy backend has a
 * tested interactive permission gate, while the sidecar path ships BYPASS only in this milestone.
 * Rendering a fixed pair here would offer the sidecar user a choice between two modes that behave
 * identically. */
const MODE_LABELS: Record<PermissionModeChoice, { title: string; detail: string }> = {
  auto: {
    title: "Auto",
    detail: "Tool calls that could change things ask first.",
  },
  bypass: {
    title: "Bypass",
    detail: "No permission prompts. Every tool call proceeds immediately.",
  },
};

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
