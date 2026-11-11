// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { detectTransport } from "./index";

function fakeWindow(properties: Record<string, unknown> = {}): Window {
  return properties as unknown as Window;
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.resetModules();
});

it("webkit_transport_posts_through_the_message_handler", () => {
  const postMessage = vi.fn();
  const w = fakeWindow({ webkit: { messageHandlers: { eitriAgent: { postMessage } } } });
  const transport = detectTransport(w);
  const json = '{"type":"ready","request_id":"req-1"}';
  expect(transport.name).toBe("webkit");
  transport.post(json);
  expect(postMessage.mock.calls).toEqual([[json]]);
  const dispatch = vi.fn();
  transport.listen(dispatch);
  w.__eitriDispatch!(json);
  expect(dispatch.mock.calls).toEqual([[json]]);
});

it("webkit_ids_keep_the_kind_and_increase_the_counter", () => {
  const transport = detectTransport(fakeWindow());
  expect(transport.nextId("req")).toMatch(/^req-[a-f0-9]{32}-1$/);
  expect(transport.nextId("mirror")).toMatch(/^mirror-[a-f0-9]{32}-2$/);
  expect(transport.nextId("req")).toMatch(/^req-[a-f0-9]{32}-3$/);
});

it("android_transport_posts_through_eitri_host", async () => {
  const postMessage = vi.fn();
  const webkitPost = vi.fn();
  vi.stubGlobal("window", fakeWindow({
    eitriHost: { postMessage },
    webkit: { messageHandlers: { eitriAgent: { postMessage: webkitPost } } },
  }));
  const { postToRust } = await import("../bridge");
  postToRust({ type: "ready", request_id: "a-req-1" });
  expect(postMessage.mock.calls).toEqual([['{"type":"ready","request_id":"a-req-1"}']]);
  expect(webkitPost).not.toHaveBeenCalled();
});

it("android_messages_reach_the_dispatch", async () => {
  const host = { postMessage: vi.fn(), onmessage: null as ((event: { data: unknown }) => void) | null };
  vi.stubGlobal("window", fakeWindow({ eitriHost: host }));
  const { installDispatch } = await import("../bridge");
  const handler = vi.fn();
  installDispatch(handler);
  const envelope = { kind: "notice", text: "From Android" };
  host.onmessage!({ data: JSON.stringify(envelope) });
  expect(handler.mock.calls).toEqual([[envelope]]);
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  try {
    host.onmessage!({ data: "invalid json" });
    host.onmessage!({ data: JSON.stringify({ kind: "unknown" }) });
    host.onmessage!({ data: { kind: "notice", text: "not a string" } });
    expect(handler).toHaveBeenCalledTimes(1);
  } finally {
    warn.mockRestore();
  }
  const replacement = vi.fn();
  installDispatch(replacement);
  host.onmessage!({ data: JSON.stringify(envelope) });
  expect(replacement.mock.calls).toEqual([[envelope]]);
  expect(handler).toHaveBeenCalledTimes(1);
});

it("android_ids_have_their_own_prefix", async () => {
  vi.stubGlobal("window", fakeWindow({ eitriHost: { postMessage: vi.fn() } }));
  const { nextRequestId, nextMirrorId } = await import("../bridge");
  expect(nextRequestId()).toMatch(/^a-req-[a-f0-9]{32}-1$/);
  expect(nextMirrorId()).toMatch(/^a-mirror-[a-f0-9]{32}-2$/);
  expect(nextRequestId()).toMatch(/^a-req-[a-f0-9]{32}-3$/);
  const desktop = detectTransport(fakeWindow());
  expect(desktop.nextId("req")).toMatch(/^req-[a-f0-9]{32}-1$/);
});

it.each(["webkit", "android"])("%s instances mint different request ids across loads", (kind) => {
  const properties = kind === "android" ? { eitriHost: { postMessage: vi.fn() } } : {};
  let load = 0;
  const random = vi.spyOn(crypto, "getRandomValues").mockImplementation((array) => {
    (array as Uint32Array).fill(++load);
    return array;
  });
  const first = detectTransport(fakeWindow(properties));
  const second = detectTransport(fakeWindow(properties));
  const ids = [first.nextId("req"), second.nextId("req"), first.nextId("mirror"), second.nextId("mirror")];
  expect(new Set(ids).size).toBe(4);
  expect(random).toHaveBeenCalledTimes(2);
  random.mockRestore();
});

it("is_mirror_request_knows_both_prefixes", async () => {
  const { isMirrorRequest } = await import("../bridge");
  expect(isMirrorRequest("mirror-1")).toBe(true);
  expect(isMirrorRequest("a-mirror-2")).toBe(true);
  for (const id of ["req-1", "a-req-2", "other-mirror-3", ""]) {
    expect(isMirrorRequest(id)).toBe(false);
  }
});

it("detect_prefers_android_only_when_eitri_host_is_real", () => {
  for (const eitriHost of [undefined, null, "host", {}, { postMessage: "post" }, () => {}]) {
    expect(detectTransport(fakeWindow({ eitriHost })).name).toBe("webkit");
  }
  expect(detectTransport(fakeWindow({ eitriHost: { postMessage: vi.fn() } })).name).toBe("android");
});
