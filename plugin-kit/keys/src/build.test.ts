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
      import type { Row, InstallOptions, KeyStroke, KitLabels, GenericAction } from "./standalone-keys";
      const row: Row = {action: "test.action", keys: ["x"], context: "page", desc: "test"};
      const labels: KitLabels = {modes: {normal: "N"}, page: "page", generic: {"scroll.down": "down"},
        sheetTitle: "Keys", sheetFooter: "Esc closes", sheetClose: "Close"};
      const opts: InstallOptions = {rows: [row], run: () => true, labels};
      const stroke: KeyStroke = {key: "x", ctrl: false, shift: false};
      const action: GenericAction = "hint.start";
      const version: 2 = window.eitriKeys.version;
      window.eitriKeys.install(opts);
      window.eitriKeys.status("s");
      const keys: string[] = window.eitriKeys.keys(action);
      window.eitriKeys.message(stroke.key + keys.join("") + String(version));
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
