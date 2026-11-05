const rows = new Map();
const el = document.getElementById("n");
const waiting = () => [...rows.values()].filter((r) => r.class === "needs");
const draw = () => {
  const w = waiting();
  el.textContent = w.length ? `⚑ ${w.length} waiting` : "no one waits";
};
el.onclick = () => {
  const w = waiting().sort((a, b) => a.waiting_since - b.waiting_since);
  if (w.length) eitriPlugin.call("agents.round", { op: "start", key: w[0].key });
};
const load = (view) => { rows.clear(); view.rows.forEach((r) => rows.set(r.id, r)); draw(); };
eitriPlugin.on("agents.row", (r) => { rows.set(r.id, r); draw(); });
eitriPlugin.on("agents.gone", (g) => { rows.delete(g.id); draw(); });
eitriPlugin.on("agents.reset", load);
eitriPlugin.call("host.hello", {})
  .then(() => eitriPlugin.call("agents.watch"))
  .then(load)
  .catch((e) => { el.textContent = e.code === "disabled" ? "agents are off" : String(e); });
