import { describe, expect, it } from "vitest";
import { classify, expectedName, validName, wordAt } from "./gdTarget";

describe("gd targets", () => {
  it("classifies files, lines and ranges with gf's grammar", () => {
    expect(classify("mesh.go:142", "")).toEqual({ kind: "pathline", path: "mesh.go", line: 142 });
    expect(classify("mesh.go:10-20", "")).toEqual({ kind: "pathline", path: "mesh.go", line: 10, endLine: 20 });
    expect(classify("src/mesh.go", "")).toEqual({ kind: "path", path: "src/mesh.go" });
    expect(classify("", "")).toEqual({ kind: "none" });
  });
  it.each(["README.md", "App.tsx", "Cargo.toml", "CLAUDE.md", "Main.go"])("reads the bare file name %s as a path", (file) => {
    expect(classify(file, "")).toEqual({ kind: "path", path: file });
    expect(wordAt(`see ${file} here`, 6)).toBe(file);
  });
  it.each(["TestMeshFieldsOmittedWhenZero", "pkg.Name", "Type::name", "M.call", "Mutex.lock", "Vec.new"])("reads %s as a name", (name) => {
    expect(classify(name, "")).toEqual({ kind: "name", name, cited: null });
    expect(classify(name, `${name} mesh.go:142 other.go:10`)).toEqual({ kind: "name", name, cited: { path: "mesh.go", line: 142 } });
  });
  it("reads whole caret words and trims punctuation", () => {
    expect(wordAt("use Type::name.: here", 8)).toBe("Type::name");
    expect(wordAt("use src/mesh.go:142 here", 10)).toBe("src/mesh.go:142");
    expect(wordAt("use M.call here", 7)).toBe("M.call");
    expect(wordAt("hello world", 5)).toBe("");
  });
  it("checks names in UTF-8 bytes and keeps unsafe selections available for a refusal", () => {
    for (const name of ["two words", 'a"b', "a".repeat(201), "é".repeat(101), ""]) expect(validName(name)).toBe(false);
    for (const name of ["a".repeat(200), "é".repeat(100), "名字", "$x_y-z.a::b"]) expect(validName(name)).toBe(true);
    // Marks that spell a letter count, as Rust's `char::is_alphanumeric` counts them; format characters never do.
    expect(validName("नाम")).toBe(true);
    for (const name of ["a\u200bb", "a\u202eb"]) expect(validName(name)).toBe(false);
    expect(classify("two words", "")).toEqual({ kind: "name", name: "two words", cited: null });
  });
  it("takes an inline code name beside a cited path", () => {
    expect(expectedName("`mesh.go:142` has `serve_replay`", "mesh.go")).toBe("serve_replay");
    expect(expectedName("`mesh.go:142` has `pkg.Name`", "mesh.go")).toBe("pkg.Name");
    expect(expectedName("`serve_replay` at `mesh.go:142`", "mesh.go")).toBe("serve_replay");
    expect(expectedName("`Old` then `mesh.go:142` has `New`", "mesh.go")).toBe("New");
    expect(expectedName("mesh.go:142 has `Name`", "mesh.go")).toBe("Name");
    expect(expectedName("`mesh.go:142` only", "mesh.go")).toBeUndefined();
  });
  it("takes no span that is a path, a number or a flag, and none past another span", () => {
    expect(expectedName("`a.go:10` calls into `b.go`", "a.go")).toBeUndefined();
    expect(expectedName("`a.go:1` and `b.go:2` define `Foo`", "a.go")).toBeUndefined();
    expect(expectedName("`mesh.go:142` returns `42`", "mesh.go")).toBeUndefined();
    expect(expectedName("`mesh.go:142` takes `-v`", "mesh.go")).toBeUndefined();
    expect(expectedName("`mesh.go:142` has `README.md`", "mesh.go")).toBeUndefined();
  });
});
