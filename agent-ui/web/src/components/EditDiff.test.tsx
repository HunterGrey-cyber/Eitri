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
