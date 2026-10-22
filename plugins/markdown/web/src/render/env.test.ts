import { describe, expect, it } from "vitest";
import { mark, newNonce, NONCE_ATTR } from "./env";

describe("the render environment", () => {
  it("makes a fresh 16-byte hex nonce each time", () => {
    const a = newNonce();
    const b = newNonce();
    expect(a).toMatch(/^[0-9a-f]{32}$/);
    expect(b).toMatch(/^[0-9a-f]{32}$/);
    expect(a).not.toBe(b);
  });
  it("marks an element with the nonce attribute", () => {
    expect(NONCE_ATTR).toBe("data-eitri-n");
    expect(mark({ nonce: "ab" })).toBe(' data-eitri-n="ab"');
  });
});
