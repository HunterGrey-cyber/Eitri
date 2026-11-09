// @vitest-environment jsdom
import { expect, it } from "vitest";
import { render } from "@testing-library/react";
import { PathPick } from "./PathPick";

it("reveals invisible characters without replacing the candidate's path", () => {
  const refs = [{ path: "a/lib.rs", line: null }, { path: "a\u200b/lib.rs", line: 12 }];
  const { container, unmount } = render(<PathPick refs={refs} labels={["a", "s"]} typed="" />);
  expect(container.textContent).toBe("a a/lib.rs · s a⟨U+200B⟩/lib.rs:12");
  expect(refs[1].path).toBe("a\u200b/lib.rs");
  unmount();
});
