import { readFileSync } from "node:fs";
import ts from "typescript";
import { expect, it } from "vitest";

it("the_bundle_installs_the_frozen_global_and_its_declarations_compile", () => {
  const bundle = readFileSync("dist/agents.js", "utf8");
  expect(bundle.startsWith('"use strict";\n(() => {')).toBe(true);
  expect(bundle).toContain("eitriAgents");
  expect(bundle).toContain("Object.freeze");
  const program = ts.createProgram(["dist/agents.d.ts", "../keys/src/index.ts"], {
    noEmit: true, strict: true, target: ts.ScriptTarget.ES2020, skipLibCheck: false,
    moduleResolution: ts.ModuleResolutionKind.Bundler, module: ts.ModuleKind.ESNext,
  });
  expect(ts.getPreEmitDiagnostics(program).map(error => ts.flattenDiagnosticMessageText(error.messageText, "\n")))
    .toEqual([]);
});
