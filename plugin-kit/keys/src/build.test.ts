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
      import type { Row, InstallOptions, KeyStroke, KitLabels, GenericAction, VisualKind } from "./standalone-keys";
      const row: Row = {action: "test.action", keys: ["x"], context: "page", desc: "test"};
      const labels: KitLabels = {modes: {normal: "N"}, page: "page", generic: {"scroll.down": "down"},
        sheetTitle: "Keys", sheetFooter: "Esc closes", sheetClose: "Close"};
      const opts: InstallOptions = {rows: [row], run: () => true, labels};
      const v3: InstallOptions = { rows: [row], run: () => true, initial: { timeoutlen: 1000,
        hasKeys: true, prefix: "Ctrl+b", settings: { scroll_step: 40 } },
        find: { root: () => document.querySelector("main"), exclude: ".chrome" },
        labels: { messages: { "find.count": "{index}/{total}" } } };
      window.eitriKeys.findRefresh();
      window.eitriKeys.findSeed("query");
      const stroke: KeyStroke = {key: "x", ctrl: false, shift: false};
      const action: GenericAction = "hint.start";
      const version: 5 = window.eitriKeys.version;
      const v4: InstallOptions = { rows: [row], run: () => true, bottom: () => null, onJump: (when) => void when };
      const v5: InstallOptions = { rows: [row], run: () => true, visual: { start: () => null,
        follow: (rect) => void rect.top, keys: { "<CR>": (kind: VisualKind) => kind === "line" } } };
      window.eitriKeys.install(v5);
      const visual: VisualKind | null = window.eitriKeys.visualMode();
      window.eitriKeys.endVisual();
      const generic: GenericAction = "visual.line";
      void [visual, generic];
      window.eitriKeys.install(opts);
      window.eitriKeys.install(v3);
      window.eitriKeys.install(v4);
      window.eitriKeys.scrollTo(window.eitriKeys.destination());
      window.eitriKeys.settle();
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


it("every_module_with_mutable_state_registers_a_reset", () => {
  const missing = ts.sys.readDirectory("src", [".ts"], undefined, ["*.ts"])
    .filter((file) => !file.endsWith(".test.ts"))
    .filter((file) => {
      const source = ts.createSourceFile(file, ts.sys.readFile(file)!, ts.ScriptTarget.Latest, true);
      const mutable = source.statements.some((statement) => ts.isVariableStatement(statement) &&
        ((statement.declarationList.flags & ts.NodeFlags.Let) !== 0 ||
          statement.declarationList.declarations.some(({ initializer }) => initializer &&
            ts.isNewExpression(initializer) && ts.isIdentifier(initializer.expression) &&
            ["Set", "Map"].includes(initializer.expression.text))));
      let registered = false;
      const visit = (node: ts.Node) => {
        if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === "onRemove") {
          registered = true;
        }
        ts.forEachChild(node, visit);
      };
      visit(source);
      return mutable && !registered;
    });
  expect(missing).toEqual([]);
});
