import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import type { RemoteDeviceRecord, RemoteDeviceRow, RemoteRecipeChoice } from "./bridge";
import { isPlainAnswerKey } from "./keymap";
import type { KeyHelp, KeyLike } from "./keymap";
import { isModifierKey } from "./typingGuard";
import { revealHidden } from "./revealHidden";
import { HiddenWarning, Revealed } from "./components/Revealed";
import { isEditableElement } from "./app/elements";
import { tabChangesGrant } from "./RemoteEnroll";
import type { RemoteDesk } from "./remoteDesk";

/** The device list's keys, as the `?` sheet lists them. Like tmux's choose-tree and vim's lists: `j`/`k` move, `Enter`
 *  opens the row, `Escape` or `q` closes. */
export const DEVICE_LIST_KEYS: KeyHelp[] = [
  { keys: "j / k", what: "next / previous device" },
  { keys: "Enter", what: "show or hide the device's last 20 audit records" },
  { keys: "a", what: "add a phone: show an enrollment code, as eitri remote add-device does" },
  { keys: "Tab", what: "change the route the next code pairs by" },
  { keys: "x", what: "revoke the device, after a y/n" },
  { keys: "r", what: "read the list again" },
  { keys: "Esc / q", what: "close the list" },
];

/** The keys a waiting question of the window's answers with besides Escape and (sometimes) Tab: `RemoteEnroll`'s. */
const QUESTION_KEYS = new Set(["y", "Y", "n", "N", "Enter"]);

/** What the list says about grants: no key here raises one. */
export const DEVICE_LIST_NOTE = "A grant is raised only with eitri remote grant <device> watch+steer and its y/n.";

const RECIPES: RemoteRecipeChoice[] = ["auto", "ssh", "direct"];

function recipeWords(recipe: RemoteRecipeChoice): string {
  switch (recipe) {
    case "auto":
      return "this window's default route";
    case "ssh":
      return "through SSH";
    case "direct":
      return "directly";
  }
}

function hiddenCount(text: string): number {
  return revealHidden(text).filter((piece) => "escape" in piece).length;
}

/** Seconds since the Unix epoch, as the list shows them (UTC, to the minute). */
function shownTime(seconds: number): string {
  return `${new Date(seconds * 1000).toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

function Text({ text }: { text: string }) {
  return <Revealed pieces={revealHidden(text)} />;
}

function RecordLine({ record }: { record: RemoteDeviceRecord }) {
  return (
    <li className="device-record">
      <Text text={record.time} /> <Text text={record.event} /> <Text text={record.writer} />
      {record.fields.map(([name, value], i) => (
        <span className="device-field" key={`${i}:${name}`}>
          {" "}
          <Text text={name} />=<Text text={value} />
        </span>
      ))}
    </li>
  );
}

function Row({ device, selected, expanded }: { device: RemoteDeviceRow; selected: boolean; expanded: boolean }) {
  return (
    <li className={selected ? "device-row selected" : "device-row"} aria-selected={selected}>
      <div>
        <Text text={device.name} /> · {device.grant} · {device.recipe} ·{" "}
        {device.connected ? "connected" : "not connected"}
        {device.last_host !== null && (
          <>
            {" "}
            · through <Text text={device.last_host} />
          </>
        )}{" "}
        · {device.last_seen !== null ? `last seen ${shownTime(device.last_seen)}` : "never seen"}
        {device.enrolled_at !== null && <> · added {shownTime(device.enrolled_at)}</>}
        {device.push_host !== null && (
          <>
            {" "}
            · wakes through <Text text={device.push_host} />
          </>
        )}{" "}
        (<Text text={device.id} />)
      </div>
      <HiddenWarning count={hiddenCount(device.name)} what="device name" />
      {expanded &&
        (device.records.length === 0 ? (
          <div className="remote-hint">no records</div>
        ) : (
          <ul className="device-records">
            {device.records.map((record, i) => (
              <RecordLine record={record} key={i} />
            ))}
          </ul>
        ))}
    </li>
  );
}

/** The panel's device list (remote control), opened from the panel's key while remote control is on in this window:
 *  one row per enrolled device, with its name, grant, recipe, whether it is connected, the `Host` it last came through,
 *  when it was last seen and where its wakes go (by host), every string drawn with its hidden characters escaped.
 *  `Enter` shows the device's last 20 audit records, `a` asks the window for an enrollment code (the same code and QR
 *  `eitri remote add-device` shows), `x` asks the window to revoke the device (the window asks its own y/n, drawn by
 *  `RemoteEnroll`), `r` reads the list again and `Escape` closes it. No key here raises a grant.
 *
 *  While open it owns the panel's keys, as tmux's choose-tree does: a key it has no use for goes nowhere (a card's `d`
 *  or INPUT's `i` behind it would otherwise act unseen), except `?`, whose sheet lists the list's keys. A key typed
 *  into a text field is the field's. While `covered` (another of the panel's overlays owns the keys) the list takes
 *  none, and while one of the window's questions waits its own keys are the question's and the rest still go
 *  nowhere; Escape with a code or a warning up is theirs too. A held `a`, `x` or `r` acts once. */
export function DeviceList({
  desk,
  covered = false,
}: {
  desk: RemoteDesk;
  covered?: boolean;
  now?: () => number;
}) {
  const { listOpen, devices, listNote } = useSyncExternalStore(desk.subscribe, desk.snapshot);
  const [selected, setSelected] = useState(0);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [recipe, setRecipe] = useState<RemoteRecipeChoice>("auto");
  const state = useRef({ selected, expanded, recipe });
  state.current = { selected, expanded, recipe };
  const root = useRef<HTMLDivElement>(null);

  const count = devices?.length ?? 0;
  useEffect(() => {
    if (selected >= count && count > 0) setSelected(count - 1);
  }, [count, selected]);
  useEffect(() => {
    if (!listOpen) {
      setSelected(0);
      setExpanded(null);
    }
  }, [listOpen]);

  const shownAt = Math.min(selected, Math.max(count - 1, 0));
  useEffect(() => {
    // `?.()`: jsdom has no `scrollIntoView`.
    root.current?.querySelector(".device-row.selected")?.scrollIntoView?.({ block: "nearest" });
  }, [shownAt, expanded, devices, listOpen]);

  useEffect(() => {
    if (!listOpen || covered) return;
    function onKey(event: KeyboardEvent) {
      if (isModifierKey(event.key)) return;
      if (event.isComposing || event.keyCode === 229) return;
      if (isEditableElement(event.target)) return;
      const view = desk.snapshot();
      const plain = isPlainAnswerKey(event as unknown as KeyLike);
      const take = () => {
        event.preventDefault();
        event.stopImmediatePropagation();
      };
      // A key the list has no use for: kept from everything behind the list, but not from the other listeners here
      // (the window's y/n counts every key for its typing guard, so a y right after one still answers nothing).
      const swallow = () => {
        event.preventDefault();
        event.stopPropagation();
      };
      // The window's y/n comes first: its keys are its own, and the rest go nowhere while it waits.
      const question = view.questions[0];
      if (question !== undefined) {
        if (event.key === "Escape") return;
        if (!plain) return;
        if (QUESTION_KEYS.has(event.key) || (event.key === "Tab" && tabChangesGrant(question))) return;
        swallow();
        return;
      }
      // A code on show and its warnings close with Escape first.
      if (event.key === "Escape" && (view.pairing !== null || view.flagged.length > 0)) return;
      if (!plain) return;
      const rows = view.devices ?? [];
      const at = Math.min(state.current.selected, Math.max(rows.length - 1, 0));
      switch (event.key) {
        case "j":
        case "ArrowDown":
          take();
          if (rows.length > 0) setSelected(Math.min(at + 1, rows.length - 1));
          return;
        case "k":
        case "ArrowUp":
          take();
          setSelected(Math.max(at - 1, 0));
          return;
        case "Enter": {
          take();
          const id = rows[at]?.id;
          if (id !== undefined) setExpanded(state.current.expanded === id ? null : id);
          return;
        }
        case "a":
          take();
          if (!event.repeat) desk.startEnroll(state.current.recipe);
          return;
        case "Tab":
          take();
          setRecipe(RECIPES[(RECIPES.indexOf(state.current.recipe) + 1) % RECIPES.length]);
          return;
        case "x": {
          take();
          const id = rows[at]?.id;
          if (id !== undefined && !event.repeat) desk.askRevoke(id);
          return;
        }
        case "r":
          take();
          if (!event.repeat) desk.refreshList();
          return;
        case "Escape":
        case "q":
          take();
          desk.closeList();
          return;
        case "?":
          // The keys sheet, which covers the list while it is open.
          return;
        default:
          swallow();
          return;
      }
    }
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [desk, listOpen, covered]);

  if (!listOpen) return null;
  return (
    <div ref={root} className="remote-desk device-list" role="dialog" aria-label="phones (remote control)">
      <div className="device-list-title">Phones (remote control)</div>
      {devices === null ? (
        <div className="remote-hint">reading the device list…</div>
      ) : devices.length === 0 ? (
        <div className="remote-hint">no device is enrolled</div>
      ) : (
        <ul className="device-rows">
          {devices.map((device, i) => (
            <Row device={device} selected={i === shownAt} expanded={expanded === device.id} key={device.id} />
          ))}
        </ul>
      )}
      {listNote !== null && <div className="remote-refusal">{listNote}</div>}
      <div className="remote-hint">
        a adds a phone ({recipeWords(recipe)}; Tab changes) · x revokes · Enter shows records · r reads again · Esc
        closes
      </div>
      <div className="remote-hint">{DEVICE_LIST_NOTE}</div>
    </div>
  );
}
