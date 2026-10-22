/** The function a CommonJS package compiled from ES modules exports as `default`. Bundlers unwrap
 *  `{ default, __esModule }`; Node's ES module loader (how the tests load an external dependency) hands the
 *  wrapper itself over, so each layer of `default` is taken off until a non-object remains. */
export function cjsDefault<T>(mod: unknown): T {
  let value: unknown = mod;
  while (value !== null && typeof value === "object" && "default" in value) value = (value as { default: unknown }).default;
  return value as T;
}
