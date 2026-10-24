// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { EditDiff } from "./EditDiff";
import { editPreview } from "../diff";

afterEach(cleanup);

/** The line a break was taken from or added to says so under it, as `git diff` does, so the two lines that
 *  read the same do not look like a change of nothing. */
it("draws a note under a line that ends without a line break the other side has", () => {
  const preview = editPreview("Edit", { file_path: "/p/run.sh", old_string: "# safety check\n", new_string: "# safety check" })!;
  const { container } = render(<EditDiff preview={preview} />);
  const lines = Array.from(container.querySelectorAll(".permission-card-diff > div"), (el) => el.textContent);
  expect(lines).toEqual(["-# safety check", "+# safety check", " \\ no line break at the end"]);
  expect(container.querySelector(".diff-noeol")).not.toBeNull();
});

it("folds a large Write row while keeping the whole card preview", () => {
  const preview = editPreview("Write", { file_path: "/p/a", content: "x\n".repeat(401) })!;
  const { container } = render(<EditDiff preview={preview} maxLines={5} />);
  expect(container.querySelectorAll(".diff-added")).toHaveLength(5);
  expect(container.textContent).not.toContain("Too large to show here");
  expect(preview.diff).toHaveLength(401);
});

it("names notebook insertion and replacement and reveals their cell ids", () => {
  const insert = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", edit_mode: "insert", cell_id: "cell-A", cell_type: "markdown", new_source: "hi" })!;
  const replace = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", cell_id: "cell-B\u202E", cell_type: "code", new_source: "x" })!;
  const { container, rerender } = render(<EditDiff preview={insert} />);
  expect(container.querySelector(".permission-card-edit-note")!.textContent).toBe("Inserts a new markdown cell after cell cell-A.");
  rerender(<EditDiff preview={replace} />);
  expect(container.querySelector(".permission-card-edit-note")!.textContent).toBe("Replaces cell cell-B⟨U+202E⟩ as a code cell. The request does not say what is there now.");
  const unknown = editPreview("NotebookEdit", { notebook_path: "/p/a.ipynb", edit_mode: "rotate\u200B", cell_id: "cell-C\u202E" })!;
  rerender(<EditDiff preview={unknown} />);
  expect(container.querySelector(".permission-card-edit-note")!.textContent).toBe("Edit mode \"rotate⟨U+200B⟩\" on cell cell-C⟨U+202E⟩.");
});

it("shows counts, not every line, for an expanded row past the row limit, and every line on a card", () => {
  const preview = editPreview("Write", { file_path: "/p/a", content: "y\n".repeat(1000) })!;
  const row = render(<EditDiff preview={preview} />);
  expect(row.container.textContent).toContain("Too large to show here (1000 lines)");
  expect(row.container.querySelectorAll(".diff-line")).toHaveLength(0);
  row.unmount();
  const card = render(<EditDiff preview={preview} whole />);
  expect(card.container.querySelectorAll(".diff-added")).toHaveLength(1000);
  expect(card.container.textContent).not.toContain("Too large to show here");
});
