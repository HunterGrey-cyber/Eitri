import { useState } from "react";

type Props = {
  disabled: boolean;
  turnInProgress: boolean;
  onSend: (text: string) => void;
  onInterrupt: () => void;
};

export function Composer({ disabled, turnInProgress, onSend, onInterrupt }: Props) {
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
        placeholder="Ask the agent..."
      />
      <button onClick={send} disabled={disabled}>Send</button>
      <button onClick={onInterrupt} disabled={!turnInProgress}>Stop</button>
    </div>
  );
}
