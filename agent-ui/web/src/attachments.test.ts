import { createHash } from "node:crypto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { base64Of, importBlob, ImportRefused, imagesBlocking, metaLine, sha256Hex, shownName } from "./attachments";

describe("attachments", () => {
  it("the platform SHA-256 matches known digests across block boundaries", async () => {
    for (const size of [0, 1, 55, 56, 63, 64, 65, 1000, 70_000]) {
      const bytes = new Uint8Array(size).map((_, i) => (i * 31 + 7) % 256);
      const expected = createHash("sha256").update(bytes).digest("hex");
      expect(await sha256Hex(bytes)).toBe(expected);
    }
  });

  it("base64 of a large chunk is whole", () => {
    const bytes = new Uint8Array(262_144).map((_, i) => i % 256);
    expect(base64Of(bytes)).toBe(Buffer.from(bytes).toString("base64"));
  });

  it("names, sizes and what holds Send", () => {
    expect(shownName("😀".repeat(41))).toBe(`${"😀".repeat(40)}…`);
    expect(metaLine({ attachmentId: "a", name: "x", state: "ready", mediaType: "image/jpeg", width: 10, height: 20, bytes: 2_000_000 }))
      .toBe("JPEG · 10×20 · 1.9 MB");
    expect(imagesBlocking([], 1)).toBe("an image is still being prepared");
    expect(imagesBlocking([{ attachmentId: "a", name: "x", state: "failed" }], 0)).toBe("an image could not be used: remove it first");
    expect(imagesBlocking([{ attachmentId: "a", name: "x", state: "ready" }], 0)).toBeNull();
  });

  it("an image over 32 MB is refused before a byte is read or anything is sent", async () => {
    const arrayBuffer = vi.fn();
    const post = vi.fn();
    const huge = { size: 33 * 1024 * 1024 + 1, type: "image/png", arrayBuffer } as unknown as Blob;
    const refused = await importBlob(1, 0, huge, "huge.png", { post }).catch((e: unknown) => e);
    expect(refused).toBeInstanceOf(ImportRefused);
    expect((refused as ImportRefused).stage).toBe("begin");
    expect((refused as ImportRefused).message).toBe("image too large: 34 MB (the limit is 32 MB)");
    expect(arrayBuffer).not.toHaveBeenCalled();
    expect(post).not.toHaveBeenCalled();
  });
});

afterEach(() => vi.unstubAllGlobals());
it.each([{}, { subtle: { digest: () => Promise.reject(new Error("unavailable")) } }])(
  "an unavailable platform digest refuses with a legible reason", async (crypto) => {
    vi.stubGlobal("crypto", crypto);
    await expect(sha256Hex(new Uint8Array([1, 2]))).rejects.toThrow("this page cannot hash images");
  },
);
