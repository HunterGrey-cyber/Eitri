import { useEffect, useRef, useState } from "react";
import type { PanelMode } from "../keymap";
import { HINT_COMPOSER_ATTR } from "../nav";

/** A draft the host is putting back into the box after a send was refused.
 *
 * An object with a `seq` rather than a bare string, because the same text can be refused twice in a
 * row: a new string identical to the old one would not change the effect's dependency, and the
 * second restore would silently not happen. */
export type RestoredDraft = { text: string; seq: number };

type Props = {
  disabled: boolean;
  /** The session is gone (lost or closed). Changes what the box SAYS; `disabled` is what stops it.
   *
   *  It also closes this component's Tab route into INPUT. INPUT on a dead session is an empty
   *  mode -- the textarea is disabled, so there is nothing to type into, and the key table drops
   *  everything but `Escape` there including the `r` the ended/lost rows promise. See the
   *  `sessionEnded` branch below and `resolveKey`'s own `case "i"`. */
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
  /** Set by the host when a send came back refused. See `RestoredDraft`. */
  restoredDraft: RestoredDraft | null;
  /** BROWSE/INPUT/HINT, owned by `App.tsx` because the keyboard table (`../keymap`) decides mode
   *  transitions for the whole panel, not just this component. Only BROWSE vs. everything-else is
   *  distinguished here: HINT is not reachable yet (see `PanelMode`'s own doc comment) and would
   *  fall into the same "not INPUT" branch as BROWSE if it ever were. */
  mode: PanelMode;
  /** Called on focus/blur of the composer's own input control, so a mouse-driven "click the box"
   *  and a keyboard-driven `i`/`Esc` agree on what mode the panel is in -- neither is the sole
   *  source of truth; both write to the same `mode` state in `App.tsx`. */
  onModeChange: (mode: PanelMode) => void;
  /** Changes when `shell` asks for the caret (`Ctrl+l`, via `App.tsx`'s `inputRequest`). A
   *  textarea that is already mounted is focused again; a fresh one takes focus through
   *  `autoFocus` as before. */
  focusRequest?: number;
  /** Whether the global `f` HINT may label this box (`../nav`'s `HINT_COMPOSER_ATTR`). `App.tsx`
   *  passes the negation of `disabled`: a landing on a textarea that cannot take focus would put the
   *  panel in an INPUT with nothing to type into. */
  hintTarget?: boolean;
  onSend: (text: string) => void;
  /** Every change to the box's text, and once more with `""` right after a send (session tabs Task
   *  11, ruling 24): the host mirrors this into a ref so a tab switch can save the unsent draft.
   *  Optional so every existing caller (and this component's own tests) needs no stand-in. */
  onDraftChange?: (text: string) => void;
};

export function Composer({
  disabled,
  sessionEnded,
  closing,
  restoredDraft,
  mode,
  onModeChange,
  focusRequest = 0,
  hintTarget = false,
  onSend,
  onDraftChange,
}: Props) {
  const [text, setText] = useState("");

  /* The box is cleared optimistically on send, because a round trip's worth of latency in a text
     box reads as lag. That is only acceptable if a refused send puts the text back — otherwise the
     message is gone with no trace, which is the one outcome this must never produce. */
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    if (focusRequest > 0 && mode === "input") textareaRef.current?.focus();
  }, [focusRequest, mode]);
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
    onDraftChange?.("");
  }

  return (
    <div className="composer" {...(hintTarget ? { [HINT_COMPOSER_ATTR]: "" } : {})}>
      {closing && (
        <p className="composer-closing" role="status">
          This conversation is being closed so it can continue in a terminal. Anything still in the
          box has <strong>not</strong> been sent, and is kept.
        </p>
      )}
      {mode === "input" ? (
        <textarea
          ref={textareaRef}
          value={text}
          disabled={disabled}
          autoFocus
          onChange={(e) => {
            setText(e.target.value);
            onDraftChange?.(e.target.value);
          }}
          onFocus={() => onModeChange("input")}
          // Mouse-driven "click away": a keyboard-driven Esc already sets BROWSE at the panel level
          // (`keymap.ts`'s `resolveKey`), and this is what keeps a click OUT of the box agreeing
          // with it, without either being the only path that can make the change.
          onBlur={() => onModeChange("browse")}
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
      ) : sessionEnded ? (
        /* A dead session offers no route into INPUT at all, and says so instead of promising `i`.
           The textarea above is `disabled` once the session ends, so INPUT there is an empty mode:
           `autoFocus` cannot take focus, keys keep arriving at the panel root, and `resolveKey`'s
           "input" branch drops everything but `Escape` -- including `r`, which is the one key the
           lost/ended rows above genuinely promise. Not focusable and no `onFocus` here, so Tab
           cannot reach INPUT by the side door either; `resolveKey` refuses `i` on the same
           condition, and `App.tsx` forces BROWSE for a session that dies while INPUT is already
           active. The text names only `r`, the key that actually resolves in the mode the user is
           in. */
        <div className="composer-browse-hint">
          This session has ended. Press r to start a new session here.
        </div>
      ) : (
        // BROWSE's rendering of the same control: not a textarea at all, so there is nothing here
        // for a stray keystroke to land in. `tabIndex` makes it a real focus target, so THREE
        // things converge on the same `onModeChange("input")`: a click (which focuses it, like any
        // focusable element), `i` (handled by the panel's own keydown table, which moves DOM focus
        // here indirectly by mounting the textarea below with `autoFocus`), and Tab -- ordinary
        // keyboard focus-navigation landing on this control the same way it would on a real
        // textarea. That third one is deliberate, not an overlooked side door: a focusable control
        // that does not become active when focus actually reaches it would be the surprising
        // behaviour, not this.
        // It reads like the empty box it stands in for, not like an instruction: the owner asked for
        // the "按 i 开始输入" line to go (2026-09-19), since Ctrl+l now opens the composer anyway.
        // `i`, a click and Tab still reach INPUT from here exactly as before.
        <div className="composer-browse-hint" tabIndex={0} onFocus={() => onModeChange("input")}>
          Ask the agent...
        </div>
      )}
      {/* No Send/Stop buttons here (spec §3.4, removed panel-as-document task 6 fix round 1):
          Enter still sends and Shift+Enter still inserts a newline (both handled above), and Stop
          survives for mouse users in `ActivityLine` instead (V2, session tabs Task 10; formerly
          `StatusLine`) -- gated the same way this one was, on the provider's advertised
          `interrupt` capability, never on the backend's name. */}
    </div>
  );
}
