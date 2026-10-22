import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { PanelMode } from "../keymap";
import { HINT_COMPOSER_ATTR } from "../nav";
import {
  caretOnFirstLine,
  caretOnLastLine,
  enterNewline,
  growHeight,
  isImeKey,
  mergeTaken,
  readlineEdit,
} from "../composerKeys";
import type { HistoryWalk } from "../promptHistory";
import { stepHistory } from "../promptHistory";
import { pickerStep } from "../chooser";
import {
  slashAction,
  heldBackSlashCommand,
  matchCompletions,
  menuToken,
  slashCommandFlashText,
  slashMenuWindow,
  SLASH_MENU_ROWS,
  startsMatch,
} from "../slashCommands";
import type { CompletionCommand, SlashAction } from "../slashCommands";
import { HistorySearch } from "./HistorySearch";
import { SlashMenu } from "./SlashMenu";
import { NO_TEXT_ASSIST } from "../textField";
import { runningPlaceholder } from "../queueLines";

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
  /** Which end the NEXT `focusRequest` bump should place the caret at -- `"kept"`
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
  /** Every change to the box's text, and once more with `""` right after a send: the host mirrors this into a ref so
   *  a tab switch can save the unsent draft. Optional so every existing caller (and this component's own tests) needs
   *  no stand-in. */
  onDraftChange?: (text: string) => void;
  /** Whether this tab has a turn running (phase 3, C1). The box stays live and typable through it --
   *  Enter puts the message into the running turn, or holds it for the turn's end when the agent cannot
   *  take it (`enterSteers`; Rust decides, `onQueue` carries both), and `Ctrl+Enter` sends now. */
  running: boolean;
  /** Whether Enter while the turn runs puts the message into it (the agent's capability `steer`): only
   *  what the empty box says changes. */
  enterSteers?: boolean;
  onQueue?: (text: string) => void;
  onSendNow?: (text: string) => void;
  /** The shared prompt history, newest last. */
  history?: string[];
  /** How many messages are held for the turn's end -- `↑` from the first line takes them back
   *  before it ever walks history. One sent into the running turn is not counted: the agent
   *  may already have read it. */
  queueCount?: number;
  onTakeBackQueue?: () => void;
  /** Reply to `take_back_queue`: the queue's texts, merged ahead of whatever is in the box
   *  (the queued messages were written first). An object with a `seq` for the same reason `RestoredDraft` has one. */
  queueTaken?: { texts: string[]; seq: number } | null;
  /** `Ctrl+c` on an idle, non-empty draft: push it to history and clear the box, as Claude Code does. */
  onHistoryPush?: (text: string) => void;
  /** `Ctrl+c` while a turn runs: interrupt it, as Claude Code does. */
  onInterrupt?: () => void;
  /** `Ctrl+g`: hand the box's current text to the nvim scratch editor, as Claude Code's `Ctrl+g` opens `$EDITOR`. */
  onEditInNvim?: (text: string) => void;
  /** Whether the scratch-editor round trip currently holds this tab's draft: the
   *  box goes read-only and says so until it returns. */
  editingInNvim?: boolean;
  /** `?` on an empty box opens the `?` keymap overlay. */
  onOpenKeymap?: () => void;
  /** Whether a typed `/command` is judged as one of Claude Code's (a held-back one is refused with a
   *  flash). False in another agent's tab, which sends every draft as typed. */
  claudeSlashCommands?: boolean;
  onSlashAction?: (action: SlashAction) => void;
  /** The commands the `/` menu offers in this tab (`../slashCommands`' `completionSource`). `null`, or
   *  absent, is "no menu": an agent that reports no list (Codex) has nothing to complete. */
  slashCommands?: readonly CompletionCommand[] | null;
  /** The keys the `Ctrl+r` search stops before they bubble, handed
   *  to the host's typing guard (`HistorySearch`'s own `onLineKey`). */
  onLineKey?: (event: React.KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => void;
  /** The key BROWSE's stand-in names at its right end, the way the start screen's menu shows its keys: the
   *  key that opens the box. Since 2026-10-15 it replaces the "i or Ctrl+j to type" sentence that followed
   *  the placeholder (`Ctrl+j` is in the `?` sheet). */
  browseKey?: string;
  /** What the empty box says while the panel does not hold the keys (the start screen: how to reach it).
   *  Only then: with the keys here the caret is the sign, and grey text behind an input method's preedit
   *  reads as a second line of input. */
  idleHint?: string;
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
  enterSteers = false,
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
  claudeSlashCommands = true,
  onSlashAction,
  slashCommands = null,
  onLineKey,
  browseKey = "i",
  idleHint,
}: Props) {
  const [text, setText] = useState("");
  const [walk, setWalk] = useState<HistoryWalk>({ index: null, stash: "" });
  const [searching, setSearching] = useState(false);
  /* The `/` menu. `caret` mirrors the textarea's selection (a keystroke, a click or a selection change
     moves it without the text changing), `menuIndex`/`menuTop` are the selected row and the first row
     drawn, and `menuDismissed` is Esc: it holds until the text next changes by typing. A draft put into
     the box by the panel itself (a history walk, the queue taken back, a refused send restored, a search
     result) starts dismissed too, so the menu only opens from what the user types and `↑`/`↓` keep walking. */
  const [caret, setCaret] = useState(0);
  const [menuIndex, setMenuIndex] = useState(0);
  const [menuTop, setMenuTop] = useState(0);
  const [menuDismissed, setMenuDismissed] = useState(false);
  /* A local flash for a held-back slash command, drawn right here rather than
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

  /** The caret `setBox` has decided on and not yet placed in the box. React reports a `select` for a
   *  keydown with the box's selection as it still was, before the new text is drawn; reading that would
   *  put the menu's caret back where it was, so selection reports are ignored until it is placed. */
  const placingCaret = useRef(false);
  function trackCaret(el: HTMLTextAreaElement) {
    if (!placingCaret.current) setCaret(el.selectionStart);
  }

  /** Puts `next` in the box. `recalled` is a draft the user did not type (history, the queue, a search
   *  result): a recalled `/command` must not open the menu, which would own `↑`/`↓` and stop the walk on it. */
  function setBox(next: string, caret: number | null = null, recalled = false) {
    setText(next);
    onDraftChange?.(next);
    caretRef.current = caret;
    setCaret(caret ?? next.length);
    setMenuIndex(0);
    setMenuTop(0);
    setMenuDismissed(recalled);
    placingCaret.current = true;
    requestAnimationFrame(() => {
      placingCaret.current = false;
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
    // This effect only ever shows what the host already decided to put
    // in the box -- it does NOT mirror it back to Rust itself. `restoredDraft` fires on every tab
    // switch too (cleared to "", `App.tsx`'s `tabs` arm) and on every inbound `draft` echo from
    // Rust, and mirroring unconditionally here re-echoed both of those back out as a fresh `draft`
    // post nobody asked for (found by a regression this task's own fix first introduced: a leader
    // sequence test with no composer in play at all started posting one). The host mirrors the
    // RECOVERED text itself, once, right where it computes it (`App.tsx`'s send-refusal handler).
    setText(restoredDraft.text);
    caretRef.current = null;
    setCaret(restoredDraft.text.length);
    setMenuIndex(0);
    setMenuTop(0);
    setMenuDismissed(true);
    setWalk({ index: null, stash: "" });
  }, [restoredDraft]);

  useEffect(() => {
    if (queueTaken === null) return;
    setBox(mergeTaken(queueTaken.texts, textareaRef.current?.value ?? text), null, true);
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

  /* The menu is open while the draft is one line whose first word starts with `/`, the caret is in that
     word, something matches, Esc has not closed it since the text last changed, and nothing else owns
     the box (the history search, the scratch editor, a dead session). */
  const token =
    slashCommands !== null && mode === "input" && !disabled && !editingInNvim && !searching ? menuToken(text, caret) : null;
  const matches = token !== null && slashCommands !== null ? matchCompletions(slashCommands, token.query) : [];
  const menuOpen = token !== null && matches.length > 0 && !menuDismissed;
  const selected = Math.min(menuIndex, Math.max(0, matches.length - 1));
  const shownFrom = slashMenuWindow(selected, menuTop, matches.length);

  function selectMenu(next: number) {
    setMenuIndex(next);
    setMenuTop(slashMenuWindow(next, shownFrom, matches.length));
  }
  function moveMenu(step: 1 | -1) {
    selectMenu((selected + step + matches.length) % matches.length);
  }

  /** Puts the command's name into the draft's first word, followed by a space the caret lands after (one
   *  already there is reused), and leaves the rest of the draft alone. */
  function completeWith(command: CompletionCommand) {
    if (token === null) return;
    const rest = text.slice(token.end);
    const head = `/${command.name} `;
    setBox(head + rest.replace(/^ /, ""), head.length);
    textareaRef.current?.focus();
  }

  function submit(now: boolean) {
    if (disabled || editingInNvim) return;
    const body = text;
    const action = claudeSlashCommands ? slashAction(body) : null;
    if (action !== null) {
      onSlashAction?.(action);
      // A pointer keeps its draft, as a held-back word does; its text is the status band's to show (the
      // panel's handler puts it there), so the composer drawing it too would say it twice.
      if (action.kind !== "pointer") {
        setBox("");
        setWalk({ index: null, stash: "" });
      }
      return;
    }
    // A terminal-only command keeps its draft so the user can revise it.
    const held = claudeSlashCommands ? heldBackSlashCommand(body) : null;
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
    /* The open `/` menu takes its keys ahead of everything below, so `↑`/`↓` never reach the prompt
       history or the queue while it shows. It is the textarea's own handler: an IME's key was returned
       above, so a composing Enter or Esc never completes or closes anything. */
    if (menuOpen && token !== null && !e.altKey && !e.metaKey) {
      const step = e.shiftKey ? null : pickerStep(e);
      if (step !== null) {
        e.preventDefault();
        moveMenu(step);
        return;
      }
      if (e.key === "Tab" && !e.shiftKey && !e.ctrlKey) {
        e.preventDefault();
        completeWith(matches[selected]);
        return;
      }
      // Enter completes only when what is typed matches the selected name from its start (or a word's
      // start in it, `startsMatch`) and is not yet the whole name; a name that merely contains the letters
      // is for Tab and the arrows, and Enter then sends the text as typed, so a user's own `/view` is not
      // rewritten into `/review`. With Ctrl Enter keeps its own meaning, and Shift+Enter is a newline.
      if (
        e.key === "Enter" &&
        !e.shiftKey &&
        !e.ctrlKey &&
        el.value.slice(0, token.end) !== `/${matches[selected].name}` &&
        startsMatch(matches[selected], token.query)
      ) {
        e.preventDefault();
        completeWith(matches[selected]);
        return;
      }
      if (e.key === "Escape" && !e.shiftKey && !e.ctrlKey) {
        // Closes the menu and nothing else: the key does not bubble to the panel's own Esc (back to
        // browsing). The typing guard is told of it, as the history search tells of its own.
        e.preventDefault();
        e.stopPropagation();
        onLineKey?.(e);
        setMenuDismissed(true);
        return;
      }
    }
    // Shift+Tab used to be claimed here (`onShiftTab`); it is now caught by
    // App.tsx's document-capture router (`modeKey.ts`) before it ever reaches this handler.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      // Alt+Enter and a backslash right before the caret are Claude Code's own
      // newline keys, not a send. Not while nvim owns the draft (`editingInNvim`): `readOnly`
      // stops typing but not this handler, and a newline added here would be lost to nvim's own write.
      const newline = editingInNvim ? null : enterNewline(el.value, el.selectionStart, el.selectionEnd, e);
      if (newline !== null) {
        setBox(newline.value, newline.caret);
        return;
      }
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
        setBox(step.text, null, true);
      }
      return;
    }
    if (e.key === "ArrowDown" && !e.shiftKey && walk.index !== null && caretOnLastLine(el.value, el.selectionStart)) {
      const step = stepHistory(history, walk, text, 1);
      if (step !== null) {
        e.preventDefault();
        setWalk(step.walk);
        setBox(step.text, null, true);
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
              onLineKey={onLineKey}
              onAccept={(t) => {
                setSearching(false);
                setBox(t, null, true);
                focusAtCaret();
              }}
              onCancel={() => {
                setSearching(false);
                focusAtCaret();
              }}
            />
          )}
          {menuOpen && (
            <SlashMenu
              rows={matches.slice(shownFrom, shownFrom + SLASH_MENU_ROWS)}
              firstIndex={shownFrom}
              total={matches.length}
              selected={selected}
              onPick={(i) => completeWith(matches[i])}
              onWheel={(by) => selectMenu(Math.max(0, Math.min(matches.length - 1, selected + by)))}
            />
          )}
          {/* No prompt glyph: the tinted block the composer is drawn as (`index.css`) is its mark. */}
          <textarea
            ref={textareaRef}
            {...NO_TEXT_ASSIST}
            value={text}
            // One line when empty, the height BROWSE's stand-in has, so `i` does not grow the box and push
            // the conversation up; the layout effect above grows it with the text.
            rows={1}
            // A fixed name: the placeholder is empty while the box holds the keys (and says different things
            // otherwise), and a control with no name is announced as just "edit text".
            aria-label="Message the agent"
            disabled={disabled}
            readOnly={editingInNvim}
            autoFocus
            onChange={(e) => {
              setText(e.target.value);
              onDraftChange?.(e.target.value);
              setCaret(e.target.selectionStart);
              setMenuIndex(0);
              setMenuTop(0);
              setMenuDismissed(false);
            }}
            onSelect={(e) => trackCaret(e.currentTarget)}
            onFocus={() => onModeChange("input")}
            // Mouse-driven "click away": a keyboard-driven Esc already sets BROWSE at the panel
            // level (`keymap.ts`'s `resolveKey`), and this is what keeps a click OUT of the box
            // agreeing with it, without either being the only path that can make the change. The
            // history-search line takes real DOM focus of its own: a blur that
            // lands there must not report BROWSE, or opening `Ctrl+r` would itself leave INPUT.
            onBlur={(e) => {
              if (!(e.relatedTarget instanceof HTMLElement && e.relatedTarget.closest(".history-search"))) {
                onModeChange("browse");
              }
            }}
            onKeyDown={onKeyDown}
            onKeyUp={(e) => {
              caretRef.current = e.currentTarget.selectionStart;
              trackCaret(e.currentTarget);
            }}
            onMouseUp={(e) => {
              caretRef.current = e.currentTarget.selectionStart;
              trackCaret(e.currentTarget);
            }}
            placeholder={
              closing
                ? "Closing this conversation — no more messages can be sent here."
                : sessionEnded
                  ? "This session has ended — start a new one."
                  : editingInNvim
                    ? "editing in nvim — :wq to return"
                    : running
                      ? runningPlaceholder(enterSteers)
                      : // Nothing in an ordinary empty box while it holds the keys: the caret and the box's
                        // INPUT frame already say where the keys are, and grey text behind an input
                        // method's preedit reads as a second line of input. Without the keys there is no
                        // caret and no preedit, and the host may say how to get here.
                        idleHint
            }
          />
        </>
      ) : sessionEnded ? (
        /* An ended session cannot enter INPUT: its textarea is disabled, `resolveKey` refuses `i`,
           and `App.tsx` forces BROWSE if the session ends during INPUT. This hint has no focus
           target or `onFocus`, so Tab cannot enter INPUT either. The lost, ended, or failed row
           above already names `r`, so only state that the session has ended here. */
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
        // It reads like the empty box it stands in for, not like an instruction: a "press i to type"
        // line went away (2026-09-19) while `Ctrl+l` was the only route into the box. That premise
        // ended once `Ctrl+l` (and `prefix a`, and a tab switch) landed BROWSE, not INPUT, so a reader
        // can once again land here with no on-screen route into the box at all. So it is back: a dim `i or Ctrl+j to
        // type`, after the placeholder or the draft alike, in BOTH cases -- never in INPUT (the caret is the sign
        // there), on an ended session (its own branch above says `r` instead), or while a scratch round trip owns the
        // draft (`editingInNvim`: typing here would only be lost to nvim's own buffer). `i`, a click and Tab still
        // reach INPUT from here exactly as before. Since 2026-10-15 the reminder is the key alone at the box's right
        // end (`browseKey`), as the start screen's menu shows its keys, not the sentence. The same card INPUT draws,
        // so the bottom keeps its shape across `i`/`Esc`; the band's mode block is the mode.
        <>
          <div className="composer-browse-hint" tabIndex={0} onFocus={() => onModeChange("input")}>
            {text.trim() === "" ? (
              "Ask the agent..."
            ) : (
              <span className="composer-draft">{twoLines(text)}</span>
            )}
            {queueCount > 0 && <span className="composer-queued">+{queueCount} queued</span>}
            {!editingInNvim && (
              <span className="composer-key" aria-hidden="true">
                {browseKey}
              </span>
            )}
          </div>
        </>
      )}
      {/* No Send/Stop buttons here (a document, not a form):
          Enter still sends and Shift+Enter still inserts a newline (both handled above), and Stop
          survives for mouse users in `ActivityLine` instead (formerly
          `StatusLine`) -- gated the same way this one was, on the provider's advertised
          `interrupt` capability, never on the backend's name. */}
    </div>
  );
}
