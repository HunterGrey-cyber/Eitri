// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { waitFor } from "@testing-library/react";

/** Make `App` throw on its first render to exercise the entry point's error boundary. */
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

/** A render failure can leave `#root` empty and every key inactive until reload.
 *  Test the error boundary at the entry point, since `App` tests do not render `main.tsx`. */
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
