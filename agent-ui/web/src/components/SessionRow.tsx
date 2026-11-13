/** Shows the first 8 characters of a provider session id. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}
