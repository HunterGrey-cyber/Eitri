// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { RemoteProjectChooser, type RemoteProjectAction, type RemoteProjectPage } from "./RemoteProjectChooser";

afterEach(cleanup);

function chooser() {
  const actions: RemoteProjectAction[] = [];
  const host = window as unknown as {
    webkit: unknown;
    eitriRemoteProject: (page: RemoteProjectPage) => void;
  };
  host.webkit = {
    messageHandlers: { eitriRemoteProject: { postMessage: (message: string) => actions.push(JSON.parse(message)) } },
  };
  render(<RemoteProjectChooser />);
  const show = (page: RemoteProjectPage) => act(() => host.eitriRemoteProject(page));
  return { actions, show };
}

it("open_on_a_host_shows_each_step", () => {
  const { actions, show } = chooser();
  expect(actions).toEqual([{ kind: "ready" }]);
  show({ kind: "hosts", hosts: ["example-host", "recent-host"] });
  expect(screen.getByText("recent-host")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "example-host" }));
  expect(actions.at(-1)).toEqual({ kind: "host", host: "example-host" });

  const question = (id: number, text: string) => {
    show({ kind: "question", id, text });
    expect(screen.getByText(text)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Yes (y)" }));
    expect(actions.at(-1)).toEqual({ kind: "answer", id, yes: true });
  };
  question(1, "Host key SHA256:example. Add it to your known hosts?");
  show({ kind: "line", text: "Server: Linux x86_64, glibc 2.39" });
  expect(screen.getByText("Server: Linux x86_64, glibc 2.39")).toBeTruthy();
  show({ kind: "folder", id: 3 });
  const folder = screen.getByLabelText("Server folder");
  fireEvent.change(folder, { target: { value: "/srv/pro" } });
  fireEvent.click(screen.getByRole("button", { name: "Complete folder" }));
  expect(actions.at(-1)).toEqual({ kind: "complete", id: 3, prefix: "/srv/pro" });
  question(2, "Install the server helper on example-host?");
  show({ kind: "suggestions", id: 3, prefix: "/srv/pro", folders: ["/srv/project‮/"], error: null });
  expect((screen.getByLabelText("Server folder") as HTMLInputElement).value).toBe("/srv/pro");
  fireEvent.click(screen.getByRole("button", { name: "/srv/project⟨U+202E⟩/" }));
  fireEvent.click(screen.getByRole("button", { name: "Open folder" }));
  expect(actions.at(-1)).toEqual({ kind: "folder", id: 3, path: "/srv/project‮" });
  show({ kind: "line", text: "Local copy: /srv/project" });
  expect(screen.getByText("Local copy: /srv/project")).toBeTruthy();
  show({ kind: "question", id: 4, text: "Move /srv/project\n spoof to a backup?" });
  expect(screen.getByText(/Move \/srv\/project/).parentElement?.textContent).toContain("⟨U+000A⟩");
  fireEvent.keyDown(screen.getByRole("region", { name: "Open on a host…" }), { key: "y" });
  expect(actions.at(-1)).toEqual({ kind: "answer", id: 4, yes: true });
  for (const text of ["expected size: 80 MiB", "syncing 2 files, 75 / 80 MiB", "history arriving: 50 %"]) {
    show({ kind: "line", text });
    expect(screen.getByText(text)).toBeTruthy();
  }
});

it("editing a revealed folder choice restores the typed prefix", () => {
  const { actions, show } = chooser();
  const prefix = "/srv/pro";
  show({ kind: "folder", id: 3 });
  const folder = screen.getByLabelText("Server folder") as HTMLInputElement;
  fireEvent.change(folder, { target: { value: prefix } });
  fireEvent.click(screen.getByRole("button", { name: "Complete folder" }));
  show({ kind: "suggestions", id: 3, prefix, folders: ["/srv/project\u202e/"], error: null });
  fireEvent.click(screen.getByRole("button", { name: "/srv/project⟨U+202E⟩/" }));
  expect(folder.value).toBe("/srv/project⟨U+202E⟩/");
  fireEvent.change(folder, { target: { value: folder.value + "child" } });
  expect(folder.value).toBe(prefix);
  fireEvent.change(folder, { target: { value: prefix + "ject2" } });
  fireEvent.click(screen.getByRole("button", { name: "Open folder" }));
  expect(actions.at(-1)).toEqual({ kind: "folder", id: 3, path: "/srv/project2" });
});

it.each(["A Syncthing folder overlaps this project.", "Remote projects need Mutagen 0.18 or later."])(
  "shows a refusal as plain text: %s",
  (text) => {
    const host = window as unknown as { eitriRemoteProject: (page: RemoteProjectPage) => void };
    const { container } = render(<RemoteProjectChooser />);
    act(() => host.eitriRemoteProject({ kind: "error", text: text + " <img src=x>" }));
    expect(screen.getByRole("alert").textContent).toBe(text + " <img src=x>");
    expect(container.querySelector("img")).toBeNull();
  },
);
