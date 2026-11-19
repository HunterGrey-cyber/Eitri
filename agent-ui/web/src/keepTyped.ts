/** The draft a box keeps when the host hands its tab back with a draft of its own (a refused restore: the queued
 *  messages, then the host's copy of the box). The host's copy trails the box by the mirror's debounce, so the box
 *  wins. `held` lists what the host may have had of the box, newest first (the mirrors still in flight and the last
 *  one it answered): the first that ends the host's text is that copy, and it is replaced by `typed`, so a deletion
 *  made since stays deleted and text already queued is not kept twice. With none of them there, the box is added
 *  after the host's text. */
export function keepTyped(host: string, held: readonly string[], typed: string): string {
  const copy = held.find((text) => text !== "" && host.endsWith(text));
  if (copy !== undefined) return tidy(host.slice(0, host.length - copy.length) + typed);
  if (typed.trim() === "") return host;
  if (host.trim() === "") return typed;
  return `${host}\n\n${typed}`;
}

/** No separator left dangling where the replaced copy was the last part. */
function tidy(text: string): string {
  return text.replace(/\n+$/, "");
}
