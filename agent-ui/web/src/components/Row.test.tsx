// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Row } from "./Row";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

describe("Row", () => {
  /* THE PROPERTY THIS COMPONENT EXISTS FOR. The two-cell shape was written six times -- once here
     and five times by hand (`App.tsx`'s two session-ended rows, `TerminalHandoff`'s command card,
     `ModeSelector`'s two choice rows -- `ModeSelector` itself is gone since session tabs Task 9,
     replaced by `EmptyTab`'s own choice rows) -- and each hand-written copy spelled its glyph out TWICE,
     once in `data-sign` and once in the visible `.row-sign` span. Tests read `data-sign`; users
     read the span; nothing compared them, so editing one and not the other drifted straight past a
     green suite. With one `sign` argument feeding both, that is not a thing a test has to catch --
     it is unrepresentable. This asserts it anyway for the shape, because the derivation is what a
     future edit could undo. */
  it("derives data-sign and the visible glyph from the same argument, on both shapes", () => {
    const { container } = render(
      <>
        <Row kind="error" sign="✗">
          body
        </Row>
        <Row as="button" kind="choice" sign="↺">
          choice
        </Row>
      </>,
    );
    const rows = Array.from(container.querySelectorAll(".row"));
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(row.getAttribute("data-sign")).toBe(row.querySelector(".row-sign")!.textContent);
    }
    expect(rows.map((r) => r.getAttribute("data-sign"))).toEqual(["✗", "↺"]);
  });

  /* A `<button>`'s content model is phrasing content, so block-level children in one are invalid
     HTML -- which is why the shape is an element prop rather than two components that could drift. */
  it("uses span cells inside a button and div cells outside one", () => {
    const { container } = render(
      <>
        <Row kind="error" sign="✗">
          body
        </Row>
        <Row as="button" kind="choice" sign="↺">
          choice
        </Row>
      </>,
    );
    expect(container.querySelector(".row-error .row-sign")!.tagName).toBe("DIV");
    expect(container.querySelector(".row-choice .row-sign")!.tagName).toBe("SPAN");
    expect(container.querySelector(".row-choice")!.tagName).toBe("BUTTON");
    // `type="button"`, always: a bare button inside a form submits it.
    expect(container.querySelector(".row-choice")!.getAttribute("type")).toBe("button");
  });

  it("puts extra classes on the row and on the body cell, without touching the grid's own", () => {
    const { container } = render(
      <Row kind="handoff" sign="→" className="extra" bodyClassName="handoff-card" current>
        body
      </Row>,
    );
    const row = container.querySelector(".row")!;
    expect(row.className.split(" ")).toEqual(["row", "row-handoff", "row-current", "extra"]);
    expect(row.querySelector(".row-body")!.className).toBe("row-body handoff-card");
    // The cursor is announced, not colour-only.
    expect(row.getAttribute("aria-current")).toBe("true");
  });

  /* Spec §10.2 (P11): a `problem` (a `classify()` result) draws a headline and remedy ABOVE the
   *  row's own children, which stay exactly as they were -- "never hide the evidence". An error row
   *  with nothing recognised (`problem` omitted, the common case) draws neither and is unchanged
   *  from before this prop existed. */
  it("draws a problem's headline and remedy above the row's own raw text, when given one", () => {
    const { container } = render(
      <Row kind="error" sign="✗" problem={{ headline: "Claude Code (claude) was not found.", remedy: "Install it." }}>
        <pre>the raw failure text</pre>
      </Row>,
    );
    const body = container.querySelector(".row-body")!;
    const problemBlock = body.querySelector(".row-problem")!;
    expect(problemBlock.querySelector("strong")!.textContent).toBe("Claude Code (claude) was not found.");
    expect(problemBlock.querySelector(".row-problem-remedy")!.textContent).toBe("Install it.");
    // The raw text is still there, unchanged, and still comes after the problem block.
    expect(body.querySelector("pre")!.textContent).toBe("the raw failure text");
    const children = Array.from(body.children);
    expect(children.indexOf(problemBlock)).toBeLessThan(children.indexOf(body.querySelector("pre")!));
  });

  it("draws nothing extra when no problem was recognised (problem omitted)", () => {
    const { container } = render(
      <Row kind="error" sign="✗">
        <pre>an unrecognised failure</pre>
      </Row>,
    );
    expect(container.querySelector(".row-problem")).toBeNull();
    expect(container.querySelector("pre")!.textContent).toBe("an unrecognised failure");
  });

  /* The invariant, enforced over the SOURCE rather than over one render: no file may write a row's
     glyph by hand again. `data-sign` and `.row-sign` are the two readings that must agree, and the
     only way they can disagree is a second place that writes them -- so the guard is "there is no
     second place", checked across every component. Without this, the five hand-written copies
     could come back one at a time and every render test would still pass. */
  it("is the only module that writes a row's sign", () => {
    const sources = import.meta.glob("../**/*.tsx", { query: "?raw", import: "default", eager: true }) as Record<
      string,
      string
    >;
    const product = Object.entries(sources).filter(([path]) => !path.includes(".test."));
    // A glob that silently matched nothing would make the assertion below vacuous. Every component
    // that renders a row must be in here: App and the four that used to write one by hand.
    for (const name of ["App.tsx", "MessageList.tsx", "EmptyTab.tsx", "TerminalHandoff.tsx", "Row.tsx"]) {
      expect(product.some(([path]) => path.endsWith(`/${name}`)), `${name} not covered`).toBe(true);
    }
    const writers = product.filter(([, source]) => /data-sign=|"row-sign"/.test(source)).map(([path]) => path);
    expect(writers).toEqual(["./Row.tsx"]);
  });
});
