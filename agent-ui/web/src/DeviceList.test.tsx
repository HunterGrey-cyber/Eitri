// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import css from "./index.css?raw";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { installDispatch, installRemoteDesk } from "./bridge";
import { BYPASS_YES_GUARD_MS } from "./modeKey";
import { DeviceList, DEVICE_LIST_NOTE } from "./DeviceList";
import { RemoteEnroll } from "./RemoteEnroll";
import { RemoteDesk } from "./remoteDesk";

type Posted = Record<string, unknown>;

let posted: Posted[] = [];
let clock = 0;
const now = () => clock;
let desk: RemoteDesk;

beforeEach(() => {
  posted = [];
  clock = 1_000;
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  installDispatch(() => {});
  desk = new RemoteDesk();
  installRemoteDesk(desk.receive);
});

afterEach(() => {
  cleanup();
  installRemoteDesk(null);
});

function fromRust(payload: object) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}

/** Presses `key` `after` ms after the last one; says whether the key was left for others (not taken). */
function press(key: string, after = BYPASS_YES_GUARD_MS, init: Record<string, unknown> = {}): boolean {
  clock += after;
  return fireEvent.keyDown(window, { key, ...init });
}

const record = (event: string, time: string, fields: [string, string][] = []) => ({ time, event, writer: "serve", fields });

const PIXEL = {
  id: "d1f2",
  name: "Pixel",
  grant: "watch+steer",
  recipe: "direct",
  enrolled_at: 1_700_000_000,
  last_seen: 1_700_000_500,
  last_host: "desk.tail1.ts.net",
  connected: true,
  push_host: "ntfy.example.net",
  records: [
    record("conn_open", "2026-11-07T10:00:02Z", [["device", "d1f2"], ["host", "desk.tail1.ts.net"]]),
    record("enroll_confirmed", "2026-11-07T09:59:00Z", [["device", "d1f2"], ["grant", "watch+steer"]]),
  ],
};
const TABLET = {
  id: "d9a0",
  name: "Tablet",
  grant: "watch",
  recipe: "ssh",
  enrolled_at: null,
  last_seen: null,
  last_host: null,
  connected: false,
  push_host: null,
  records: [],
};

const sentTypes = () => posted.map((m) => m.type);

/** Remote control on, the list opened from the panel's key, and the window's answer with `devices`. */
function openWith(devices: object[]) {
  fromRust({ kind: "remote_on" });
  act(() => {
    expect(desk.openList()).toBe(true);
  });
  expect(posted.at(-1)).toMatchObject({ type: "remote_devices_open" });
  fromRust({ kind: "remote_devices", devices });
}

describe("DeviceList", () => {
  it("list_shows_each_device_and_its_last_records: one row per device, Enter shows its records", () => {
    const { container } = render(<DeviceList desk={desk} now={now} />);
    openWith([PIXEL, TABLET]);
    const rows = container.querySelectorAll(".device-row");
    expect(rows.length).toBe(2);
    const pixel = rows[0].textContent ?? "";
    for (const part of ["Pixel", "watch+steer", "direct", "desk.tail1.ts.net", "connected", "ntfy.example.net"]) {
      expect(pixel).toContain(part);
    }
    const tablet = rows[1].textContent ?? "";
    expect(tablet).toContain("Tablet");
    expect(tablet).toContain("not connected");
    expect(tablet).toContain("never seen");
    // Records are shown for the selected row on Enter, newest first, every field with it.
    expect(container.querySelectorAll(".device-record").length).toBe(0);
    press("Enter");
    const shown = [...container.querySelectorAll(".device-record")].map((r) => r.textContent ?? "");
    expect(shown.length).toBe(2);
    expect(shown[0]).toContain("conn_open");
    expect(shown[0]).toContain("host=desk.tail1.ts.net");
    expect(shown[1]).toContain("enroll_confirmed");
    press("Enter");
    expect(container.querySelectorAll(".device-record").length).toBe(0);
    // j moves to the next device, whose records Enter shows (none).
    press("j");
    press("Enter");
    expect(container.textContent).toContain("no records");
    press("Escape");
    expect(container.textContent).toBe("");
  });

  it("a_starts_an_enrollment: a asks the window for a code by the recipe chosen, and r reads the list again", () => {
    const { container } = render(<DeviceList desk={desk} now={now} />);
    openWith([PIXEL]);
    posted = [];
    press("a");
    expect(posted).toEqual([{ type: "remote_start_enroll", request_id: expect.any(String), recipe: "auto" }]);
    expect(container.textContent).toContain("(this window's default route; Tab changes)");
    press("Tab");
    expect(container.textContent).toContain("through SSH");
    press("a");
    expect(posted[1]).toMatchObject({ type: "remote_start_enroll", recipe: "ssh" });
    press("Tab");
    press("a");
    expect(posted[2]).toMatchObject({ type: "remote_start_enroll", recipe: "direct" });
    // A refusal is said in the list.
    fromRust({ kind: "command_result", requestId: posted[2].request_id, ok: false, error: "remote control has no address for phones in this window" });
    expect(container.textContent).toContain("remote control has no address for phones in this window");
    press("r");
    expect(posted[3]).toMatchObject({ type: "remote_devices_open" });
  });

  it("x_asks_then_revokes: x asks the window, and only a painted y of the window's question revokes", () => {
    const { container } = render(
      <>
        <DeviceList desk={desk} now={now} />
        <RemoteEnroll desk={desk} now={now} />
      </>,
    );
    openWith([PIXEL, TABLET]);
    posted = [];
    press("x");
    expect(posted).toEqual([{ type: "remote_revoke_ask", request_id: expect.any(String), device: "d1f2" }]);
    // Nothing is revoked until the window asks and the desk says yes.
    fromRust({ kind: "remote_revoke_ask", nonce: 12, device: "d1f2", name: "Pixel", expiresInMs: 180_000 });
    expect(container.textContent).toContain("Revoke Pixel (d1f2)? (y/n)");
    // A y pressed as the question appeared, or right after another key, answers nothing.
    press("y", 1);
    press("k");
    press("y", 10);
    expect(posted.filter((m) => m.type === "remote_revoke")).toEqual([]);
    // While the question waits, the list's own keys are not the list's: j moves nothing, x asks nothing.
    press("x");
    expect(posted.filter((m) => m.type === "remote_revoke_ask").length).toBe(1);
    press("y");
    expect(posted.filter((m) => m.type === "remote_revoke")).toEqual([
      { type: "remote_revoke", request_id: expect.any(String), device: "d1f2", nonce: 12 },
    ]);
    const yes = posted.find((m) => m.type === "remote_revoke")!;
    fromRust({ kind: "command_result", requestId: yes.request_id, ok: true });
    expect(container.textContent).not.toContain("Revoke Pixel");
    // n takes the question down and sends nothing.
    fromRust({ kind: "remote_revoke_ask", nonce: 13, device: "d9a0", name: "Tablet", expiresInMs: 180_000 });
    const before = posted.length;
    press("n");
    expect(container.textContent).not.toContain("Revoke Tablet");
    expect(posted.length).toBe(before);
  });

  it("no_key_raises_a_grant: every key the list takes reads, makes a code or asks about a revoke", () => {
    render(
      <>
        <DeviceList desk={desk} now={now} />
        <RemoteEnroll desk={desk} now={now} />
      </>,
    );
    openWith([{ ...PIXEL, grant: "watch" }, TABLET]);
    posted = [];
    const keys = [..."abcdefghijklmnoprstuvwxz+-=<>ABCDEFGHIJKLMNOPQRSTUVWXYZ"].concat(["Enter", "Tab", " ", "ArrowUp", "ArrowDown"]);
    for (const key of keys) press(key);
    const kinds = new Set(sentTypes());
    for (const kind of kinds) {
      expect(["remote_devices_open", "remote_start_enroll", "remote_revoke_ask"]).toContain(kind);
    }
    expect(kinds.has("remote_enroll_answer")).toBe(false);
    // The list says where a grant is raised instead.
    expect(DEVICE_LIST_NOTE).toContain("eitri remote grant");
  });

  it("list_hidden_while_remote_is_off: the list does not open, and draws nothing, until remote control is on", () => {
    const { container } = render(<DeviceList desk={desk} now={now} />);
    act(() => {
      expect(desk.openList()).toBe(false);
    });
    expect(posted).toEqual([]);
    fromRust({ kind: "remote_devices", devices: [PIXEL] });
    expect(container.textContent).toBe("");
    expect(press("j")).toBe(true);
    fromRust({ kind: "remote_on" });
    act(() => {
      expect(desk.openList()).toBe(true);
    });
    fromRust({ kind: "remote_devices", devices: [PIXEL] });
    expect(container.textContent).toContain("Pixel");
  });

  it("record_fields_show_hidden_characters: names, hosts and every record field are drawn with hidden characters escaped", () => {
    const { container } = render(<DeviceList desk={desk} now={now} />);
    openWith([
      {
        ...PIXEL,
        name: "Pix‮el",
        last_host: "desk​.example",
        push_host: "nt⁦fy.example",
        records: [record("refused", "2026-11-07T10:00:00Z", [["device_name", "evil‮"], ["host", "h​ost"]])],
      },
    ]);
    press("Enter");
    const text = container.textContent ?? "";
    for (const raw of ["‮", "​", "⁦"]) expect(text).not.toContain(raw);
    expect(text).toContain("Pix⟨U+202E⟩el");
    expect(text).toContain("desk⟨U+200B⟩.example");
    expect(text).toContain("nt⟨U+2066⟩fy.example");
    expect(text).toContain("device_name=evil⟨U+202E⟩");
    expect(text).toContain("host=h⟨U+200B⟩ost");
  });

  it("the list owns the keys while it is open: a key it has no use for reaches nothing behind it", () => {
    render(
      <>
        <DeviceList desk={desk} now={now} />
        <RemoteEnroll desk={desk} now={now} />
      </>,
    );
    openWith([PIXEL]);
    posted = [];
    const reached: string[] = [];
    const behind = (event: KeyboardEvent) => reached.push(event.key);
    // Where the panel's own key handlers listen: a card's `d`, INPUT's `i` and the rest.
    document.addEventListener("keydown", behind);
    const down = (key: string, init: Record<string, unknown> = {}) => {
      clock += BYPASS_YES_GUARD_MS;
      return fireEvent.keyDown(document.body, { key, ...init });
    };
    try {
      for (const key of ["d", "D", "i", "A", " ", "1", "c", "ISO_Left_Tab"]) expect(down(key), key).toBe(false);
      expect(down("Tab", { shiftKey: true })).toBe(false);
      expect(reached).toEqual([]);
      // `?` still opens the keys sheet, which lists the list's own keys; a key with Ctrl held is not the list's.
      expect(down("?")).toBe(true);
      expect(down("c", { ctrlKey: true })).toBe(true);
      expect(reached).toEqual(["?", "c"]);
      reached.length = 0;
      // While the window's question waits, its keys are its own and the rest still reach nothing.
      fromRust({ kind: "remote_revoke_ask", nonce: 7, device: "d1f2", name: "Pixel", expiresInMs: 180_000 });
      for (const key of ["d", "i", "x", "j"]) expect(down(key), key).toBe(false);
      expect(down("Tab", { shiftKey: true })).toBe(false);
      expect(reached).toEqual([]);
      // A y right after a key the list took still follows a key: it answers nothing.
      down("d");
      clock += 10;
      fireEvent.keyDown(document.body, { key: "y" });
      expect(posted.filter((m) => m.type === "remote_revoke")).toEqual([]);
      expect(down("y")).toBe(false);
      expect(posted.filter((m) => m.type === "remote_revoke")).toHaveLength(1);
    } finally {
      document.removeEventListener("keydown", behind);
    }
  });

  it("a key typed into a text field is the field's, not the list's", () => {
    const { container } = render(
      <>
        <textarea />
        <DeviceList desk={desk} now={now} />
      </>,
    );
    openWith([PIXEL, TABLET]);
    posted = [];
    const box = container.querySelector("textarea")!;
    box.focus();
    for (const key of ["j", "k", "x", "a", "r", "Enter", "Tab", "q", "Escape"]) {
      expect(fireEvent.keyDown(box, { key }), key).toBe(true);
    }
    expect(posted).toEqual([]);
    expect(container.textContent).toContain("Phones (remote control)");
    expect(container.querySelector(".device-row.selected")?.textContent).toContain("Pixel");
  });

  it("a held a, r or x does its work once", () => {
    render(<DeviceList desk={desk} now={now} />);
    openWith([PIXEL]);
    posted = [];
    for (const key of ["r", "a", "x"]) {
      press(key);
      for (let n = 0; n < 5; n += 1) expect(press(key, 30, { repeat: true })).toBe(false);
    }
    expect(sentTypes()).toEqual(["remote_devices_open", "remote_start_enroll", "remote_revoke_ask"]);
  });

  it("the list is bounded inside the panel and keeps the selected row in view", () => {
    const scrolled = vi.fn();
    Element.prototype.scrollIntoView = scrolled;
    try {
      const { container } = render(<DeviceList desk={desk} now={now} />);
      openWith([PIXEL, TABLET]);
      press("j");
      expect(scrolled.mock.contexts.at(-1)).toBe(container.querySelectorAll(".device-row")[1]);
      expect(scrolled).toHaveBeenLastCalledWith({ block: "nearest" });
    } finally {
      delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView;
    }
    // A device's records scroll inside the list, so the window's y/n drawn under it stays on screen.
    const rule = /(?:^|\})\s*\.device-list\s*\{([^}]*)\}/.exec(css.replace(/\/\*[\s\S]*?\*\//g, ""));
    expect(rule, ".device-list has a rule of its own").not.toBeNull();
    const body = rule![1];
    expect(body).toMatch(/max-height\s*:/);
    expect(body).toMatch(/overflow-y\s*:\s*auto/);
    expect(body).toMatch(/flex\s*:\s*0 0 auto/);
  });

  it("drops a device list that cannot be drawn whole", () => {
    const { container } = render(<DeviceList desk={desk} now={now} />);
    openWith([PIXEL]);
    fromRust({ kind: "remote_devices", devices: [{ ...PIXEL, records: [{ time: 1 }] }] });
    fromRust({ kind: "remote_devices", devices: [{ ...PIXEL, grant: "root" }] });
    fromRust({ kind: "remote_revoke_ask", nonce: 3, device: "d1f2", expiresInMs: 180_000 });
    expect(container.querySelectorAll(".device-row").length).toBe(1);
    expect(desk.snapshot().questions).toEqual([]);
  });
});
