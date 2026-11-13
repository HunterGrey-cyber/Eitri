// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { createHash, webcrypto } from "node:crypto";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { SEND_NOW_WITH_IMAGES } from "./components/Composer";

/* Composer images in the real page: paste imports them through the host in bounded chunks, chips show what
   the host says of them, and they go, wait, come back and are refused together with their text. */

type Posted = Record<string, unknown> & { type: string; request_id: string };
let posted: Posted[];
beforeEach(() => {
  posted = [];
  vi.stubGlobal("crypto", webcrypto);
  Element.prototype.scrollIntoView = vi.fn();
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  delete (window as unknown as { webkit?: unknown }).webkit;
});

function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}

const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null };
const THUMB = "data:image/png;base64,iVBORw0KGgo=";

function ready(id: string, name = "shot.png") {
  return { attachmentId: id, name, state: "ready", mediaType: "image/png", width: 4, height: 2, bytes: 120, thumbnail: THUMB };
}

function mount(options: { imageInput?: boolean; why?: string; running?: boolean; images?: unknown[] } = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, defaultMode: "auto", tabs: [TAB] });
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 1,
    state: { ...initialState(), status: { kind: "running" }, activeTurnId: options.running ? "t1" : null, nextSeq: 1 },
  });
  dispatch({
    kind: "draft_attachments", tab: 1, generation: 0, imageInput: options.imageInput ?? true,
    ...(options.why !== undefined ? { why: options.why } : {}), attachments: options.images ?? [],
  });
  const { container } = rendered;
  // Into INPUT, as a click on the box does.
  fireEvent.focus(container.querySelector(".composer-browse-hint")!);
  const textarea = () => container.querySelector<HTMLTextAreaElement>(".composer textarea")!;
  const chips = () => Array.from(container.querySelectorAll<HTMLElement>(".attachment-chip"));
  return { ...rendered, textarea, chips };
}

function sent(type: string): Posted[] {
  return posted.filter((m) => m.type === type);
}

async function next(type: string, count = 1): Promise<Posted> {
  await waitFor(() => expect(sent(type).length).toBeGreaterThanOrEqual(count));
  return sent(type)[count - 1];
}

function answer(message: Posted, extra: Record<string, unknown> = {}) {
  dispatch({ kind: "command_result", requestId: message.request_id, ok: true, ...extra });
}

function png(size: number): File {
  const bytes = new Uint8Array(size);
  for (let i = 0; i < size; i += 1) bytes[i] = (i * 7) % 251;
  return new File([bytes], "shot.png", { type: "image/png" });
}

function imageItem(file: File, getAsFile = () => file) {
  return { kind: "file", type: file.type, getAsFile };
}

function type(textarea: HTMLTextAreaElement, text: string) {
  fireEvent.change(textarea, { target: { value: text, selectionStart: text.length } });
}

it("pasting an image imports it and does not insert text", async () => {
  const { textarea } = mount();
  const file = png(100);
  const inserted = fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(file)] } });
  expect(inserted).toBe(false);
  const begin = await next("attach_begin");
  expect(begin).toMatchObject({ tab: 1, generation: 0, byte_length: 100, name: "shot.png" });
  expect(textarea().value).toBe("");
});

it("pasting text still inserts text", () => {
  const { textarea } = mount();
  const inserted = fireEvent.paste(textarea(), {
    clipboardData: { types: ["text/plain"], items: [{ kind: "string", type: "text/plain", getAsFile: () => null }] },
  });
  expect(inserted).toBe(true);
  expect(sent("attach_begin")).toEqual([]);
});

it("chunks are sequential, at most 256 KiB, one outstanding", async () => {
  const { textarea } = mount();
  const size = 600 * 1024;
  const file = png(size);
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(file)] } });
  answer(await next("attach_begin"), { attachmentId: "x".repeat(32) });
  const lengths: number[] = [];
  for (let n = 1; n <= 3; n += 1) {
    const chunk = await next("attach_chunk", n);
    // Nothing more goes until this one is answered.
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(sent("attach_chunk")).toHaveLength(n);
    expect(chunk.offset).toBe((n - 1) * 256 * 1024);
    lengths.push(Buffer.from(chunk.data_b64 as string, "base64").length);
    answer(chunk);
  }
  expect(lengths).toEqual([262144, 262144, size - 524288]);
  const end = await next("attach_end");
  const bytes = new Uint8Array(await file.arrayBuffer());
  expect(end.sha256).toBe(createHash("sha256").update(bytes).digest("hex"));
  expect(end.attachment_id).toBe("x".repeat(32));
});

it("pasting two images imports both in order", async () => {
  const { textarea } = mount();
  const first = new File([new Uint8Array(10)], "first.png", { type: "image/png" });
  const second = new File([new Uint8Array(20)], "second.jpg", { type: "image/jpeg" });
  const dropped = fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(first), imageItem(second)] } });
  expect(dropped).toBe(false);
  const begin = await next("attach_begin");
  expect(begin.name).toBe("first.png");
  answer(begin, { attachmentId: "1".repeat(32) });
  answer(await next("attach_chunk"));
  answer(await next("attach_end"));
  const later = await next("attach_begin", 2);
  expect(later.name).toBe("second.jpg");
});

it("a dropped file's link is kept out of the box", () => {
  const { container, textarea } = mount();
  const dropped = fireEvent.drop(container.querySelector(".composer")!, {
    dataTransfer: { types: ["text/uri-list", "text/html"], files: [], getData: () => "<a href='file:///home/u/x.png'>/home/u/x.png</a>" },
  });
  expect(dropped).toBe(false);
  expect(textarea().value).toBe("");
  expect(sent("attach_begin")).toEqual([]);
});

it("an importing chip blocks Send", () => {
  const { textarea, chips, container } = mount();
  dispatch({ kind: "attachment_state", tab: 1, attachmentId: "2".repeat(32), name: "big.png", state: "importing" });
  expect(chips()).toHaveLength(1);
  type(textarea(), "look");
  fireEvent.keyDown(textarea(), { key: "Enter" });
  expect(sent("send_message")).toEqual([]);
  expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("an image is still being prepared");
  expect(textarea().value).toBe("look");
});

it("a failed chip shows the host's sentence", () => {
  const { container } = mount();
  dispatch({ kind: "attachment_state", tab: 1, attachmentId: "3".repeat(32), name: "x.bmp", state: "failed", reason: "this is not a PNG, JPEG, GIF or WebP image" });
  expect(container.querySelector(".attachment-reason")?.textContent).toBe("this is not a PNG, JPEG, GIF or WebP image");
});

it("x and Backspace at caret 0 remove chips", async () => {
  const one = "4".repeat(32);
  const two = "5".repeat(32);
  const { chips, textarea } = mount({ images: [ready(one, "one.png"), ready(two, "two.png")] });
  fireEvent.keyDown(chips()[0], { key: "x" });
  const removed = await next("attach_remove");
  expect(removed.attachment_id).toBe(one);
  expect(chips().map((c) => c.dataset.attachmentId)).toEqual([two]);
  answer(removed);
  fireEvent.keyDown(textarea(), { key: "Backspace" });
  expect((await next("attach_remove", 2)).attachment_id).toBe(two);
  expect(chips()).toHaveLength(0);
});

it("a refused send restores text and chips", async () => {
  const id = "6".repeat(32);
  const { textarea, chips } = mount({ images: [ready(id)] });
  type(textarea(), "what is this");
  fireEvent.keyDown(textarea(), { key: "Enter" });
  const send = sent("send_message")[0];
  expect(send).toMatchObject({ text: "what is this", attachments: [id] });
  expect(chips()).toHaveLength(0);
  dispatch({ kind: "command_result", requestId: send.request_id, ok: false, error: "an image is too large to send" });
  expect(chips().map((c) => c.dataset.attachmentId)).toEqual([id]);
  expect(textarea().value).toBe("what is this");
});

it("an image alone can be sent", () => {
  const id = "7".repeat(32);
  const { textarea } = mount({ images: [ready(id)] });
  fireEvent.keyDown(textarea(), { key: "Enter" });
  expect(sent("send_message")[0]).toMatchObject({ text: "", attachments: [id] });
});

it("Ctrl+Enter is not offered while images wait", () => {
  const { textarea, container } = mount({ running: true, images: [ready("8".repeat(32))] });
  type(textarea(), "now");
  fireEvent.keyDown(textarea(), { key: "Enter", ctrlKey: true });
  expect(sent("send_now")).toEqual([]);
  expect(container.querySelector(".composer-slash-flash")?.textContent).toBe(SEND_NOW_WITH_IMAGES);
});

it("take-back restores chips with the text", () => {
  const id = "9".repeat(32);
  const { textarea, chips, container } = mount({ running: true, images: [ready(id)] });
  type(textarea(), "and this");
  fireEvent.keyDown(textarea(), { key: "Enter" });
  const queued = sent("queue_message")[0];
  expect(queued).toMatchObject({ text: "and this", attachments: [id] });
  answer(queued);
  dispatch({ kind: "draft_attachments", tab: 1, generation: 1, imageInput: true, attachments: [] });
  dispatch({ kind: "queue", tab: 1, items: [{ text: "and this", queuedAt: 1, steered: false, images: 1 }], error: null });
  expect(chips()).toHaveLength(0);
  expect(container.querySelector(".queue-lines")?.textContent).toContain("[1 image]");
  fireEvent.keyDown(textarea(), { key: "ArrowUp" });
  expect(sent("take_back_queue")).toHaveLength(1);
  dispatch({ kind: "queue_taken", tab: 1, texts: ["and this"], attachments: [[id]] });
  dispatch({ kind: "draft_attachments", tab: 1, generation: 1, imageInput: true, attachments: [ready(id)] });
  dispatch({ kind: "queue", tab: 1, items: [], error: null });
  expect(textarea().value).toBe("and this");
  expect(chips().map((c) => c.dataset.attachmentId)).toEqual([id]);
});

it("reload restores chips from the snapshot", () => {
  const { chips, container } = mount({ images: [ready("a".repeat(32), "kept.png")] });
  expect(chips()).toHaveLength(1);
  expect(container.querySelector(".attachment-thumb")?.getAttribute("src")).toBe(THUMB);
  expect(container.querySelector(".attachment-name")?.textContent).toBe("kept.png");
});

it("a thumbnail that is not the host's PNG data URI is not drawn", () => {
  const { chips } = mount();
  dispatch({ kind: "attachment_state", tab: 1, ...ready("b".repeat(32)), thumbnail: "https://example.com/x.png" });
  expect(chips()).toHaveLength(0);
});

it("without imageInput a paste reads nothing and says why", () => {
  const why = "this tab cannot take images: codex takes text only here";
  const { textarea, container } = mount({ imageInput: false, why });
  const getAsFile = vi.fn(() => png(10));
  const inserted = fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [{ kind: "file", type: "image/png", getAsFile }] } });
  expect(inserted).toBe(false);
  expect(getAsFile).not.toHaveBeenCalled();
  expect(sent("attach_begin")).toEqual([]);
  expect(container.querySelector(".composer-slash-flash")?.textContent).toBe(why);
});

it("a begin the host refuses is said, and leaves no chip", async () => {
  const { textarea, chips, container } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
  const begin = await next("attach_begin");
  expect(container.querySelectorAll(".attachment-chip-importing")).toHaveLength(1);
  dispatch({ kind: "command_result", requestId: begin.request_id, ok: false, error: "this window has reached its 64 MB image limit" });
  await waitFor(() => expect(chips()).toHaveLength(0));
  await waitFor(() =>
    expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("this window has reached its 64 MB image limit"),
  );
});

it("a clipboard with an image and plain text pastes the text", () => {
  const { textarea } = mount();
  const inserted = fireEvent.paste(textarea(), {
    clipboardData: {
      types: ["Files", "text/plain"],
      items: [imageItem(png(10)), { kind: "string", type: "text/plain", getAsFile: () => null }],
      getData: (type: string) => (type === "text/plain" ? "A1\tB1" : ""),
    },
  });
  expect(inserted).toBe(true);
  expect(sent("attach_begin")).toEqual([]);
});

it("Ctrl+Enter with an empty box sends what is queued and leaves the images", () => {
  const id = "c".repeat(32);
  const { textarea, chips } = mount({ running: true, images: [ready(id)] });
  fireEvent.keyDown(textarea(), { key: "Enter", ctrlKey: true });
  expect(sent("send_now")).toHaveLength(1);
  expect(sent("send_now")[0].text).toBe("");
  expect(chips().map((c) => c.dataset.attachmentId)).toEqual([id]);
});

it("← at the start of the box moves onto the chips, and l past the last goes back", () => {
  const one = "d".repeat(32);
  const two = "e".repeat(32);
  const { textarea, chips } = mount({ images: [ready(one, "one.png"), ready(two, "two.png")] });
  fireEvent.keyDown(textarea(), { key: "ArrowLeft" });
  expect(document.activeElement).toBe(chips()[1]);
  fireEvent.keyDown(chips()[1], { key: "h" });
  expect(document.activeElement).toBe(chips()[0]);
  fireEvent.keyDown(chips()[0], { key: "l" });
  fireEvent.keyDown(chips()[1], { key: "l" });
  expect(document.activeElement).toBe(textarea());
});

it("one tab's import never holds another tab's", async () => {
  const { textarea } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
  await next("attach_begin");
  const second = { ...TAB, id: 2, number: 2, label: "2 new" };
  dispatch({ kind: "tabs", active: 2, defaultMode: "auto", tabs: [TAB, second] });
  dispatch({ kind: "draft_attachments", tab: 2, generation: 0, imageInput: true, attachments: [] });
  const hint = document.querySelector(".composer-browse-hint");
  if (hint !== null) fireEvent.focus(hint);
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(20))] } });
  const later = await next("attach_begin", 2);
  expect(later.tab).toBe(2);
});

it("a closed tab's refused import lets the next paste go", async () => {
  const { textarea, container } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
  const begin = await next("attach_begin");
  answer(begin, { attachmentId: "f".repeat(32) });
  const chunk = await next("attach_chunk");
  dispatch({ kind: "command_result", requestId: chunk.request_id, ok: false, error: "this image's tab is no longer in this window: paste it again" });
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(20))] } });
  expect((await next("attach_begin", 2)).byte_length).toBe(20);
  await waitFor(() => expect(container.querySelectorAll(".attachment-chip-importing")).toHaveLength(1));
});

it("chained images keep their originating generation across a tab switch and keep background refusals", async () => {
  const { textarea, container } = mount();
  dispatch({ kind: "draft_attachments", tab: 1, generation: 7, imageInput: true, attachments: [] });
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10)), imageItem(png(20))] } });
  const begin = await next("attach_begin");
  const second = { ...TAB, id: 2, number: 2, label: "2 new" };
  dispatch({ kind: "tabs", active: 2, defaultMode: "auto", tabs: [TAB, second] });
  dispatch({ kind: "draft_attachments", tab: 2, generation: 99, imageInput: true, attachments: [] });
  answer(begin, { attachmentId: "ab".repeat(16) });
  answer(await next("attach_chunk"));
  answer(await next("attach_end"));
  const later = await next("attach_begin", 2);
  expect(later).toMatchObject({ tab: 1, generation: 7 });
  dispatch({ kind: "command_result", requestId: later.request_id, ok: false, error: "this draft cannot take another image" });
  await act(async () => {});
  expect(container.textContent).not.toContain("this draft cannot take another image");
  dispatch({ kind: "tabs", active: 1, defaultMode: "auto", tabs: [TAB, second] });
  dispatch({ kind: "draft_attachments", tab: 1, generation: 7, imageInput: true, attachments: [] });
  await waitFor(() => expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("this draft cannot take another image"));
});

it("a chunk refused by the window gate cancels the import and marks its chip failed", async () => {
  const { textarea, chips, container } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(600 * 1024))] } });
  const id = "ac".repeat(16);
  answer(await next("attach_begin"), { attachmentId: id });
  dispatch({ kind: "attachment_state", tab: 1, attachmentId: id, name: "shot.png", state: "importing" });
  answer(await next("attach_chunk"));
  const refused = await next("attach_chunk", 2);
  dispatch({ kind: "command_result", requestId: refused.request_id, ok: false, error: "the panel is covered by another window" });
  expect(await next("attach_cancel")).toMatchObject({ tab: 1, attachment_id: id });
  await waitFor(() => expect(container.querySelector(".attachment-reason")?.textContent).toBe("the panel is covered by another window"));
  expect(container.querySelectorAll(".attachment-chip-importing")).toHaveLength(0);
  expect(sent("attach_chunk")).toHaveLength(2);
  expect(sent("attach_end")).toEqual([]);
  dispatch({ kind: "draft_attachments", tab: 1, generation: 0, imageInput: true, attachments: [] });
  expect(container.querySelector(".attachment-chip-failed")).toBeTruthy();
  fireEvent.keyDown(chips()[0], { key: "x" });
  expect(chips()).toHaveLength(0);
  expect(sent("attach_remove")).toEqual([]);
});

it.each([{}, { subtle: { digest: () => Promise.reject(new Error("unavailable")) } }])(
  "an image that cannot be hashed is cancelled and its chip says why", async (crypto) => {
    vi.stubGlobal("crypto", crypto);
    const { textarea, container } = mount();
    fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
    const id = "ad".repeat(16);
    answer(await next("attach_begin"), { attachmentId: id });
    answer(await next("attach_chunk"));
    expect(await next("attach_cancel")).toMatchObject({ tab: 1, attachment_id: id });
    await waitFor(() => expect(container.querySelector(".attachment-reason")?.textContent).toBe("this page cannot hash images"));
    expect(sent("attach_end")).toEqual([]);
  },
);

it("DOM file drops are prevented without reading files or importing", () => {
  const { container, textarea } = mount();
  const files = vi.fn(() => [png(10)]);
  const dataTransfer = { types: ["Files"], get files() { return files(); } };
  expect(fireEvent.dragOver(container.querySelector(".composer")!, { dataTransfer })).toBe(false);
  expect(fireEvent.drop(container.querySelector(".composer")!, { dataTransfer })).toBe(false);
  expect(files).not.toHaveBeenCalled();
  expect(sent("attach_begin")).toEqual([]);
  expect(textarea().value).toBe("");
});

it("background begin refusals survive another tab's notice with the same sequence", async () => {
  const { textarea, container } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
  const first = await next("attach_begin");
  const second = { ...TAB, id: 2, number: 2, label: "2 new" };
  dispatch({ kind: "tabs", active: 2, defaultMode: "auto", tabs: [TAB, second] });
  dispatch({ kind: "draft_attachments", tab: 2, generation: 1, imageInput: true, attachments: [] });
  const hint = container.querySelector(".composer-browse-hint");
  if (hint) fireEvent.focus(hint);
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(20))] } });
  const later = await next("attach_begin", 2);
  dispatch({ kind: "command_result", requestId: later.request_id, ok: false, error: "tab B refuses this image" });
  await waitFor(() => expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("tab B refuses this image"));
  dispatch({ kind: "command_result", requestId: first.request_id, ok: false, error: "tab A refuses this image" });
  await act(async () => {});
  expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("tab B refuses this image");
  dispatch({ kind: "tabs", active: 1, defaultMode: "auto", tabs: [TAB, second] });
  dispatch({ kind: "draft_attachments", tab: 1, generation: 0, imageInput: true, attachments: [] });
  await waitFor(() => expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("tab A refuses this image"));
});

it("an image notice stays with its tab when another tab has none", async () => {
  const { textarea, container } = mount();
  fireEvent.paste(textarea(), { clipboardData: { types: ["Files"], items: [imageItem(png(10))] } });
  const begin = await next("attach_begin");
  dispatch({ kind: "command_result", requestId: begin.request_id, ok: false, error: "this image was refused" });
  await waitFor(() => expect(container.querySelector(".composer-slash-flash")?.textContent).toBe("this image was refused"));
  const second = { ...TAB, id: 2, number: 2, label: "2 new" };
  dispatch({ kind: "tabs", active: 2, defaultMode: "auto", tabs: [TAB, second] });
  expect(container.querySelector(".composer-slash-flash")).toBeNull();
});
