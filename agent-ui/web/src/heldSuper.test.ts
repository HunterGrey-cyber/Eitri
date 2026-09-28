// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { installHeldSuperTracking, noteKey, reset, superHeld } from "./heldSuper";

/* The shape WebKitGTK 2.52.6 actually delivers (sandbox pass, 2026-09-28, Task 1): the Super key's
   own keydown/keyup, `key: "Super"`, `code: "OSLeft"`, and no modifier flag on any other key. */
describe("heldSuper", () => {
  afterEach(() => reset());

  it("is held between the Super key's own keydown and keyup", () => {
    expect(superHeld()).toBe(false);
    noteKey("keydown", { key: "Super", code: "OSLeft" });
    expect(superHeld()).toBe(true);
    noteKey("keydown", { key: "a", code: "KeyA" });
    expect(superHeld()).toBe(true);
    noteKey("keyup", { key: "Super", code: "OSLeft" });
    expect(superHeld()).toBe(false);
  });

  it("recognises Hyper, OS and a Meta-coded Super key, not Alt or Meta by name alone", () => {
    for (const event of [{ key: "Hyper" }, { key: "OS" }, { key: "Meta", code: "MetaLeft" }, { key: "Unidentified", code: "OSRight" }]) {
      noteKey("keydown", event);
      expect(superHeld()).toBe(true);
      reset();
    }
    noteKey("keydown", { key: "Alt", code: "AltLeft" });
    noteKey("keydown", { key: "Meta", code: "AltRight" });
    expect(superHeld()).toBe(false);
  });

  /* Codex review (2026-09-28): one boolean let releasing right Super clear a left Super still down. */
  it("stays held while either Super key is down", () => {
    noteKey("keydown", { key: "Super", code: "OSLeft" });
    noteKey("keydown", { key: "Super", code: "OSRight" });
    noteKey("keyup", { key: "Super", code: "OSRight" });
    expect(superHeld()).toBe(true);
    noteKey("keyup", { key: "Super", code: "OSLeft" });
    expect(superHeld()).toBe(false);
  });

  /* Codex review (2026-09-28): HINT's own window-capture listener stops a key's propagation while a
     request is pending; a tracker installed after it, or on the document, never saw Super go down. */
  it("sees Super's keydown even when a later window-capture listener stops it", () => {
    const remove = installHeldSuperTracking(document, window);
    const stopper = (event: Event) => event.stopImmediatePropagation();
    window.addEventListener("keydown", stopper, true);
    try {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Super", code: "OSLeft" }));
      expect(superHeld()).toBe(true);
    } finally {
      window.removeEventListener("keydown", stopper, true);
      remove();
    }
  });

  it("forgets a held Super when the window loses focus, since its keyup may never arrive", () => {
    const remove = installHeldSuperTracking(document, window);
    try {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Super", code: "OSLeft" }));
      expect(superHeld()).toBe(true);
      window.dispatchEvent(new Event("blur"));
      expect(superHeld()).toBe(false);
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Super", code: "OSLeft" }));
      document.dispatchEvent(new KeyboardEvent("keyup", { key: "Super", code: "OSLeft" }));
      expect(superHeld()).toBe(false);
    } finally {
      remove();
    }
  });

  it("stops listening, and forgets, once removed", () => {
    const remove = installHeldSuperTracking(document, window);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Super", code: "OSLeft" }));
    remove();
    expect(superHeld()).toBe(false);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Super", code: "OSLeft" }));
    expect(superHeld()).toBe(false);
  });
});
