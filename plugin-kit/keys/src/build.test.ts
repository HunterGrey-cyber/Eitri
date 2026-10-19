import ts from "typescript";
import { expect, it } from "vitest";

it("emits a keys declaration that can be copied without sibling modules", () => {
  const config = ts.readConfigFile("tsconfig.json", ts.sys.readFile);
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, process.cwd());
  const program = ts.createProgram(parsed.fileNames, { ...parsed.options, emitDeclarationOnly: true });
  expect(ts.getPreEmitDiagnostics(program).map((error) => ts.flattenDiagnosticMessageText(error.messageText, "\n")))
    .toEqual([]);
  let declaration = "";
  program.emit(undefined, (file, text) => { if (file.endsWith("/index.d.ts")) declaration = text; });
  expect(declaration).not.toBe("");

  const root = process.cwd();
  const files = new Map([
    [`${root}/standalone-keys.d.ts`, declaration],
    [`${root}/consumer.ts`, `
      import type { Row, InstallOptions, KeyStroke } from "./standalone-keys";
      const row: Row = {action: "test.action", keys: ["x"], context: "页面", desc: "测试"};
      const opts: InstallOptions = {rows: [row], run: () => true};
      const stroke: KeyStroke = {key: "x", ctrl: false, shift: false};
      window.eitriKeys.install(opts);
      window.eitriKeys.message(stroke.key);
    `],
  ]);
  const options = { ...parsed.options, noEmit: true, skipLibCheck: false, rootDir: root };
  const host = ts.createCompilerHost(options);
  const original = host.getSourceFile.bind(host);
  host.fileExists = (file) => files.has(file) || ts.sys.fileExists(file);
  host.getSourceFile = (file, version, onError, fresh) => files.has(file)
    ? ts.createSourceFile(file, files.get(file)!, version) : original(file, version, onError, fresh);
  const consumer = ts.createProgram([...files.keys()], options, host);
  expect(ts.getPreEmitDiagnostics(consumer).map((error) => ts.flattenDiagnosticMessageText(error.messageText, "\n")))
    .toEqual([]);
});
