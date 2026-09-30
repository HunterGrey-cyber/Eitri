// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { PanelErrorBoundary } from "./PanelErrorBoundary";

/** React reports a caught render error twice over: on `console.error`, and as an uncaught error on
 *  `window` (jsdom re-dispatches it). Neither is the point of these tests, and vitest would count the
 *  second as an unhandled error and fail the run, so both are silenced -- the second by claiming it. */
let errorSpy: ReturnType<typeof vi.spyOn>;
const claimWindowError = (event: ErrorEvent) => event.preventDefault();
beforeEach(() => {
  errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
  window.addEventListener("error", claimWindowError);
});
afterEach(() => {
  window.removeEventListener("error", claimWindowError);
  errorSpy.mockRestore();
  cleanup();
});

function Boom({ message = "boom" }: { message?: string }): never {
  throw new Error(message);
}

/** K03 (kbux 2026-09-29): one throw during render unmounted the whole React tree, so `#root` went
 *  empty and every key went dead. A boundary is the only thing React keeps a tree alive with. */
describe("PanelErrorBoundary", () => {
  it("draws its children when nothing throws, and never calls onError", () => {
    const onError = vi.fn();
    const { container } = render(
      <PanelErrorBoundary name="chooser" onError={onError} fallback={() => <p>fallback</p>}>
        <p>fine</p>
      </PanelErrorBoundary>,
    );
    expect(container.textContent).toBe("fine");
    expect(onError).not.toHaveBeenCalled();
  });

  it("a throwing child calls onError once and renders fallback(error)", () => {
    const onError = vi.fn();
    const fallback = vi.fn((e: Error) => <p role="alert">failed: {e.message}</p>);
    const { container } = render(
      <PanelErrorBoundary name="chooser" onError={onError} fallback={fallback}>
        <Boom message="kaput" />
      </PanelErrorBoundary>,
    );
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ message: "kaput" }));
    expect(fallback).toHaveBeenCalledWith(expect.objectContaining({ message: "kaput" }));
    expect(container.textContent).toBe("failed: kaput");
    expect(container.querySelector("[role=alert]")).not.toBeNull();
  });

  it("with no fallback it renders nothing, and the siblings outside it stay", () => {
    const { container } = render(
      <div>
        <span>before</span>
        <PanelErrorBoundary name="picker">
          <Boom />
        </PanelErrorBoundary>
        <span>after</span>
      </div>,
    );
    expect(container.textContent).toBe("beforeafter");
  });

  it("logs which boundary caught what, with the component stack, since a throw used to leave no trace at all", () => {
    render(
      <PanelErrorBoundary name="chooser">
        <Boom message="kaput" />
      </PanelErrorBoundary>,
    );
    const ours = errorSpy.mock.calls.find((call) => call[0] === "[agent-ui] chooser failed:");
    expect(ours, "one console.error naming the boundary").toBeDefined();
    expect(ours![1]).toEqual(expect.objectContaining({ message: "kaput" }));
    expect(String(ours![2])).toContain("Boom");
  });

  it("hands fallback and onError an Error even when something else was thrown", () => {
    function ThrowsAString(): never {
      throw "not an Error";
    }
    const onError = vi.fn();
    const { container } = render(
      <PanelErrorBoundary name="panel" onError={onError} fallback={(e) => <p>{e.message}</p>}>
        <ThrowsAString />
      </PanelErrorBoundary>,
    );
    expect(onError).toHaveBeenCalledWith(expect.any(Error));
    expect(container.textContent).toBe("not an Error");
  });

  it("an error in one boundary leaves a sibling boundary's children drawn", () => {
    const { container } = render(
      <div>
        <PanelErrorBoundary name="a">
          <Boom />
        </PanelErrorBoundary>
        <PanelErrorBoundary name="b">
          <p>still here</p>
        </PanelErrorBoundary>
      </div>,
    );
    expect(container.textContent).toBe("still here");
  });

  it("stays on its fallback until it is unmounted, and a new one starts fresh", () => {
    const first = render(
      <PanelErrorBoundary name="chooser" fallback={() => <p>fallback</p>}>
        <Boom />
      </PanelErrorBoundary>,
    );
    first.rerender(
      <PanelErrorBoundary name="chooser" fallback={() => <p>fallback</p>}>
        <p>recovered?</p>
      </PanelErrorBoundary>,
    );
    expect(first.container.textContent, "the same boundary does not retry by itself").toBe("fallback");
    first.unmount();
    const second = render(
      <PanelErrorBoundary name="chooser" fallback={() => <p>fallback</p>}>
        <p>fresh</p>
      </PanelErrorBoundary>,
    );
    expect(second.container.textContent).toBe("fresh");
  });
});
