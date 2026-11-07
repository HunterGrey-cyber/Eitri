import type { Transport } from "./index";

export interface AndroidHost {
  postMessage(json: string): void;
  onmessage?: ((event: { data: unknown }) => void) | null;
}

declare global {
  interface Window {
    eitriHost?: AndroidHost;
  }
}

export function androidTransport(host: AndroidHost): Transport {
  let counter = 0;
  return {
    name: "android",
    post(json) {
      host.postMessage(json);
    },
    listen(onMessage) {
      host.onmessage = (event) => {
        if (typeof event.data === "string") onMessage(event.data);
      };
    },
    nextId(kind) {
      return `a-${kind}-${++counter}`;
    },
  };
}
