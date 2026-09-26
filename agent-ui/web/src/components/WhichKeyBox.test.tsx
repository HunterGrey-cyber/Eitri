// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { WhichKeyBox } from "./WhichKeyBox";
import type { BoxEntry } from "../leader";

afterEach(cleanup);

const ENTRIES: BoxEntry[] = [
  { key: "m", label: "mode", group: false, disabled: false },
  { key: "b", label: "+tab", group: true, disabled: false },
  { key: "f", label: "+new", group: false, disabled: true },
];

describe("WhichKeyBox", () => {
  it("draws the title on the border, an arrow between each key and its label, and the foot", () => {
    const { container } = render(<WhichKeyBox title="Space" entries={ENTRIES} onPick={() => {}} />);
    expect(container.querySelector(".wk-title")!.textContent).toBe("Space");
    expect(Array.from(container.querySelectorAll(".wk-sep")).map((el) => el.textContent)).toEqual(["➜", "➜", "➜"]);
    expect(container.querySelector(".wk-foot")!.textContent).toBe("esc close · bs back");
  });

  it("marks a group's label wk-group and a disabled entry's label wk-disabled", () => {
    const { container } = render(<WhichKeyBox title="Space" entries={ENTRIES} onPick={() => {}} />);
    const rows = Array.from(container.querySelectorAll(".wk-entry"));
    expect(rows[1].querySelector(".wk-group")?.textContent).toBe("+tab");
    expect(rows[2].querySelector(".wk-disabled")?.textContent).toBe("+new");
    // The leaf's own label carries neither class.
    expect(rows[0].querySelector(".wk-group")).toBeNull();
    expect(rows[0].querySelector(".wk-disabled")).toBeNull();
  });

  it("calls onPick with the entry's key on a click", () => {
    const onPick = vi.fn();
    const { container } = render(<WhichKeyBox title="Space" entries={ENTRIES} onPick={onPick} />);
    fireEvent.click(container.querySelectorAll(".wk-entry")[1]);
    expect(onPick).toHaveBeenCalledTimes(1);
    expect(onPick).toHaveBeenCalledWith("b");
  });

  it("renders nothing but the title and the foot for an empty node", () => {
    const { container } = render(<WhichKeyBox title="g" entries={[]} onPick={() => {}} />);
    expect(container.querySelectorAll(".wk-entry").length).toBe(0);
    expect(container.querySelector(".wk-title")!.textContent).toBe("g");
    expect(container.querySelector(".wk-foot")!.textContent).toBe("esc close · bs back");
  });
});
