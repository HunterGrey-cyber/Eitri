const host = window.eitriPlugin;
const head = document.getElementById("head");
const list = document.getElementById("list");
const rows = new Map(); // by the tab's key, which stays when the tab moves to another window
let picked = 0;

const slot = (row) => row.key || row.id;
const waiting = () => [...rows.values()]
  .filter((row) => row.class === "needs")
  .sort((a, b) => (a.waiting_since ?? 0) - (b.waiting_since ?? 0));

function draw() {
  const shown = waiting();
  picked = Math.max(0, Math.min(picked, shown.length - 1));
  head.textContent = shown.length ? `${shown.length} waiting, oldest first` : "no agent waits for you";
  list.replaceChildren(...shown.map((row, at) => {
    const item = document.createElement("li");
    item.textContent = `${row.project.name} ${row.window.name}·${row.number}  ${row.state}  ${row.label}`;
    item.classList.toggle("picked", at === picked);
    item.addEventListener("click", () => { picked = at; goToPicked(); });
    return item;
  }));
}

function goToPicked() {
  const row = waiting()[picked];
  if (row) host.call("agents.focus", { id: row.id, card_first: true }).catch((e) => window.eitriKeys.message(e.message));
}

function load(view) {
  rows.clear();
  for (const row of view.rows) rows.set(slot(row), row);
  draw();
}

host.on("agents.row", (row) => { rows.set(slot(row), row); draw(); });
host.on("agents.gone", (gone) => { if (rows.get(gone.key || gone.id)?.id === gone.id) rows.delete(gone.key || gone.id); draw(); });
host.on("agents.reset", load);

async function start() {
  const hello = await host.call("host.hello");
  await window.eitriKeys.install({
    rows: [
      { action: "waiting.down", keys: ["j"], context: "Who waits", desc: "next agent" },
      { action: "waiting.up", keys: ["k"], context: "Who waits", desc: "previous agent" },
      { action: "waiting.open", keys: ["<CR>"], context: "Who waits", desc: "go to its chat, card first" },
      { action: "waiting.back", keys: ["q"], context: "Who waits", desc: "back to the editor" },
    ],
    run(action) {
      if (action === "waiting.down") { picked += 1; draw(); return true; }
      if (action === "waiting.up") { picked -= 1; draw(); return true; }
      if (action === "waiting.open") { goToPicked(); return true; }
      if (action === "waiting.back") { host.call("layout.focus", { target: "editor" }); return true; }
      return false;
    },
    initial: { timeoutlen: hello.timeoutlen, hasKeys: true, prefix: hello.prefix, settings: hello.keys_settings },
  });
  try {
    load(await host.call("agents.watch"));
  } catch (e) {
    head.textContent = e.code === "disabled" ? "agents are off in this window" : e.message;
  }
}

start();
