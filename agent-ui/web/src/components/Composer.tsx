import { useState } from "react";

type Props = {
  disabled: boolean;
  turnInProgress: boolean;
  /** The session is gone (lost or closed). Changes what the box SAYS; `disabled` is what stops it. */
  sessionEnded: boolean;
  /** From the provider's advertised capabilities, not from the backend's name. */
  canInterrupt: boolean;
  onSend: (text: string) => void;
  onInterrupt: () => void;
};

export function Composer({ disabled, turnInProgress, sessionEnded, canInterrupt, onSend, onInterrupt }: Props) {
  const [text, setText] = useState("");

  function send() {
    if (!text.trim()) return;
    onSend(text);
    setText("");
  }

  return (
    <div className="composer">
      <textarea
        value={text}
        disabled={disabled}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            send();
          }
        }}
        placeholder={sessionEnded ? "This session has ended — start a new one." : "Ask the agent..."}
      />
      <button onClick={send} disabled={disabled}>Send</button>
      {canInterrupt && (
        <button onClick={onInterrupt} disabled={!turnInProgress}>Stop</button>
      )}
    </div>
  );
}
