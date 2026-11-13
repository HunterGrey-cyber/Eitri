// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { AttachmentChips } from "./AttachmentChips";
import type { DraftImage } from "../types";

afterEach(cleanup);

const READY: DraftImage = {
  attachmentId: "a".repeat(32),
  name: "screenshot.png",
  state: "ready",
  mediaType: "image/png",
  width: 1280,
  height: 720,
  bytes: 319_488,
  thumbnail: "data:image/png;base64,iVBORw0KGgo=",
};

describe("AttachmentChips", () => {
  it("draws a ready image's thumbnail, name and what it is", () => {
    const { container } = render(<AttachmentChips items={[READY]} onRemove={() => {}} />);
    expect(container.querySelector("img")?.getAttribute("src")).toBe(READY.thumbnail);
    expect(container.textContent).toContain("screenshot.png");
    expect(container.textContent).toContain("PNG · 1280×720 · 312 KB");
  });

  it("cuts a long name to 40 characters", () => {
    const name = `${"n".repeat(50)}.png`;
    const { container } = render(<AttachmentChips items={[{ ...READY, name }]} onRemove={() => {}} />);
    expect(container.querySelector(".attachment-name")?.textContent).toBe(`${"n".repeat(40)}…`);
  });

  it("a name with hidden characters shows them", () => {
    const { container } = render(
      <AttachmentChips items={[{ ...READY, name: "cat‮gnp.exe", state: "importing" }]} onRemove={() => {}} />,
    );
    const name = container.querySelector(".attachment-name")!;
    expect(name.textContent).toBe("cat⟨U+202E⟩gnp.exe");
    expect(name.innerHTML).not.toContain("‮");
  });

  it("a failed chip shows the host's sentence and no thumbnail", () => {
    const failed: DraftImage = { attachmentId: "b".repeat(32), name: "x.bmp", state: "failed", reason: "this is not a PNG, JPEG, GIF or WebP image" };
    const { container, getByRole } = render(<AttachmentChips items={[failed]} onRemove={() => {}} />);
    expect(getByRole("alert").textContent).toBe("this is not a PNG, JPEG, GIF or WebP image");
    expect(container.querySelector("img")).toBeNull();
  });

  it("an importing chip says so, and so does an image still being read", () => {
    const { container } = render(
      <AttachmentChips items={[{ attachmentId: "c".repeat(32), name: "big.png", state: "importing" }]} pending={1} onRemove={() => {}} />,
    );
    expect(container.querySelectorAll(".attachment-chip-importing")).toHaveLength(2);
    expect(container.textContent?.match(/importing…/g)).toHaveLength(2);
  });

  it("x, Delete or Backspace on a focused chip, or its ×, removes it", () => {
    const onRemove = vi.fn();
    const second = { ...READY, attachmentId: "d".repeat(32) };
    const { container, getAllByRole } = render(<AttachmentChips items={[READY, second]} onRemove={onRemove} />);
    const chips = container.querySelectorAll<HTMLElement>(".attachment-chip");
    fireEvent.keyDown(chips[1], { key: "x" });
    fireEvent.keyDown(chips[0], { key: "Delete" });
    fireEvent.keyDown(chips[0], { key: "x", ctrlKey: true });
    fireEvent.click(getAllByRole("button")[1]);
    expect(onRemove.mock.calls).toEqual([[second.attachmentId], [READY.attachmentId], [second.attachmentId]]);
  });

  it("draws nothing without images", () => {
    const { container } = render(<AttachmentChips items={[]} onRemove={() => {}} />);
    expect(container.innerHTML).toBe("");
  });

  it("h and l move along the chips, l past the last and i go back to the box, x keeps the keys on the chips", () => {
    const onRemove = vi.fn();
    const onLeave = vi.fn();
    const second = { ...READY, attachmentId: "e".repeat(32) };
    const { container } = render(<AttachmentChips items={[READY, second]} onRemove={onRemove} onLeave={onLeave} />);
    const chips = container.querySelectorAll<HTMLElement>(".attachment-chip");
    chips[0].focus();
    fireEvent.keyDown(chips[0], { key: "l" });
    expect(document.activeElement).toBe(chips[1]);
    fireEvent.keyDown(chips[1], { key: "ArrowLeft" });
    expect(document.activeElement).toBe(chips[0]);
    fireEvent.keyDown(chips[0], { key: "x" });
    expect(onRemove).toHaveBeenCalledWith(READY.attachmentId);
    expect(document.activeElement).toBe(chips[1]);
    fireEvent.keyDown(chips[1], { key: "l" });
    fireEvent.keyDown(chips[1], { key: "i" });
    expect(onLeave).toHaveBeenCalledTimes(2);
  });
});
