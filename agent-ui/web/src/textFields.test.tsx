// @vitest-environment jsdom
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Chooser } from "./components/Chooser";
import { Composer } from "./components/Composer";
import { HistorySearch } from "./components/HistorySearch";
import { PermissionCard } from "./components/PermissionCard";
import { SearchBar } from "./components/SearchBar";
import { TabBar } from "./components/TabBar";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const FIELDS = "textarea, input[type=text], input:not([type])";

/** Every text field under `root` must switch the platform's own text assistance off: macOS capitalises
 *  the first letter, corrects words and underlines them unless told not to. */
function expectNoTextAssist(root: ParentNode, expectedCount: number) {
  const fields = Array.from(root.querySelectorAll<HTMLElement>(FIELDS));
  expect(fields).toHaveLength(expectedCount);
  for (const field of fields) {
    expect(field.getAttribute("autocapitalize")).toBe("off");
    expect(field.getAttribute("autocorrect")).toBe("off");
    expect(field.getAttribute("spellcheck")).toBe("false");
  }
}

const TAB = {
  id: 1, number: 1, label: "1 fix-parser", name: null, state: "live" as const, mode: "auto" as const,
  marker: null, pending: 0, resumable: true, failure: null, title: null,
};

function renderChooser() {
  return render(
    <Chooser
      envelope={{
        open: [{ tab: 1, label: "1 fix-parser", marker: null, pending: 0, resumable: true }],
        records: [],
      }}
      tabs={[TAB]}
      active={1}
      defaultMode="auto"
      projectDir="/home/user/project"
      newTabChord="Ctrl+b c"
      focusRequest={0}
      onSwitch={vi.fn()}
      onResume={vi.fn()}
      onNewSession={vi.fn()}
      onCloseTab={vi.fn()}
      onRenameTab={vi.fn()}
      onCycleMode={vi.fn()}
      onCycleTabMode={vi.fn()}
      onLeave={vi.fn()}
      answerConfirm={vi.fn(() => false)}
    />,
  );
}

describe("text fields take no autocorrection", () => {
  it("the composer", () => {
    const { container } = render(
      <Composer
        disabled={false}
        sessionEnded={false}
        closing={false}
        restoredDraft={null}
        mode="input"
        onModeChange={vi.fn()}
        onSend={vi.fn()}
        running={false}
      />,
    );
    expectNoTextAssist(container, 1);
  });

  it("the history search line, opened from the composer", () => {
    const { container } = render(<HistorySearch history={["one"]} onAccept={vi.fn()} onCancel={vi.fn()} />);
    expectNoTextAssist(container, 1);
  });

  it("the permission card's reason box", () => {
    const { container } = render(
      <PermissionCard
        request={{ seq: 0, permissionId: "p", toolUseId: "t", toolName: "Bash", input: { command: "ls" } }}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    expectNoTextAssist(container, 1);
  });

  it("the search, command and review-comment line (one component)", () => {
    const { container } = render(<SearchBar query="" onChange={vi.fn()} onAccept={vi.fn()} onCancel={vi.fn()} />);
    expectNoTextAssist(container, 1);
  });

  it("the tab bar's rename field", () => {
    const { container } = render(
      <TabBar
        tabs={[{ ...TAB }]}
        active={1}
        renaming={{ tab: 1, initial: "docs" }}
        focusRequest={0}
        onSelect={vi.fn()}
        onRenameCommit={vi.fn()}
        onRenameCancel={vi.fn()}
      />,
    );
    expectNoTextAssist(container, 1);
  });

  it("the chooser's filter and its rename field", () => {
    const { container } = renderChooser();
    const root = container.querySelector<HTMLElement>(".chooser")!;
    fireEvent.keyDown(root, { key: "/" });
    expectNoTextAssist(container, 1);
    fireEvent.keyDown(container.querySelector(".chooser-filter")!, { key: "Escape" });
    fireEvent.keyDown(root, { key: "r", ctrlKey: true });
    expect(container.querySelector(".chooser-rename")).not.toBeNull();
    expectNoTextAssist(container, 1);
  });

  /** A field added later must opt in too: every `<input` or `<textarea` in a component is paired with
   *  one spread of the shared attributes. */
  it("every field in the source spreads the shared attributes", () => {
    const files: string[] = [];
    const walk = (dir: string) => {
      for (const name of readdirSync(dir)) {
        const path = join(dir, name);
        if (statSync(path).isDirectory()) walk(path);
        else if (path.endsWith(".tsx") && !path.endsWith(".test.tsx")) files.push(path);
      }
    };
    walk(join(__dirname));
    let fields = 0;
    let spreads = 0;
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      fields += (text.match(/^\s*<(input|textarea)\b/gm) ?? []).length;
      spreads += (text.match(/\{\.\.\.NO_TEXT_ASSIST\}/g) ?? []).length;
    }
    expect(fields).toBeGreaterThan(0);
    expect(spreads).toBe(fields);
  });
});
