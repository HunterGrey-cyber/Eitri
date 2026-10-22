// The host bridge of a plugin page. It runs at document start, before any of the page's own scripts,
// and is the page's only way to the host: window.eitriPlugin.call(verb, args) and .on(event, fn).
// It also keeps the host's theme on the document root as --nv-* custom properties.
(function () {
  "use strict";
  var handlers = window.webkit && window.webkit.messageHandlers;
  var handler = handlers && handlers.eitriPlugin;
  if (!handler) {
    return;
  }
  var LIMIT = 256 * 1024;
  // A saved state may itself be 256 KiB, so its message gets room for what wraps it; the host checks the state.
  var SAVED_LIMIT = LIMIT + 1024;
  var pending = new Map();
  var listeners = new Map();
  // A reply may run in a different document after navigation, including a cached page's restore.
  // Give each document a random id base so such a reply is unlikely to match a call waiting here.
  var next = 1 + Math.floor(Math.random() * 0x100000) * 0x100000000;

  function hostError(code, message) {
    var e = new Error(message);
    e.code = code;
    return e;
  }

  // The theme the host hands every page: the user's colours and font lists, set as custom properties on the
  // root so a page reads them with var(). Each value is checked again here with the host's own rule.
  var THEME_NAME = /^--nv-[a-z0-9]+(?:-[a-z0-9]+)*$/u;
  var THEME_COLOUR = /^#[0-9a-f]{6}$/u;
  var THEME_FONTS = /^(?:"[^"\x5C;{}<>()[\]/*\u{0}-\u{1F}\u{7F}-\u{9F}]{1,100}"|[A-Za-z][A-Za-z0-9-]{0,39})(?:, ?(?:"[^"\x5C;{}<>()[\]/*\u{0}-\u{1F}\u{7F}-\u{9F}]{1,100}"|[A-Za-z][A-Za-z0-9-]{0,39})){0,15}$/u;
  var themeNames = new Set();

  function themeValueOk(name, value) {
    if (typeof name !== "string" || typeof value !== "string") return false;
    if (Array.from(name).length > 64 || !THEME_NAME.test(name)) return false;
    if (name === "--nv-color-scheme") return value === "light" || value === "dark";
    if (name.indexOf("--nv-font-") === 0) return Array.from(value).length <= 1024 && THEME_FONTS.test(value);
    return THEME_COLOUR.test(value);
  }

  function applyTheme(theme) {
    try {
      var root = document.documentElement;
      if (!root || !theme || typeof theme !== "object" || !theme.vars || typeof theme.vars !== "object" ||
        Array.isArray(theme.vars)) {
        return;
      }
      var vars = theme.vars;
      var now = new Set();
      var skipped = 0;
      Object.keys(vars).forEach(function (name) {
        if (!themeValueOk(name, vars[name])) {
          skipped++;
          return;
        }
        root.style.setProperty(name, vars[name]);
        now.add(name);
      });
      themeNames.forEach(function (name) {
        if (!now.has(name)) {
          root.style.removeProperty(name);
        }
      });
      themeNames = now;
      if (skipped) {
        console.warn("eitri: skipped " + skipped + " theme value(s) that failed the check");
      }
    } catch (e) {
      console.error(e);
    }
  }

  function dispatch(text) {
    var msg;
    try {
      msg = JSON.parse(text);
    } catch (_) {
      return;
    }
    if (!msg || msg.v !== 1) {
      return;
    }
    if (typeof msg.re === "number") {
      var waiting = pending.get(msg.re);
      if (!waiting) {
        return;
      }
      pending.delete(msg.re);
      if (msg.ok === true) {
        if (waiting.verb === "host.hello" && msg.result && typeof msg.result === "object") {
          applyTheme(msg.result.theme);
        }
        waiting.resolve(msg.result);
      } else {
        var err = msg.error || {};
        waiting.reject(
          hostError(
            typeof err.code === "string" ? err.code : "bad_request",
            typeof err.message === "string" ? err.message : "the host refused the call"
          )
        );
      }
      return;
    }
    if (typeof msg.event === "string") {
      if (msg.event === "theme") {
        applyTheme(msg.data);
      }
      var set = listeners.get(msg.event);
      if (!set) {
        return;
      }
      Array.from(set).forEach(function (fn) {
        try {
          fn(msg.data);
        } catch (e) {
          console.error(e);
        }
      });
    }
  }

  function call(verb, args) {
    return new Promise(function (resolve, reject) {
      var id = next++;
      var text;
      try {
        text = JSON.stringify({ v: 1, id: id, verb: verb, args: args === undefined ? {} : args });
      } catch (e) {
        reject(e);
        return;
      }
      // The host counts UTF-8 bytes.
      var saved = verb === "page.saved";
      if (new TextEncoder().encode(text).length > (saved ? SAVED_LIMIT : LIMIT)) {
        reject(
          hostError("too_large", saved ? "a saved state may be at most 256 KiB" : "a message may be at most 256 KiB")
        );
        return;
      }
      // No timer here: the host answers every call, at the latest with an error at the call's deadline (the
      // editor, a path check and the store each have one), so the promise settles even when the work behind
      // it never finishes. The one exception is a call made before the host started a new document (a reload,
      // a restore from the page cache): the host drops its answer.
      pending.set(id, { resolve: resolve, reject: reject, verb: verb });
      handler.postMessage(text);
    });
  }

  function on(event, fn) {
    var set = listeners.get(event);
    if (!set) {
      set = new Set();
      listeners.set(event, set);
    }
    set.add(fn);
    return function () {
      set.delete(fn);
    };
  }

  Object.defineProperty(window, "__eitriPluginDispatch", { value: dispatch, writable: false, configurable: false });
  Object.defineProperty(window, "eitriPlugin", {
    value: Object.freeze({ protocol: 1, call: call, on: on }),
    writable: false,
    configurable: false,
  });
})();
