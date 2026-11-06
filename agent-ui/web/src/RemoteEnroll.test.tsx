// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { installDispatch, installRemoteDesk } from "./bridge";
import { BYPASS_YES_GUARD_MS } from "./modeKey";
import { PLAIN_LINK_WARNING, RemoteEnroll } from "./RemoteEnroll";
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

const ENROLL = {
  kind: "remote_enroll",
  nonce: 4,
  name: "Pixel",
  host: "127.0.0.1:7341",
  peer: "127.0.0.1:50122",
  grants: ["watch", "watch+steer"],
  grant: "watch",
  expiresInMs: 180_000,
};

const answers = () => posted.filter((m) => m.type === "remote_enroll_answer" || m.type === "remote_listen_answer");

describe("RemoteEnroll", () => {
  it("device_name_shows_hidden_characters: the name a phone claims is drawn with its invisible characters escaped", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust({ ...ENROLL, name: "Pixel‮9 ​desk" });
    const text = container.textContent ?? "";
    expect(text).toContain("Add device Pixel⟨U+202E⟩9 ⟨U+200B⟩desk connecting through 127.0.0.1:7341 from 127.0.0.1:50122?");
    expect(text).not.toContain("‮");
    expect(text).toContain("the device name holds 2 invisible or direction-changing characters");
    expect(container.querySelectorAll(".permission-card-escape").length).toBe(2);
  });

  it("a y before the question is painted, or right after another key, answers nothing", () => {
    render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    // Pressed as the question appeared: the guard has not run from its paint.
    press("y", 1);
    expect(answers()).toEqual([]);
    // Right after another key.
    press("x");
    press("y", 10);
    expect(answers()).toEqual([]);
    // Alone, once the question has been on screen.
    press("y");
    expect(answers()).toEqual([{ type: "remote_enroll_answer", request_id: expect.any(String), nonce: 4, yes: true, grant: "watch" }]);
  });

  it("n, Escape and Enter answer no", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    for (const key of ["n", "Escape", "Enter"]) {
      fromRust(ENROLL);
      press(key);
      expect(container.textContent).toBe("");
    }
    expect(answers().map((m) => m.yes)).toEqual([false, false, false]);
  });

  it("Tab chooses the grant the y sends, among those offered", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    expect(container.textContent).toContain("It may: watch (Tab changes)");
    press("Tab");
    expect(container.textContent).toContain("It may: watch and steer");
    press("y");
    expect(answers()[0]).toMatchObject({ yes: true, grant: "watch+steer" });

    fromRust({ ...ENROLL, nonce: 5, grants: ["watch"] });
    press("Tab");
    expect(container.textContent).toContain("It may: watch (a phone using SSH only watches for now)");
    press("y");
    expect(answers()[1]).toMatchObject({ nonce: 5, yes: true, grant: "watch" });
  });

  it("a y the window turns down keeps the question up with the reason, to be answered again", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    press("y");
    const first = answers()[0];
    fromRust({ kind: "command_result", requestId: first.request_id, ok: false, error: "agent 1 is under terminal 1" });
    expect(container.textContent).toContain("Add device Pixel");
    expect(container.textContent).toContain("agent 1 is under terminal 1");
    press("y");
    const second = answers()[1];
    expect(second).toMatchObject({ nonce: 4, yes: true });
    fromRust({ kind: "command_result", requestId: second.request_id, ok: true });
    expect(container.textContent).toBe("");
  });

  it("plain_interface_warns_and_defaults_no: a plain link is named as plain text, and Enter is no", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust({ kind: "remote_listen", nonce: 9, addr: "198.51.100.20", iface: "eth0", link: "plain", plain: true, expiresInMs: 180_000 });
    expect(container.textContent).toContain("Listen for phones on 198.51.100.20 (eth0, plain)? (y/N)");
    expect(container.textContent).toContain(PLAIN_LINK_WARNING);
    press("Enter");
    expect(answers()).toEqual([{ type: "remote_listen_answer", request_id: expect.any(String), nonce: 9, yes: false }]);

    fromRust({ kind: "remote_listen", nonce: 10, addr: "203.0.113.2", iface: "wg0", link: "wireguard", plain: false, expiresInMs: 180_000 });
    expect(container.textContent).not.toContain(PLAIN_LINK_WARNING);
  });

  it("grant_change_needs_a_desk_yes: a grant question names both grants, and a y answers with the grant asked about", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    const GRANT = { kind: "remote_grant", nonce: 7, device: "d1f2", name: "Pixel", from: "watch", to: "watch+steer", expiresInMs: 180_000 };
    fromRust(GRANT);
    expect(container.textContent).toContain("Let device Pixel (d1f2) watch and steer (now it may only watch)? (y/n)");
    press("y", 1);
    expect(answers()).toEqual([]);
    press("Tab");
    press("y");
    expect(answers()).toEqual([
      { type: "remote_enroll_answer", request_id: expect.any(String), nonce: 7, yes: true, grant: "watch+steer" },
    ]);
    fromRust({ kind: "command_result", requestId: answers()[0].request_id, ok: true });
    expect(container.textContent).toBe("");

    fromRust({ ...GRANT, nonce: 8, from: "watch+steer", to: "watch" });
    press("n");
    expect(answers()[1]).toMatchObject({ nonce: 8, yes: false, grant: "watch" });
    fromRust({ ...GRANT, nonce: 9, to: "admin" });
    expect(container.textContent).toBe("");
  });

  it("device_without_confirmation_is_flagged_once_per_process: a flagged device is named once, with how to take it out", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust({ kind: "remote_device_flagged", device: "d9a", name: "Planted\u200b" });
    fromRust({ kind: "remote_device_flagged", device: "d9a", name: "Planted\u200b" });
    const text = container.textContent ?? "";
    expect(text).toContain("Device Planted⟨U+200B⟩ (d9a) is in the device list, but no desk said yes to it.");
    expect(text).toContain("eitri remote revoke d9a");
    expect(container.querySelectorAll(".remote-warning").length).toBe(1);
    press("Escape");
    expect(container.textContent).toBe("");
    expect(answers()).toEqual([]);
  });

  it("shows the code to scan until a phone presents it, and drops a malformed question", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust({ kind: "remote_pairing", code: "BwcHBwcHBwcHBwcHBwcHBw", uri: "eitri-pair:v=1", modules: ["10", "01"], expiresInMs: 180000 });
    expect(container.querySelectorAll(".remote-qr rect").length).toBe(3);
    expect(container.textContent).toContain("BwcHBwcHBwcHBwcHBwcHBw");
    fromRust({ ...ENROLL, grant: "root" });
    expect(container.textContent).not.toContain("Add device");
    fromRust(ENROLL);
    expect(container.querySelector(".remote-qr")).toBeNull();
    expect(container.textContent).toContain("Add device");
  });

  it("a question outlives the component: drawn again after a switch of layout, it still waits for its answer", () => {
    const first = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    first.unmount();
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    expect(container.textContent).toContain("Add device Pixel");
    // Drawn anew, it must be on screen afresh before a y counts.
    press("y", 1);
    expect(answers()).toEqual([]);
    press("y");
    expect(answers()).toEqual([{ type: "remote_enroll_answer", request_id: expect.any(String), nonce: 4, yes: true, grant: "watch" }]);
    fromRust({ kind: "command_result", requestId: answers()[0].request_id, ok: true });
    expect(container.textContent).toBe("");
  });

  it("questions of different kinds wait one behind another, each answered under its own nonce", () => {
    const { container } = render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    fromRust({ kind: "remote_listen", nonce: 9, addr: "203.0.113.2", iface: "wg0", link: "wireguard", plain: false, expiresInMs: 180_000 });
    expect(container.textContent).toContain("Add device Pixel");
    expect(container.textContent).toContain("1 more question waits");
    expect(container.textContent).not.toContain("Listen for phones");
    press("n");
    expect(container.textContent).toContain("Listen for phones on 203.0.113.2");
    // The next question is new on screen: a y right away does not count.
    press("y", 1);
    expect(answers()).toEqual([{ type: "remote_enroll_answer", request_id: expect.any(String), nonce: 4, yes: false, grant: "watch" }]);
    press("y");
    expect(answers()[1]).toMatchObject({ type: "remote_listen_answer", nonce: 9, yes: true });
  });

  it("while another overlay owns the keys the question takes none, and is shown afresh once uncovered", () => {
    const view = render(<RemoteEnroll desk={desk} covered now={now} />);
    fromRust(ENROLL);
    for (const key of ["y", "n", "Escape", "Enter", "Tab"]) {
      expect(press(key), key).toBe(true);
    }
    expect(answers()).toEqual([]);
    expect(view.container.textContent).toContain("Add device Pixel");
    view.rerender(<RemoteEnroll desk={desk} now={now} />);
    press("y", 1);
    expect(answers()).toEqual([]);
    press("y");
    expect(answers()).toEqual([{ type: "remote_enroll_answer", request_id: expect.any(String), nonce: 4, yes: true, grant: "watch" }]);
  });

  it("a y with a modifier held, being composed or repeated is not the answer", () => {
    render(<RemoteEnroll desk={desk} now={now} />);
    fromRust(ENROLL);
    expect(press("y", BYPASS_YES_GUARD_MS, { ctrlKey: true })).toBe(true);
    expect(press("y", BYPASS_YES_GUARD_MS, { altKey: true })).toBe(true);
    expect(press("y", BYPASS_YES_GUARD_MS, { isComposing: true })).toBe(true);
    expect(press("y", BYPASS_YES_GUARD_MS, { keyCode: 229 })).toBe(true);
    expect(press("n", BYPASS_YES_GUARD_MS, { ctrlKey: true })).toBe(true);
    press("y", BYPASS_YES_GUARD_MS, { repeat: true });
    expect(answers()).toEqual([]);
    press("y");
    expect(answers()).toEqual([{ type: "remote_enroll_answer", request_id: expect.any(String), nonce: 4, yes: true, grant: "watch" }]);
  });

  it("a question the window stopped waiting for is taken down, and so is one past its time", () => {
    vi.useFakeTimers();
    try {
      const { container } = render(<RemoteEnroll desk={desk} now={now} />);
      fromRust(ENROLL);
      fromRust({ kind: "remote_question_closed", nonce: 4 });
      expect(container.textContent).toBe("");
      fromRust({ ...ENROLL, nonce: 5, expiresInMs: 1_000 });
      expect(container.textContent).toContain("Add device Pixel");
      act(() => void vi.advanceTimersByTime(1_000));
      expect(container.textContent).toBe("");
      expect(answers()).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });
});
