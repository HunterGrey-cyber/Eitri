// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { waitFor } from "@testing-library/react";

/** `App` throws on its first render, matching a chooser crash on a filter with no matches. */
vi.mock("./App", () => ({
  default: () => {
    throw new Error("render exploded");
  },
}));

const claimWindowError = (event: ErrorEvent) => event.preventDefault();
beforeEach(() => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  window.addEventListener("error", claimWindowError);
  document.body.innerHTML = '<div id="root"></div>';
});
afterEach(() => {
  window.removeEventListener("error", claimWindowError);
  vi.restoreAllMocks();
  document.body.innerHTML = "";
});

/** A render failure must not leave `#root` empty -- no chrome, every key dead, nothing logged -- until `prefix r`.
 *  The entry point is the one place a boundary must exist for that to be impossible, so it is pinned here
 *  rather than in `App`'s own tests, which never render `main.tsx`. */
it(
  "draws a crash notice, not an empty #root, when the panel itself throws",
  async () => {
    await import("./main");
    await waitFor(() => expect(document.querySelector("#root .panel-crashed")).not.toBeNull(), { timeout: 5000 });
    const notice = document.querySelector("#root .panel-crashed")!;
    expect(notice.getAttribute("role")).toBe("alert");
    expect(notice.textContent).toContain("The panel stopped drawing (render exploded)");
    expect(notice.textContent).toContain("prefix r");
    expect(notice.textContent).toContain("the conversation lives in Eitri");
  },
  // Importing the entry point pulls in React DOM and the whole stylesheet: slow on a busy machine.
  20_000,
);
