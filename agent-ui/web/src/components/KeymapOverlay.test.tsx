// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { KeymapOverlay } from "./KeymapOverlay";
import { BROWSE_KEYS, INPUT_KEYS, PREFIX_KEYS, WINDOW_KEYS } from "../keymap";

afterEach(cleanup);

describe("KeymapOverlay", () => {
  it("renders the four groups with their exact titles, in order (spec §3.2)", () => {
    const { container } = render(<KeymapOverlay onClose={() => {}} />);
    const titles = Array.from(container.querySelectorAll("h2")).map((h) => h.textContent);
    expect(titles).toEqual(["This panel", "Typing", "Anywhere in the window", "After Ctrl+a"]);
  });

  it("lists every row of every table, with no row silently dropped", () => {
    const { container } = render(<KeymapOverlay onClose={() => {}} />);
    const text = container.textContent!;
    for (const table of [BROWSE_KEYS, INPUT_KEYS, WINDOW_KEYS, PREFIX_KEYS]) {
      for (const row of table) {
        expect(text).toContain(row.keys);
        expect(text).toContain(row.what);
      }
    }
    // A row count check as well as a text-substring one: a row whose `what` happens to be a
    // substring of another row's text would pass the loop above even if it were never rendered.
    expect(container.querySelectorAll("tr").length).toBe(
      BROWSE_KEYS.length + INPUT_KEYS.length + WINDOW_KEYS.length + PREFIX_KEYS.length,
    );
  });

  it("closes on a click on its own backdrop, not on a click inside a table (spec §3.1)", () => {
    const onClose = vi.fn();
    const { container } = render(<KeymapOverlay onClose={onClose} />);
    fireEvent.click(container.querySelector("table")!);
    fireEvent.click(container.querySelector("td")!);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(container.querySelector(".keymap-overlay")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
