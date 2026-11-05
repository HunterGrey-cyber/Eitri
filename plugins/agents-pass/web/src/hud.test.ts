import { afterEach, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { showMessage, showMode } from "../../../../plugin-kit/keys/src/hud";

// The page is one 28 px line; the keys kit's bar is a fixed 20 px band along the bottom of whatever page it is in.
const here = dirname(fileURLToPath(import.meta.url));
const PASS_CSS = readFileSync(resolve(here, "pass.css"), "utf8");

afterEach(() => {
  showMessage("");
  showMode("normal", "");
  document.head.replaceChildren();
  document.body.replaceChildren();
});

function page(): HTMLElement {
  // The page's own sheet comes first, as `index.html` links it; the kit adds its sheet later, when it first draws.
  const sheet = document.createElement("style");
  sheet.textContent = PASS_CSS;
  document.head.append(sheet);
  document.body.innerHTML = '<div id="pass" class="pass"><span class="lead">round 1/3</span></div>';
  showMode("normal", "");
  return document.getElementById("eitri-keys-hud")!;
}
const shows = (el: Element | null) => el !== null && getComputedStyle(el).display !== "none";

it("the_keys_bar_does_not_cover_the_line", () => {
  const hud = page();
  expect(document.getElementById("eitri-keys-style")).not.toBeNull();
  const style = getComputedStyle(hud);
  // On the line's right end, as tall as the line, never a band along the bottom across it.
  expect(style.top).toMatch(/^0(px)?$/);
  expect(style.bottom).toMatch(/^0(px)?$/);
  expect(style.left).toBe("auto");
  expect(style.right).toMatch(/^0(px)?$/);
  // With nothing to say in normal mode it takes no room: no mode word, no empty message cell.
  expect(shows(hud.querySelector(".eitri-keys-mode"))).toBe(false);
  expect(shows(hud.querySelector(".eitri-keys-message"))).toBe(false);
  expect(style.paddingLeft).toMatch(/^0(px)?$/);
  expect(style.paddingRight).toMatch(/^0(px)?$/);
});

it("the_keys_bar_still_says_what_it_has_to", () => {
  const hud = page();
  showMessage("no_window");
  expect(shows(hud.querySelector(".eitri-keys-message"))).toBe(true);
  expect(hud.querySelector(".eitri-keys-message")!.textContent).toBe("no_window");
  showMode("hint", "a");
  expect(shows(hud.querySelector(".eitri-keys-mode"))).toBe(true);
  expect(hud.querySelector(".eitri-keys-mode")!.textContent).toBe("HINT");
  expect(shows(hud.querySelector(".eitri-keys-pending"))).toBe(true);
});
