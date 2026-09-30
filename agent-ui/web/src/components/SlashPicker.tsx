import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import { pickerStep } from "../chooser";

export type SlashPickerKind = "model" | "effort";

type Props = {
  kind: SlashPickerKind;
  /** In the CLI's own reply order -- never sorted or otherwise reshaped, so this list stays
   *  whatever the CLI itself considers the natural order (`../slashPicker`'s own doc comment: never
   *  a hard-coded list). */
  options: string[];
  /** The option the reply named as current (`/model`'s "Current model: …"), or `null` when the
   *  reply named none (`/effort`'s bare reply never does -- see `../slashPicker`). Marked on its own
   *  row and used only to seed the starting cursor; picking a different row never "un-marks" it,
   *  since nothing here claims to know what Enter will end up changing until it is sent. */
  current: string | null;
  /** Fix round (Codex review finding, App.tsx's `takeKeys` gaining a picker branch): bumped by
   *  `App` the same way `Chooser`'s own `focusRequest` is, whenever the keys arrive somewhere that
   *  could otherwise steal them out from under an open picker (a GTK focus round trip, `arrive`, a
   *  HINT landing) -- re-focuses the root so `j`/`k`/`Enter`/`Escape` keep reaching this component's
   *  own `onKeyDown` rather than going nowhere. Mount still focuses too (whatever `focusRequest`
   *  starts at). */
  focusRequest: number;
  onChoose: (value: string) => void;
  onCancel: () => void;
};

/** Owner trial item 2 (2026-09-28): the picker a bare `/model`/`/effort` reply opens
 *  (`App.tsx`'s own wiring watches for the reply and parses it via `../slashPicker`). Copies the
 *  chooser's own keys (`./Chooser`, spec `docs/superpowers/specs/2026-09-26-panel-round2-design.md`
 *  §6): `j`/`k` -- and, since v1 picks Task 10 (R12), `↓`/`↑` and `Ctrl+n`/`Ctrl+p` -- move, `Enter`
 *  chooses, `Escape`/`q` cancel with nothing sent -- no new binding, the same table `prefix w` already
 *  uses. A click on a row chooses it directly, the same as a chooser row's own `onClick`. */
export function SlashPicker({ kind, options, current, focusRequest, onChoose, onCancel }: Props) {
  const startIndex = current === null ? -1 : options.indexOf(current);
  const [cursor, setCursor] = useState(startIndex === -1 ? 0 : startIndex);
  const rootRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    rootRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRequest]);
  // Fix round (Codex review finding, mirroring `Chooser`'s own identical effect): the highlighted
  // row can scroll out of view on a long `/model` list before `j`/`k` walks it back into the
  // viewport otherwise.
  useEffect(() => {
    rootRef.current?.querySelector(".slash-picker-row.current")?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    // R12: `↓` and `Ctrl+n` are `j`, `↑` and `Ctrl+p` are `k` (`pickerStep`), before the allow-list.
    const step = pickerStep(event);
    const key = step === null ? event.key : step > 0 ? "j" : "k";
    if (!["j", "k", "Enter", "Escape", "q"].includes(key)) return;
    event.preventDefault();
    event.stopPropagation();
    if (key === "j") setCursor((c) => Math.min(c + 1, options.length - 1));
    else if (key === "k") setCursor((c) => Math.max(c - 1, 0));
    else if (key === "Enter") {
      const choice = options[cursor];
      if (choice !== undefined) onChoose(choice);
    } else onCancel();
  }

  const title = kind === "model" ? "Model" : "Effort";
  const command = kind === "model" ? "/model" : "/effort";

  return (
    <div
      className="slash-picker"
      data-kind={kind}
      role="dialog"
      aria-label={`Choose ${title.toLowerCase()}`}
      tabIndex={-1}
      ref={rootRef}
      onKeyDown={onKeyDown}
    >
      <div className="slash-picker-header">
        <span className="slash-picker-title">{title}</span>
      </div>
      <ul className="slash-picker-list">
        {options.map((option, index) => (
          <li
            key={option}
            className={["slash-picker-row", index === cursor ? "current" : ""].filter(Boolean).join(" ")}
            onClick={() => onChoose(option)}
          >
            <span className="slash-picker-sign">{index === cursor ? "›" : ""}</span>
            <span className="slash-picker-name">{option}</span>
            {option === current && <span className="slash-picker-current-marker"> (current)</span>}
          </li>
        ))}
      </ul>
      <div className="slash-picker-hint">
        {kind === "effort" && <span className="slash-picker-scope">applies to this session only · </span>}
        {`enter ${command} ${options[cursor] ?? ""} · esc cancel`}
      </div>
    </div>
  );
}
