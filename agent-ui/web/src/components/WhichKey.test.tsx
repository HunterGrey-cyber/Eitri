// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { WhichKey } from "./WhichKey";

afterEach(cleanup);

describe("WhichKey", () => {
  it("renders each entry as a keycap plus its label, and the ? end", () => {
    const { container } = render(
      <WhichKey
        entries={[
          { key: "a", label: "allow" },
          { key: "d", label: "deny" },
        ]}
        focused={true}
        prefix={null}
      />,
    );
    expect(container.textContent).toContain("a");
    expect(container.textContent).toContain("allow");
    expect(container.textContent).toContain("d");
    expect(container.textContent).toContain("deny");
    // The strip's own keycaps: one per entry, plus the always-there `?`.
    const keycaps = Array.from(container.querySelectorAll(".keycap")).map((el) => el.textContent);
    expect(keycaps).toEqual(["a", "d", "?"]);
    expect(container.textContent).toContain("keys");
  });

  it("shows nothing but the ? end for an empty entry list", () => {
    const { container } = render(<WhichKey entries={[]} focused={true} prefix={null} />);
    expect(Array.from(container.querySelectorAll(".keycap")).map((el) => el.textContent)).toEqual(["?"]);
    expect(container.textContent).toContain("keys");
  });

  it("replaces the entries with the g prefix line, and still ends in ?", () => {
    const { container } = render(
      <WhichKey entries={[{ key: "a", label: "allow" }]} focused={true} prefix="g" />,
    );
    expect(container.querySelector(".which-key-entry")).toBeNull();
    expect(container.textContent).not.toContain("allow");
    const keycaps = Array.from(container.querySelectorAll(".keycap")).map((el) => el.textContent);
    expect(keycaps).toEqual(["g", "g", "?"]);
    expect(container.textContent).toContain("first row");
  });

  it("marks data-focused from the focused prop, both ways", () => {
    const { container, rerender } = render(<WhichKey entries={[]} focused={true} prefix={null} />);
    expect(container.querySelector(".which-key")!.getAttribute("data-focused")).toBe("true");
    rerender(<WhichKey entries={[]} focused={false} prefix={null} />);
    expect(container.querySelector(".which-key")!.getAttribute("data-focused")).toBe("false");
  });

  it("holds no control of its own", () => {
    const { container } = render(
      <WhichKey entries={[{ key: "a", label: "allow" }]} focused={true} prefix={null} />,
    );
    expect(container.querySelector("[data-nav-stop]")).toBeNull();
  });
});
