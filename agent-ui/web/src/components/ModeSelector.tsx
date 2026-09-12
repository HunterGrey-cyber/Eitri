import { useState } from "react";
import type { Hello, PermissionModeChoice } from "../types";

type Props = {
  hello: Hello | null;
  connecting: boolean;
  onStart: (mode: PermissionModeChoice, resume?: string) => void;
};

/** One button per permission mode the backend GENUINELY offers, driven by `hello.permissionModes`
 * and never hardcoded. That list is the client's own implemented set intersected, at session
 * creation, with what the provider advertises -- a backend whose provider does not offer a policy
 * fails loudly there rather than being quietly given a different one. It is deliberately not keyed
 * on which backend this is: both have a real, separately verified interactive gate. */
const MODE_LABELS: Record<PermissionModeChoice, { title: string; detail: string }> = {
  auto: { title: "Auto", detail: "Tool calls that could change things ask first." },
  bypass: { title: "Bypass", detail: "No permission prompts. Every tool call proceeds immediately." },
};

function shortId(id: string): string {
  return id.slice(0, 8);
}

export function ModeSelector({ hello, connecting, onStart }: Props) {
  /* Whether the next start continues the stored session or begins a fresh one. Two axes, not one:
     WHICH conversation and under WHAT permission policy are independent choices, and a resume mints
     a new run whose policy is genuinely open.

     This used to be one button that carried a permission mode chosen for the user
     (`permissionModes.includes("auto") ? "auto" : "bypass"`). That was invisible but harmless while
     the sidecar offered a single mode -- there was nothing to choose. Once it offered two, the same
     line started silently picking one, on a screen whose own text promises "choose a permission mode
     (it cannot be changed afterwards)". A screen cannot say that and then decide for you. */
  const [continuePrevious, setContinuePrevious] = useState(false);

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
  const resumable = hello.resumableSession;
  // Guards against a stale `true` if a hello ever arrives without the record that armed it.
  const resuming = continuePrevious && resumable !== null;

  return (
    <div className="mode-selector">
      <p>
        Start a conversation in <code>{hello.projectDir}</code>
        {hello.permissionModes.length > 1
          ? " — choose a permission mode (it cannot be changed afterwards):"
          : ":"}
      </p>

      {resumable !== null && (
        /* Rendered on `resumableSession` alone. That field is already the full condition -- server
           advertised resume, this client implements it, and this workspace has a stored provider
           session -- so there is no second check to forget here.

           A selector, not a start button: picking it changes what the mode buttons below will do
           rather than starting anything, which is what keeps the permission choice the user's. */
        <div className="session-choice" role="radiogroup" aria-label="Which conversation">
          <button
            type="button"
            role="radio"
            aria-checked={!resuming}
            className={resuming ? "" : "selected"}
            onClick={() => setContinuePrevious(false)}
          >
            <strong>New session</strong>
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={resuming}
            className={`resume ${resuming ? "selected" : ""}`}
            onClick={() => setContinuePrevious(true)}
          >
            <strong>Continue previous session</strong>
            <span className="detail">
              Claude {shortId(resumable.providerSessionId)}
              {resumable.updatedAt !== "" ? ` · last used ${formatWhen(resumable.updatedAt)}` : ""}
            </span>
          </button>
        </div>
      )}

      {hello.permissionModes.map((mode) => (
        <button key={mode} onClick={() => onStart(mode, resuming ? resumable.providerSessionId : undefined)}>
          <strong>{MODE_LABELS[mode].title}</strong>
          <span className="detail">
            {MODE_LABELS[mode].detail}
            {resuming ? " Continues the session above." : ""}
          </span>
        </button>
      ))}

      {onlyBypass && (
        // Said plainly rather than buried: this backend offers exactly one policy, and it runs every
        // tool without asking. Calling that a "choice" would imply an alternative exists.
        <p className="warning">
          This backend ({hello.backend}) offers <strong>Bypass only</strong>. Tool calls run without
          asking.
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
