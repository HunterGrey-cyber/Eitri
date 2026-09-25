// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { KeymapOverlay } from "./KeymapOverlay";
import { BROWSE_KEYS, INPUT_KEYS } from "../keymap";
import type { KeyHelp } from "../keymap";

afterEach(cleanup);

const WINDOW: KeyHelp[] = [{ keys: "F11", what: "Fullscreen" }];
const PREFIX: KeyHelp[] = [
  { keys: "Ctrl+b f", what: "HINT: jump anywhere in the window" },
  { keys: "Ctrl+b %", what: "Then a module key (e / a / t): open it right of this one, or move it there" },
];

function overlay(onClose = () => {}) {
  return render(<KeymapOverlay onClose={onClose} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+b" />);
}

describe("KeymapOverlay", () => {
  it("renders the four groups, the last headed by the configured prefix", () => {
    const { container } = overlay();
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual(["This panel", "Typing", "Anywhere in the window", "After Ctrl+b"]);
  });

  it("follows a user's prefix", () => {
    const { container } = render(<KeymapOverlay onClose={() => {}} windowKeys={WINDOW} prefixKeys={PREFIX} prefixLabel="Ctrl+a" />);
    // No `.at(-1)`: tsconfig targets ES2020 (see `lastOfType` in `App.test.tsx`).
    const titles = Array.from(container.querySelectorAll("h2"));
    expect(titles[titles.length - 1].textContent).toBe("After Ctrl+a");
  });

  it("lists every row it was given and every local row, none dropped", () => {
    const { container } = overlay();
    const text = container.textContent!;
    for (const row of [...BROWSE_KEYS, ...INPUT_KEYS, ...WINDOW, ...PREFIX]) {
      expect(text).toContain(row.keys);
      expect(text).toContain(row.what);
    }
    expect(container.querySelectorAll("tr").length).toBe(
      BROWSE_KEYS.length + INPUT_KEYS.length + WINDOW.length + PREFIX.length,
    );
  });

  it("closes on a click on its own backdrop, not on a click inside a table", () => {
    const onClose = vi.fn();
    const { container } = overlay(onClose);
    fireEvent.click(container.querySelector("table")!);
    fireEvent.click(container.querySelector("td")!);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(container.querySelector(".keymap-overlay")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
