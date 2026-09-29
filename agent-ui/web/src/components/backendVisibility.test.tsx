// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Chooser } from "./Chooser";
import { Dashboard } from "./Dashboard";
import { DetailPopover } from "./DetailPopover";
import type { BackendKind, ChooserEnvelope, Hello, TabInfo } from "../types";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const BACKENDS: BackendKind[] = ["legacy", "sidecar"];

/** v1 trial item 1 (owner: "⏵⏵ auto · sidecar，这个东西应该出现在all session的选择上吗，是参考了别人的
 *  设计，还是我们自己的设计失误" -- our own design mistake). Decision 6 of the round-2 spec already said
 *  "model/backend live in `prefix i`"; this guard pins that the chooser and the empty tab's dashboard
 *  never draw the backend's own name, on either backend, and that `DetailPopover` (`prefix i` /
 *  `<leader>i`) is the one place that still does -- so a future row cannot silently reintroduce the
 *  word without a test failing here first. */
describe("the backend name is drawn only in the detail popover, never the chooser or the dashboard", () => {
  it.each(BACKENDS)("does not appear in a rendered chooser (%s)", (backend) => {
    const tabs: TabInfo[] = [
      {
        id: 1,
        number: 1,
        label: "1 fix-parser",
        name: null,
        state: "live",
        mode: "bypass",
        marker: null,
        pending: 0,
        resumable: true,
        failure: null,
        title: "write the docs",
      },
    ];
    const envelope: ChooserEnvelope = {
      open: [{ tab: 1, label: "1 fix-parser", marker: null, pending: 0, resumable: true }],
      records: [
        { providerSessionId: "aaaa1111bbbb", name: null, title: null, createdAt: "1", updatedAt: "2", heldElsewhere: false },
      ],
    };
    const { container } = render(
      <Chooser
        envelope={envelope}
        tabs={tabs}
        active={1}
        defaultMode="auto"
        projectDir="/home/user/src/neovibe"
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
        answerConfirm={() => false}
      />,
    );
    expect(container.textContent).not.toContain(backend);
  });

  it.each(BACKENDS)("does not appear in a rendered empty-tab dashboard (%s)", (backend) => {
    const hello: Hello = {
      backend,
      projectDir: "/home/user/src/neovibe",
      permissionModes: ["auto", "bypass"],
      resumableSessions: [],
      expectedVerdandiRevision: "28a5e4c",
      account: "work",
    };
    const { container } = render(<Dashboard hello={hello} mode="auto" cursor={0} narrow={false} onItem={vi.fn()} prefix="Ctrl+b" />);
    expect(container.textContent).not.toContain(backend);
  });

  it("DetailPopover (prefix i / <leader>i) is the one place it does appear", () => {
    const { container } = render(
      <DetailPopover rows={[{ label: "backend", value: "sidecar" }]} current={0} onClose={vi.fn()} />,
    );
    expect(container.textContent).toContain("sidecar");
  });
});
