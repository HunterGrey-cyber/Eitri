import { useEffect, useState } from "react";

/** A draft the host is putting back into the box after a send was refused.
 *
 * An object with a `seq` rather than a bare string, because the same text can be refused twice in a
 * row: a new string identical to the old one would not change the effect's dependency, and the
 * second restore would silently not happen. */
export type RestoredDraft = { text: string; seq: number };

type Props = {
  disabled: boolean;
  turnInProgress: boolean;
  /** The session is gone (lost or closed). Changes what the box SAYS; `disabled` is what stops it. */
  sessionEnded: boolean;
  /** A "continue in a terminal" handoff is in flight: the session has already been taken out of the
   *  Rust panel's hands and its real close is running on a worker thread, but nothing has been torn
   *  down here yet.
   *
   *  This is one of the two halves of not eating a typed message. The box is disabled (so Enter
   *  cannot clear it into a session that no longer exists) AND says why, because a box that stops
   *  accepting input with no explanation is indistinguishable from a broken one. The other half is
   *  `restoredDraft`, for the refusals this cannot pre-empt. */
  closing: boolean;
  /** From the provider's advertised capabilities, not from the backend's name. */
  canInterrupt: boolean;
  /** Set by the host when a send came back refused. See `RestoredDraft`. */
  restoredDraft: RestoredDraft | null;
  onSend: (text: string) => void;
  onInterrupt: () => void;
};

export function Composer({
  disabled,
  turnInProgress,
  sessionEnded,
  closing,
  canInterrupt,
  restoredDraft,
  onSend,
  onInterrupt,
}: Props) {
  const [text, setText] = useState("");

  /* The box is cleared optimistically on send, because a round trip's worth of latency in a text
     box reads as lag. That is only acceptable if a refused send puts the text back — otherwise the
     message is gone with no trace, which is the one outcome this must never produce. */
  useEffect(() => {
    if (restoredDraft === null) return;
    setText(restoredDraft.text);
  }, [restoredDraft]);

  function send() {
    /* `disabled` on the element is NOT sufficient, and assuming it was is how a message got eaten:
       a disabled control cannot be focused or typed into by a user, but if a keydown reaches this
       handler by any route it still runs, sends the text and clears the box. Found by the test for
       exactly that — it sent a message into a conversation that was already being closed, with the
       textarea correctly marked disabled. The refusal has to live here, not only in the attribute. */
    if (disabled) return;
    if (!text.trim()) return;
    onSend(text);
    setText("");
  }

  return (
    <div className="composer">
      {closing && (
        <p className="composer-closing" role="status">
          This conversation is being closed so it can continue in a terminal. Anything still in the
          box has <strong>not</strong> been sent, and is kept.
        </p>
      )}
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
        placeholder={
          closing
            ? "Closing this conversation — no more messages can be sent here."
            : sessionEnded
              ? "This session has ended — start a new one."
              : "Ask the agent..."
        }
      />
      <button onClick={send} disabled={disabled}>Send</button>
      {canInterrupt && (
        <button onClick={onInterrupt} disabled={!turnInProgress}>Stop</button>
      )}
    </div>
  );
}
