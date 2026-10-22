import { onRemove } from "./bridge";
export type FindCase = "smart" | "ignore" | "match";
export type KitSettings = {
  scrollStep: number;
  smooth: boolean;
  prefixWait: boolean;
  whichKeyDelay: number | null;
  hintChars: string;
  findCase: FindCase;
};

export const DEFAULT_SETTINGS: KitSettings = {
  scrollStep: 60, smooth: true, prefixWait: true, whichKeyDelay: 200, hintChars: "sadfjklewcmpgh", findCase: "smart",
};

export function settingsOf(wire: unknown): KitSettings {
  const settings = { ...DEFAULT_SETTINGS };
  if (!wire || typeof wire !== "object") return settings;
  const prototype = Object.getPrototypeOf(wire);
  if (prototype !== Object.prototype && prototype !== null) return settings;
  const value = wire as Record<string, unknown>;
  const integer = (field: unknown, min: number, max: number): field is number =>
    typeof field === "number" && Number.isInteger(field) && field >= min && field <= max;
  if (integer(value.scroll_step, 10, 400)) settings.scrollStep = value.scroll_step;
  if (typeof value.smooth === "boolean") settings.smooth = value.smooth;
  if (typeof value.prefix_wait === "boolean") settings.prefixWait = value.prefix_wait;
  if (value.which_key_delay === null || integer(value.which_key_delay, 0, 5000)) {
    settings.whichKeyDelay = value.which_key_delay;
  }
  if (typeof value.hint_chars === "string" && /^[a-z]{2,26}$/.test(value.hint_chars) &&
    new Set(value.hint_chars).size === value.hint_chars.length) settings.hintChars = value.hint_chars;
  if (value.find_case === "smart" || value.find_case === "ignore" || value.find_case === "match") {
    settings.findCase = value.find_case;
  }
  return settings;
}

let settings = DEFAULT_SETTINGS;

onRemove(() => { settings = DEFAULT_SETTINGS; });

export function useSettings(next: KitSettings): void {
  settings = next;
}

export function currentSettings(): KitSettings {
  return settings;
}

export function prefixKey(label: string | undefined | null): string | null {
  const match = typeof label === "string" ? /^Ctrl\+([a-z])$/.exec(label) : null;
  return match ? `<C-${match[1]}>` : null;
}
