import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PanelMode } from "../keymap";
import { HINT_COMPOSER_ATTR } from "../nav";
import {
  caretOnFirstLine,
  caretOnLastLine,
  growHeight,
  isImeKey,
  mergeTaken,
  readlineEdit,
} from "../composerKeys";
import type { HistoryWalk } from "../promptHistory";
import { stepHistory } from "../promptHistory";
import { heldBackSlashCommand, slashCommandFlashText } from "../slashCommands";
import { HistorySearch } from "./HistorySearch";

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
  /** C1a (spec §3.2): which end the NEXT `focusRequest` bump should place the caret at -- `"kept"`
   *  (the default) leaves `caretRef` alone (wherever the box was last left, or the end of the draft
   *  the first time), `"end"` is `A`'s own promise (`:h A`) and forces it there regardless of
   *  `caretRef`. Read once per bump, alongside `focusRequest` itself; a caller that never sets this
   *  (every existing one before C1a) keeps today's "kept" behaviour exactly. */
  caretOnFocus?: "kept" | "end";
  /** Whether the global `f` HINT may label this box (`../nav`'s `HINT_COMPOSER_ATTR`). `App.tsx`
   *  passes the negation of `disabled`: a landing on a textarea that cannot take focus would put the
   *  panel in an INPUT with nothing to type into. */
  hintTarget?: boolean;
  onSend: (text: string) => void;
  /** Every change to the box's text, and once more with `""` right after a send (session tabs Task
   *  11, ruling 24): the host mirrors this into a ref so a tab switch can save the unsent draft.
   *  Optional so every existing caller (and this component's own tests) needs no stand-in. */
  onDraftChange?: (text: string) => void;
  /** Whether this tab has a turn running (phase 3, C1). The box stays live and typable through it --
   *  Enter queues a follow-up instead of sending, and `Ctrl+Enter` sends now (ruling 8). */
  running: boolean;
  onQueue?: (text: string) => void;
  onSendNow?: (text: string) => void;
  /** The shared prompt history (ruling 11, 12), newest last. */
  history?: string[];
  /** How many items are queued behind the running turn -- `↑` from the first line takes them back
   *  (ruling 7) before it ever walks history. */
  queueCount?: number;
  onTakeBackQueue?: () => void;
  /** Reply to `take_back_queue`: the queue's texts, merged ahead of whatever is in the box
   *  (ruling 7). An object with a `seq` for the same reason `RestoredDraft` has one. */
  queueTaken?: { texts: string[]; seq: number } | null;
  /** `Ctrl+c` on an idle, non-empty draft (D1, ruling 31): push it to history and clear the box. */
  onHistoryPush?: (text: string) => void;
  /** `Ctrl+c` while a turn runs (D1, ruling 31, N1). */
  onInterrupt?: () => void;
  /** `Ctrl+g`: hand the box's current text to the nvim scratch editor (plan ruling 18). */
  onEditInNvim?: (text: string) => void;
  /** Whether the scratch-editor round trip currently holds this tab's draft (plan ruling 18): the
   *  box goes read-only and says so until it returns. */
  editingInNvim?: boolean;
  /** `?` on an empty box opens the `?` keymap overlay. */
  onOpenKeymap?: () => void;
};

/** C2: the BROWSE stand-in's own draft preview, cut to two lines so it reads like a hint rather
 *  than reproducing the whole unsent message. */
function twoLines(text: string): string {
  const lines = text.split("\n");
  return lines.length > 2 ? `${lines.slice(0, 2).join("\n")}…` : text;
}

export function Composer({
  disabled,
  sessionEnded,
  closing,
  restoredDraft,
  mode,
  onModeChange,
  focusRequest = 0,
  caretOnFocus = "kept",
  hintTarget = false,
  onSend,
  onDraftChange,
  running,
  onQueue = () => {},
  onSendNow = () => {},
  history = [],
  queueCount = 0,
  onTakeBackQueue = () => {},
  queueTaken = null,
  onHistoryPush = () => {},
  onInterrupt = () => {},
  onEditInNvim = () => {},
  editingInNvim = false,
  onOpenKeymap = () => {},
}: Props) {
  const [text, setText] = useState("");
  const [walk, setWalk] = useState<HistoryWalk>({ index: null, stash: "" });
  const [searching, setSearching] = useState(false);
  /* Spec §9.2 (P10): a local flash for a held-back slash command, drawn right here rather than
   *  through the shared footer/band -- see `Chooser`'s own identical `showFlash`/`flash` pair and
   *  its doc comment for why a local one exists at all (the window's own flash can sit behind
   *  something drawn over it). `seq` for the same reason `RestoredDraft` has one: hitting Enter on
   *  the SAME held-back text twice in a row is two flashes, not a no-op second attempt. */
  const [slashFlash, setSlashFlash] = useState<{ text: string; seq: number } | null>(null);
  const slashFlashSeq = useRef(0);
  useEffect(() => {
    if (slashFlash === null) return;
    const seq = slashFlash.seq;
    const timer = setTimeout(() => setSlashFlash((f) => (f?.seq === seq ? null : f)), 2000);
    return () => clearTimeout(timer);
  }, [slashFlash]);
  function showSlashFlash(text: string) {
    slashFlashSeq.current += 1;
    setSlashFlash({ text, seq: slashFlashSeq.current });
  }

  /* The box is cleared optimistically on send, because a round trip's worth of latency in a text
     box reads as lag. That is only acceptable if a refused send puts the text back — otherwise the
     message is gone with no trace, which is the one outcome this must never produce. */
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  /** Where the caret should go the next time the box takes focus: the end, unless the user left it
   *  somewhere in the same text (defect 1: a landing put it at the start). */
  const caretRef = useRef<number | null>(null);

  function setBox(next: string, caret: number | null = null) {
    setText(next);
    onDraftChange?.(next);
    caretRef.current = caret;
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (el === null) return;
      const at = caret ?? next.length;
      el.setSelectionRange(at, at);
    });
  }

  function focusAtCaret() {
    const el = textareaRef.current;
    if (el === null) return;
    el.focus();
    const at = Math.min(caretRef.current ?? el.value.length, el.value.length);
    el.setSelectionRange(at, at);
  }

  useEffect(() => {
    if (mode === "input") {
      // C1a: `A` forces the caret to the end regardless of where the box was last left; `i`/`o`
      // (and every caller that predates C1a) leave `caretRef` alone, i.e. "kept".
      if (caretOnFocus === "end") caretRef.current = null;
      focusAtCaret();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest, mode]);

  // F1: a box that becomes enabled while the panel is in INPUT takes the keys back.
  const wasDisabled = useRef(disabled);
  useEffect(() => {
    if (wasDisabled.current && !disabled && mode === "input") focusAtCaret();
    wasDisabled.current = disabled;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [disabled]);

  useEffect(() => {
    if (restoredDraft === null) return;
    setText(restoredDraft.text);
    caretRef.current = null;
    setWalk({ index: null, stash: "" });
  }, [restoredDraft]);

  useEffect(() => {
    if (queueTaken === null) return;
    setBox(mergeTaken(queueTaken.texts, textareaRef.current?.value ?? text));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queueTaken?.seq]);

  // C3: grow to the content, at most 40% of the panel.
  useLayoutEffect(() => {
    const el = textareaRef.current;
    if (el === null) return;
    el.style.height = "auto";
    const panel = el.closest<HTMLElement>(".agent-ui-root")?.clientHeight || window.innerHeight;
    el.style.height = `${growHeight(el.scrollHeight, panel)}px`;
  }, [text, mode]);

  function submit(now: boolean) {
    if (disabled || editingInNvim) return;
    const body = text;
    // Spec §9.2: a held-back slash command (no-op/error/hangs classed, or interactive-only --
    // `/login`, `/config`, `/resume` always, `/model` only without an argument) is never sent, not
    // even queued or "sent now": the draft stays exactly as typed and the flash names it.
    const held = heldBackSlashCommand(body);
    if (held !== null) {
      showSlashFlash(slashCommandFlashText(held));
      return;
    }
    if (running) {
      if (now) onSendNow(body);
      else if (body.trim()) onQueue(body);
      else return;
    } else {
      if (!body.trim()) return;
      onSend(body);
    }
    setBox("");
    setWalk({ index: null, stash: "" });
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (isImeKey({ isComposing: e.nativeEvent.isComposing, keyCode: e.keyCode })) return;
    const el = e.currentTarget;
    // Wave 4 Task 1: Shift+Tab used to be claimed here (`onShiftTab`); it is now caught by
    // App.tsx's document-capture router (`modeKey.ts`) before it ever reaches this handler.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit(e.ctrlKey);
      return;
    }
    if (e.ctrlKey && !e.shiftKey && !e.altKey) {
      if (e.key === "c") {
        if (running) {
          e.preventDefault();
          onInterrupt();
        } else if (text.trim() !== "") {
          e.preventDefault();
          onHistoryPush(text);
          setBox("");
        }
        return;
      }
      if (e.key === "w" || e.key === "u") {
        const edit = readlineEdit(el.value, el.selectionStart, el.selectionEnd, e.key);
        e.preventDefault();
        if (edit !== null) setBox(edit.value, edit.caret);
        return;
      }
      if (e.key === "r") {
        e.preventDefault();
        setSearching(true);
        return;
      }
      if (e.key === "g") {
        e.preventDefault();
        if (!editingInNvim) onEditInNvim(text);
        return;
      }
    }
    if (e.key === "?" && text === "") {
      e.preventDefault();
      onOpenKeymap();
      return;
    }
    if (e.key === "ArrowUp" && !e.shiftKey && caretOnFirstLine(el.value, el.selectionStart)) {
      if (queueCount > 0) {
        e.preventDefault();
        onTakeBackQueue();
        return;
      }
      const step = stepHistory(history, walk, text, -1);
      if (step !== null) {
        e.preventDefault();
        setWalk(step.walk);
        setBox(step.text);
      }
      return;
    }
    if (e.key === "ArrowDown" && !e.shiftKey && walk.index !== null && caretOnLastLine(el.value, el.selectionStart)) {
      const step = stepHistory(history, walk, text, 1);
      if (step !== null) {
        e.preventDefault();
        setWalk(step.walk);
        setBox(step.text);
      }
    }
  }

  return (
    <div className="composer" {...(hintTarget ? { [HINT_COMPOSER_ATTR]: "" } : {})}>
      {closing && (
        <p className="composer-closing" role="status">
          This conversation is being closed so it can continue in a terminal. Anything still in the
          box has <strong>not</strong> been sent, and is kept.
        </p>
      )}
      {slashFlash !== null && (
        <p className="composer-slash-flash" role="status">
          {slashFlash.text}
        </p>
      )}
      {mode === "input" ? (
        <>
          {searching && (
            <HistorySearch
              history={history}
              onAccept={(t) => {
                setSearching(false);
                setBox(t);
                focusAtCaret();
              }}
              onCancel={() => {
                setSearching(false);
                focusAtCaret();
              }}
            />
          )}
          {/* Claude Code's own shell-prompt look (panel round 2 plan, Task 10; spec §5.1, `mock:
              bottom.html` B `.b-comp`): a bare `❯` line replaces the textarea's bordered box.
              Plain `--nv-fg` (`index.css`'s own `.composer-prompt` doc comment explains why it is
              not a mode colour). */}
          <span className="composer-prompt" aria-hidden="true">
            ❯
          </span>
          <textarea
            ref={textareaRef}
            value={text}
            disabled={disabled}
            readOnly={editingInNvim}
            autoFocus
            onChange={(e) => {
              setText(e.target.value);
              onDraftChange?.(e.target.value);
            }}
            onFocus={() => onModeChange("input")}
            // Mouse-driven "click away": a keyboard-driven Esc already sets BROWSE at the panel
            // level (`keymap.ts`'s `resolveKey`), and this is what keeps a click OUT of the box
            // agreeing with it, without either being the only path that can make the change. The
            // history-search line takes real DOM focus of its own (C5, ruling 12): a blur that
            // lands there must not report BROWSE, or opening `Ctrl+r` would itself leave INPUT.
            onBlur={(e) => {
              if (!(e.relatedTarget instanceof HTMLElement && e.relatedTarget.closest(".history-search"))) {
                onModeChange("browse");
              }
            }}
            onKeyDown={onKeyDown}
            onKeyUp={(e) => {
              caretRef.current = e.currentTarget.selectionStart;
            }}
            onMouseUp={(e) => {
              caretRef.current = e.currentTarget.selectionStart;
            }}
            placeholder={
              closing
                ? "Closing this conversation — no more messages can be sent here."
                : sessionEnded
                  ? "This session has ended — start a new one."
                  : editingInNvim
                    ? "editing in nvim — :wq to return"
                    : running
                      ? "Queue a follow-up…"
                      : "Ask the agent..."
            }
          />
        </>
      ) : sessionEnded ? (
        /* A dead session offers no route into INPUT at all, and says so instead of promising `i`.
           The textarea above is `disabled` once the session ends, so INPUT there is an empty mode:
           `autoFocus` cannot take focus, keys keep arriving at the panel root, and `resolveKey`'s
           "input" branch drops everything but `Escape` -- including `r`, which is the one key the
           lost/ended rows above genuinely promise. Not focusable and no `onFocus` here, so Tab
           cannot reach INPUT by the side door either; `resolveKey` refuses `i` on the same
           condition, and `App.tsx` forces BROWSE for a session that dies while INPUT is already
           active. The text names only `r`, the key that actually resolves in the mode the user is
           in. Since v1 polish item 7 it names no key at all: the lost/ended/failed row just above
           already says "Press r…", and a failed tab read it twice. */
        <div className="composer-browse-hint">This session has ended.</div>
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
        // the "按 i 开始输入" line to go (2026-09-19), when `Ctrl+l` was the only route into it. Its
        // premise ended with panel round 2's decision 4: `Ctrl+l` (and `prefix a`, and a tab switch)
        // land BROWSE now, not INPUT, so a reader can once again land here with no on-screen route
        // into the box at all. C1b/S3 (spec §3.3) restores it: a dim `i or Ctrl+j to type`, after the
        // placeholder or the draft alike, in BOTH cases -- never in INPUT (the caret is the sign
        // there), on an ended session (its own branch above says `r` instead), or while a scratch
        // round trip owns the draft (`editingInNvim`: typing here would only be lost to nvim's own
        // buffer). `i`, a click and Tab still reach INPUT from here exactly as before.
        // The r2-gui GUI pass (2026-09-26): the same bare `❯` line INPUT draws (`mock: bottom.html`
        // B), so the bottom keeps its shape across `i`/`Esc`; the band's mode block is the mode.
        <>
          <span className="composer-prompt" aria-hidden="true">
            ❯
          </span>
          <div className="composer-browse-hint" tabIndex={0} onFocus={() => onModeChange("input")}>
            {text.trim() === "" ? (
              "Ask the agent..."
            ) : (
              <span className="composer-draft">{twoLines(text)}</span>
            )}
            {!editingInNvim && (
              <span className="composer-hint" aria-hidden="true">
                {" "}
                — i or Ctrl+j to type
              </span>
            )}
            {queueCount > 0 && <span className="composer-queued">+{queueCount} queued</span>}
          </div>
        </>
      )}
      {/* No Send/Stop buttons here (spec §3.4, removed panel-as-document task 6 fix round 1):
          Enter still sends and Shift+Enter still inserts a newline (both handled above), and Stop
          survives for mouse users in `ActivityLine` instead (V2, session tabs Task 10; formerly
          `StatusLine`) -- gated the same way this one was, on the provider's advertised
          `interrupt` capability, never on the backend's name. */}
    </div>
  );
}
