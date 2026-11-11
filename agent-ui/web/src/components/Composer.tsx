import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { isPlainAnswerKey } from "../keymap";
import type { PanelMode } from "../keymap";
import { HINT_COMPOSER_ATTR } from "../nav";
import {
  caretOnFirstLine,
  caretOnLastLine,
  enterNewline,
  growHeight,
  isImeKey,
  lineEdge,
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
import { atToken, atInsertion, withEditorMentions } from "../atMenu";
import { AtMenu } from "./AtMenu";
import { SlashMenu } from "./SlashMenu";
import { NO_TEXT_ASSIST } from "../textField";
import { runningPlaceholder } from "../queueLines";
import { isModifierKey } from "../typingGuard";
import { CTRL_X_ONLY_ENTER_FLASH } from "../app/flashes";

/** How long `Ctrl+x` waits for its second key. */
export const CTRL_X_CHORD_MS = 2000;
/** What the band's showcmd says while `Ctrl+x` waits for its second key. */
export const CTRL_X_SHOWCMD = "Ctrl+x -";

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
  /** Which end the NEXT `focusRequest` bump places the caret at: `"kept"` defaults to the saved
   *  position or the end of a fresh draft; `"end"` is `A`'s promise and overrides that position.
   *  Read once per bump; an absent prop keeps the saved position. */
  caretOnFocus?: "kept" | "end";
  /** Whether the global `f` HINT may label this box (`../nav`'s `HINT_COMPOSER_ATTR`). `App.tsx`
   *  passes the negation of `disabled`: a landing on a textarea that cannot take focus would put the
   *  panel in an INPUT with nothing to type into. */
  hintTarget?: boolean;
  onSend: (text: string) => void;
  /** Enter on a `!` line in a Claude tab: it runs as the person's own command. The one way a line runs; the
   *  rest of the panel's paths send words. */
  onRunShell?: (text: string) => void;
  /** Every change to the box's text, and once more with `""` right after a send: the host mirrors this into a ref so
   *  a tab switch can save the unsent draft. Optional so every existing caller (and this component's own tests) needs
   *  no stand-in. */
  onDraftChange?: (text: string) => void;
  /** Whether this tab has a turn running. The box stays live and typable through it --
   *  Enter puts the message into the running turn, or holds it for the turn's end when the agent cannot
   *  take it (`enterSteers`; Rust decides, `onQueue` carries both), and `Ctrl+Enter` sends now. */
  running: boolean;
  /** Whether Enter while the turn runs puts the message into it (the agent's capability `steer`): only
   *  what the empty box says changes. */
  enterSteers?: boolean;
  /** A message for the running turn (Enter while one runs), or, with `afterTurn`, one that waits for the turn's
   *  end whatever Enter does with what is typed after it (`Ctrl+x Enter`, sent with or without a turn). */
  onQueue?: (text: string, options?: { afterTurn: boolean }) => void;
  /** The `Ctrl+x` chord's showcmd: `CTRL_X_SHOWCMD` while it waits for its second key, `null` when it ends.
   *  Only a composer given this has the chord; without it (the start screen's) `Ctrl+x` is the box's own cut. */
  onChord?: (showcmd: string | null) => void;
  /** Filled by the box with a function for keys the page takes before the box sees them (the document-capture
   *  Shift+Tab): while the `Ctrl+x` chord waits it ends the chord as any other key does, with the chord's flash,
   *  and returns true, so the caller does nothing else with the key; false when no chord waits. */
  chordEnd?: { current: (() => boolean) | null };
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
  atPaths?: { items: string[]; seq: number } | null;
  onAtQuery?: (query: string, seq: number) => void;
  /** The keys the `Ctrl+r` search stops before they bubble, handed
   *  to the host's typing guard (`HistorySearch`'s own `onLineKey`). */
  onLineKey?: (event: React.KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => void;
  /** The INPUT entry key at the BROWSE box's right end, like the start menu's keys.
   *  `Ctrl+j` is described in the `?` sheet. */
  browseKey?: string;
  /** What the empty box says while the panel does not hold the keys (the start screen: how to reach it).
   *  Only then: with the keys here the caret is the sign, and grey text behind an input method's preedit
   *  reads as a second line of input. */
  idleHint?: string;
};

/** `Ctrl+x` itself, nothing else held (Caps Lock may make the letter upper case). */
function isCtrlX(e: { key: string; ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean }): boolean {
  return e.ctrlKey && !e.shiftKey && !e.altKey && !e.metaKey && (e.key === "x" || e.key === "X");
}

/** The BROWSE stand-in's own draft preview, cut to two lines so it reads like a hint rather
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
  onChord,
  chordEnd,
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
  atPaths = null,
  onRunShell,
  onAtQuery,
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
  const [atIndex, setAtIndex] = useState(0);
  const [atTop, setAtTop] = useState(0);
  const [atMoved, setAtMoved] = useState(false);
  const [atDirectory, setAtDirectory] = useState<{ query: string; items: string[] } | null>(null);
  const [atAsked, setAtAsked] = useState<{ query: string; seq: number } | null>(null);
  const atSeq = useRef(0);
  const atQueryCallback = useRef(onAtQuery);
  atQueryCallback.current = onAtQuery;
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

  /* `Ctrl+x` waiting for its second key: the timer that ends it, `null` when no chord waits. The host's
     `onChord` is read through a ref so the timer's end reaches the current one. */
  const chordTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onChordRef = useRef(onChord);
  onChordRef.current = onChord;
  function startChord() {
    if (chordTimer.current !== null) clearTimeout(chordTimer.current);
    chordTimer.current = setTimeout(() => {
      chordTimer.current = null;
      onChordRef.current?.(null);
    }, CTRL_X_CHORD_MS);
    onChordRef.current?.(CTRL_X_SHOWCMD);
  }
  function endChord() {
    if (chordTimer.current === null) return;
    clearTimeout(chordTimer.current);
    chordTimer.current = null;
    onChordRef.current?.(null);
  }
  useEffect(() => endChord, []);
  useEffect(() => {
    if (mode !== "input") endChord();
  }, [mode]);
  // Only refs and a state setter inside, so the first render's closure stays right.
  useEffect(() => {
    if (chordEnd === undefined) return;
    chordEnd.current = () => {
      if (chordTimer.current === null) return false;
      endChord();
      showSlashFlash(CTRL_X_ONLY_ENTER_FLASH);
      return true;
    };
    return () => {
      chordEnd.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chordEnd]);

  /* The box is cleared optimistically on send, because a round trip's worth of latency in a text
     box reads as lag. That is only acceptable if a refused send puts the text back — otherwise the
     message is gone with no trace, which is the one outcome this must never produce. */
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  /** Where the caret goes on focus: the end, unless the user left it elsewhere in the same text. */
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
    setAtDirectory(null);
    setText(next);
    onDraftChange?.(next);
    caretRef.current = caret;
    setCaret(caret ?? next.length);
    setAtIndex(0);
    setAtTop(0);
    setAtMoved(false);
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
      // `A` forces the caret to the end; `i`/`o` leave `caretRef` alone ("kept").
      if (caretOnFocus === "end") caretRef.current = null;
      focusAtCaret();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest, mode]);

  // The `Ctrl+r` search belongs to the INPUT it was opened in. Leaving INPUT another way than its own Esc (a
  // tab switch or an arrival lands BROWSE) ends it, so the next `i` opens the box rather than the search line.
  useEffect(() => {
    if (mode !== "input") setSearching(false);
  }, [mode]);

  // A box that becomes enabled while the panel is in INPUT takes the keys back.
  const wasDisabled = useRef(disabled);
  useEffect(() => {
    if (wasDisabled.current && !disabled && mode === "input") focusAtCaret();
    wasDisabled.current = disabled;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [disabled]);

  useEffect(() => {
    if (restoredDraft === null) return;
    // This effect shows the host's text without mirroring it back. Tab switches and inbound `draft`
    // echoes also set `restoredDraft`; echoing them would post an unrequested draft back to Rust.
    // The host mirrors recovered text once, in its send-refusal handler.
    setAtDirectory(null);
    setText(restoredDraft.text);
    caretRef.current = null;
    setCaret(restoredDraft.text.length);
    setAtIndex(0);
    setAtTop(0);
    setAtMoved(false);
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

  // Grow to the content, at most 40% of the panel.
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

  const mention =
    onAtQuery !== undefined && claudeSlashCommands && mode === "input" && !disabled && !editingInNvim &&
    !searching && !menuOpen && !menuDismissed ? atToken(text, caret) : null;
  const atQuery = mention?.query;
  useEffect(() => {
    setAtIndex(0);
    setAtTop(0);
    setAtMoved(false);
    setAtAsked(null);
    if (atQuery === undefined || atQueryCallback.current === undefined) return;
    const timer = setTimeout(() => {
      const seq = ++atSeq.current;
      setAtAsked({ query: atQuery, seq });
      atQueryCallback.current?.(atQuery, seq);
    }, 60);
    return () => clearTimeout(timer);
  }, [atQuery, mention?.start]);
  const atFiles = mention === null
    ? []
    : atAsked?.query === mention.query && atPaths?.seq === atAsked?.seq
      ? atPaths?.items ?? []
      : atDirectory?.query === mention.query ? atDirectory.items : [];
  // A quoted word names a file, whatever its letters spell.
  const atQuoted = mention !== null && text[mention.start + 1] === '"';
  const atItems = mention === null ? [] : atQuoted ? atFiles : withEditorMentions(mention.query, atFiles);
  const atOpen = atItems.length > 0;
  const atSelected = Math.min(atIndex, Math.max(0, atItems.length - 1));
  const atFrom = slashMenuWindow(atSelected, atTop, atItems.length);
  function selectAt(next: number) {
    setAtIndex(next);
    setAtTop(slashMenuWindow(next, atFrom, atItems.length));
    setAtMoved(true);
  }
  function completeAt(path: string) {
    if (mention === null) return;
    const insertion = atInsertion(path, atQuoted);
    const rest = text.slice(mention.end);
    const head = text.slice(0, mention.start) + insertion.text;
    setBox(head + (insertion.keepOpen ? rest : rest.replace(/^ /, "")), head.length);
    setMenuDismissed(!insertion.keepOpen);
    if (insertion.keepOpen) {
      // Keep the chosen directory visible while its children are requested.
      setAtDirectory({ query: path, items: atItems.filter((item) => item.startsWith(path)) });
    }
    textareaRef.current?.focus();
  }

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

  /** What a `!` first turns the box into, as Claude Code's shell mode: Enter runs the line. */
  const shellMode = claudeSlashCommands === true && text.startsWith("!");

  /** `how`: Enter's send, `Ctrl+Enter`'s send now, or `Ctrl+x Enter`'s send after the turn. */
  function submit(how: "enter" | "now" | "after") {
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
    if (claudeSlashCommands && body.startsWith("!")) {
      // `!` with nothing after it waits for the command, as Claude Code's shell mode does.
      if (body.slice(1).trim() === "") return;
      onRunShell?.(body);
      setBox("");
      setWalk({ index: null, stash: "" });
      return;
    }
    // A terminal-only command keeps its draft so the user can revise it.
    const held = claudeSlashCommands ? heldBackSlashCommand(body) : null;
    if (held !== null) {
      showSlashFlash(slashCommandFlashText(held));
      return;
    }
    if (how === "after") {
      // Always the queue, turn shown or not: the host holds it for the running turn's end, or sends it as the
      // next turn when none runs, and nothing on its way can put it into a turn.
      if (!body.trim()) return;
      onQueue(body, { afterTurn: true });
    } else if (running) {
      if (how === "now") onSendNow(body);
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
    const ime = isImeKey({ isComposing: e.nativeEvent.isComposing, keyCode: e.keyCode });
    /* The key after `Ctrl+x`, ahead of everything else (the open menus too). A bare modifier waits for the key
       it modifies, and the held `Ctrl+x`'s own repeats wait too. A fresh, unmodified Enter sends the draft after
       the turn; any other key ends the chord, says so, and does nothing else. An input method's key ends it
       as well, but is left to the input method. */
    if (chordTimer.current !== null) {
      const plain = !e.ctrlKey && !e.shiftKey && !e.altKey && !e.metaKey;
      if (!ime && isModifierKey(e.key)) return;
      if (!ime && e.repeat && isCtrlX(e)) {
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      endChord();
      if (ime) {
        showSlashFlash(CTRL_X_ONLY_ENTER_FLASH);
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      onLineKey?.(e);
      if (e.key === "Enter" && plain && !e.repeat) submit("after");
      else showSlashFlash(CTRL_X_ONLY_ENTER_FLASH);
      return;
    }
    if (ime) return;
    const el = e.currentTarget;
    /* `Ctrl+x` with nothing selected starts the chord (only where the host takes it, `onChord`); with a
       selection it is the box's own cut. */
    if (onChord !== undefined && isCtrlX(e) && el.selectionStart === el.selectionEnd) {
      e.preventDefault();
      e.stopPropagation();
      onLineKey?.(e);
      if (!e.repeat) startChord();
      return;
    }
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
    if (atOpen && !e.altKey && !e.metaKey) {
      const step = e.shiftKey ? null : pickerStep(e);
      if (step !== null) {
        e.preventDefault();
        selectAt((atSelected + step + atItems.length) % atItems.length);
        return;
      }
      if (!e.shiftKey && !e.ctrlKey && (e.key === "Tab" || (e.key === "Enter" && atMoved))) {
        e.preventDefault();
        completeAt(atItems[atSelected]);
        return;
      }
      if (e.key === "Escape" && !e.shiftKey && !e.ctrlKey) {
        e.preventDefault();
        e.stopPropagation();
        onLineKey?.(e);
        setMenuDismissed(true);
        return;
      }
    }
    // `modeKey.ts` claims Shift+Tab in document capture before this handler.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      // A held Enter's repeat sends nothing: only a fresh press does. The keys can come back to this box while a key
      // pressed elsewhere is still held -- the Enter of `:wq` after `Ctrl+g` -- and its repeats would send the draft
      // nobody asked to send.
      if (e.repeat) return;
      // Alt+Enter and a backslash right before the caret are Claude Code's own
      // newline keys, not a send. Not while nvim owns the draft (`editingInNvim`): `readOnly`
      // stops typing but not this handler, and a newline added here would be lost to nvim's own write.
      const newline = editingInNvim ? null : enterNewline(el.value, el.selectionStart, el.selectionEnd, e);
      if (newline !== null) {
        setBox(newline.value, newline.caret);
        return;
      }
      submit(e.ctrlKey ? "now" : "enter");
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
      // Readline's line start and end, as Claude Code's own input has them. The caret only moves:
      // the box's text is untouched, so there is no `setBox`.
      if (e.key === "a" || e.key === "e") {
        e.preventDefault();
        const at = lineEdge(el.value, el.selectionStart, e.key === "a" ? "start" : "end");
        el.setSelectionRange(at, at);
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
    if ((e.key === "?" && text === "") || (e.key === "F1" && !e.shiftKey && isPlainAnswerKey(e.nativeEvent))) {
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
    <div className={shellMode ? "composer composer-shell" : "composer"} {...(hintTarget ? { [HINT_COMPOSER_ATTR]: "" } : {})}>
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
          {atOpen && (
            <AtMenu
              rows={atItems.slice(atFrom, atFrom + SLASH_MENU_ROWS)}
              firstIndex={atFrom}
              total={atItems.length}
              selected={atSelected}
              onPick={(i) => completeAt(atItems[i])}
              onWheel={(by) => selectAt(Math.max(0, Math.min(atItems.length - 1, atSelected + by)))}
            />
          )}
          {/* No prompt glyph: the framed box the composer is drawn as (`index.css`) is its mark. */}
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
              setAtDirectory(null);
              setText(e.target.value);
              onDraftChange?.(e.target.value);
              setCaret(e.target.selectionStart);
              setAtIndex(0);
              setAtTop(0);
              setAtMoved(false);
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
              // The keys are going elsewhere: the chord's second key could not reach this box.
              endChord();
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
                    : // Only held messages: ↑ takes those back and Ctrl+Enter sends them, while a message
                      // already sent into the turn can be neither, so it leaves the running hint in place.
                      queueCount > 0
                      ? "↑ to edit queued messages, Ctrl+Enter to send them now"
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
        /* A dead session offers no route into INPUT at all, and says so instead of promising `i`.
           The textarea above is `disabled` once the session ends, so INPUT there is an empty mode:
           `autoFocus` cannot take focus, keys keep arriving at the panel root, and `resolveKey`'s
           "input" branch drops everything but `Escape` -- including `r`, which is the one key the
           lost/ended rows above genuinely promise. Not focusable and no `onFocus` here, so Tab
           cannot reach INPUT by the side door either; `resolveKey` refuses `i` on the same
           condition, and `App.tsx` forces BROWSE for a session that dies while INPUT is already
           active. The text names only `r`, the key that actually resolves in the mode the user is
           in. It names no key at all: the lost/ended/failed row just above
           already says "Press r…", and a failed tab read it twice. */
        <div className="composer-browse-hint">This session has ended.</div>
      ) : (
        // A focusable BROWSE stand-in lets a click, Tab and the panel's `i` route agree on INPUT.
        // It names the entry key (`browseKey`) at the right end, since arrivals land in BROWSE.
        // INPUT needs no reminder (the caret is its sign); an ended session offers `r` instead,
        // and a scratch round trip owns the draft, so typing there would be lost to nvim's buffer.
        // The same card shape in both modes keeps the bottom steady across `i`/`Esc`; the band names the mode.
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
      {/* Enter sends and Shift+Enter inserts a newline. Stop stays in ActivityLine for mouse users,
          gated on the provider's interrupt capability rather than its backend name. */}
    </div>
  );
}
