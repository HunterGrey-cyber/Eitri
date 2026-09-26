// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { ContextLine, contextLineText } from "./ContextLine";

afterEach(cleanup);

it("says which file, and which lines when there is a selection (V1)", () => {
  expect(contextLineText({ file: "src/parser.rs", lines: [10, 20] })).toBe("⧉ src/parser.rs · L10–20");
  expect(contextLineText({ file: "src/parser.rs", lines: null })).toBe("⧉ src/parser.rs");
  expect(contextLineText({ file: "a.rs", lines: [7, 7] })).toBe("⧉ a.rs · L7");
  const { container } = render(<ContextLine context={null} />);
  expect(container.firstChild).toBeNull();
});
