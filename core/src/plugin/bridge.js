// The host bridge of a plugin page. It runs at document start, before any of the page's own scripts,
// and is the page's only way to the host: window.eitriPlugin.call(verb, args) and .on(event, fn).
(function () {
  "use strict";
  var handlers = window.webkit && window.webkit.messageHandlers;
  var handler = handlers && handlers.eitriPlugin;
  if (!handler) {
    return;
  }
  var LIMIT = 256 * 1024;
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
      if (new TextEncoder().encode(text).length > LIMIT) {
        reject(hostError("too_large", "a message may be at most 256 KiB"));
        return;
      }
      pending.set(id, { resolve: resolve, reject: reject });
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
