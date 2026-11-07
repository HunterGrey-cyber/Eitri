import { androidTransport } from "./android";
import { webkitTransport } from "./webkit";

export interface Transport {
  readonly name: "webkit" | "android";
  post(json: string): void;
  listen(onMessage: (json: string) => void): void;
  nextId(kind: "req" | "mirror"): string;
}

export function detectTransport(w: Window): Transport {
  const host = w.eitriHost;
  return host !== null && typeof host === "object" && typeof host.postMessage === "function"
    ? androidTransport(host)
    : webkitTransport(w);
}
