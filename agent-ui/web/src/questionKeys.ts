/*! Question key mapping adapted from opencode, Copyright (c) 2025 opencode, MIT License. */

import { isPlainAnswerKey } from "./keymap";

export type QuestionKeyZone = "option" | "custom" | "input" | "other";
export type QuestionKeyIntent =
  | { type: "none" | "swallow" | "dismiss" | "advance" | "activate" | "commit" | "type" | "edit" | "notes" | "leave" }
  | { type: "question" | "move"; step: 1 | -1 }
  | { type: "edge"; to: "first" | "last" }
  | { type: "number"; index: number };

type Key = Pick<KeyboardEvent, "key" | "keyCode" | "repeat" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey" | "isComposing"> & { getModifierState?: (key: string) => boolean };

/** Answering is a key-down action. Text fields retain their editing keys and IME composition. */
export function questionKey(event: Key, context: { zone: QuestionKeyZone; count: number; preview?: boolean }): QuestionKeyIntent {
  if (!isPlainAnswerKey({ key: event.key, ctrlKey: false, shiftKey: event.shiftKey,
    altKey: event.altKey, metaKey: event.metaKey, isComposing: event.isComposing, keyCode: event.keyCode,
    getModifierState: event.getModifierState?.bind(event) })) return { type: "none" };
  if (event.repeat) {
    const editing = context.zone === "input" && !event.ctrlKey && !event.metaKey && !event.altKey &&
      (event.key.length === 1 || ["ArrowLeft", "ArrowRight", "ArrowDown", "Backspace", "Delete", "Home", "End"].includes(event.key));
    return { type: editing ? "none" : "swallow" };
  }
  if (event.metaKey || event.altKey) return { type: "none" };
  if (event.ctrlKey) {
    if (event.key === "Enter") return { type: "advance" };
    if (event.key.toLowerCase() === "g" && (context.zone === "custom" || context.zone === "input")) return { type: "edit" };
    return { type: "none" };
  }
  if (event.key === "Escape") return { type: context.zone === "input" ? "leave" : "dismiss" };
  if (event.key === "Tab") return { type: "question", step: event.shiftKey ? -1 : 1 };
  if (context.zone === "input") {
    if (event.key === "ArrowUp" && !event.shiftKey) return { type: "leave" };
    if (event.key === "Enter" && !event.shiftKey) return { type: "commit" };
    return { type: "none" };
  }
  if (event.shiftKey) return { type: context.zone === "custom" && event.key.length === 1 && event.key !== " " ? "type" : "none" };
  if (/^[1-9]$/.test(event.key)) {
    const index = Number(event.key) - 1;
    return index < context.count ? { type: "number", index } : { type: "swallow" };
  }
  if (event.key === "n" && context.preview) return { type: "notes" };
  if (["ArrowLeft", "h", "ArrowRight", "l"].includes(event.key)) return { type: "question", step: ["ArrowLeft", "h"].includes(event.key) ? -1 : 1 };
  if (context.zone !== "other") {
    if (["ArrowUp", "k", "ArrowDown", "j"].includes(event.key)) return { type: "move", step: ["ArrowUp", "k"].includes(event.key) ? -1 : 1 };
    if (event.key === "Home" || event.key === "End") return { type: "edge", to: event.key === "Home" ? "first" : "last" };
    if (event.key === " ") return { type: "activate" };
    if (event.key === "Enter") return { type: "commit" };
    if (context.zone === "custom" && event.key.length === 1) return { type: "type" };
  }
  return { type: "none" };
}
