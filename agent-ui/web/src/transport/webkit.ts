import type { Transport } from "./index";

declare global {
  interface Window {
    webkit?: { messageHandlers?: { eitriAgent?: { postMessage: (msg: string) => void } } };
    __eitriDispatch?: (json: string) => void;
  }
}

export function webkitTransport(w: Window): Transport {
  const prefix = Array.from(crypto.getRandomValues(new Uint32Array(4)), (word) => word.toString(16).padStart(8, "0")).join("");
  let counter = 0;
  return {
    name: "webkit",
    post(json) {
      const handler = w.webkit?.messageHandlers?.eitriAgent;
      if (!handler) {
        console.warn("agent-ui: no WebKitGTK message handler present, message dropped", json);
        return;
      }
      handler.postMessage(json);
    },
    listen(onMessage) {
      w.__eitriDispatch = onMessage;
    },
    nextId(kind) {
      return `${kind}-${prefix}-${++counter}`;
    },
  };
}
