import { describe, expect, it } from "vitest";
import { projectRelative } from "./projectPath";

describe("projectRelative (v1 polish F21)", () => {
  it("shortens a path under the root and leaves every other path as sent", () => {
    expect(projectRelative("/home/u/proj/src/main.rs", "/home/u/proj")).toBe("src/main.rs");
    expect(projectRelative("/home/u/proj/src/main.rs", "/home/u/proj/")).toBe("src/main.rs");
    expect(projectRelative("/home/u/proj", "/home/u/proj")).toBe(".");
    expect(projectRelative("/home/u/project/a.rs", "/home/u/proj")).toBe("/home/u/project/a.rs");
    expect(projectRelative("/etc/hostname", "/home/u/proj")).toBe("/etc/hostname");
    expect(projectRelative("src/main.rs", "/home/u/proj")).toBe("src/main.rs");
    expect(projectRelative("/home/u/proj/a.rs", "")).toBe("/home/u/proj/a.rs");
  });
});
