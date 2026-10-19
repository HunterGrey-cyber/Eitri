import type { Row } from "./index";
export type { Row, Mode } from "./index";

export const GENERIC_ROWS: Row[] = [
  { action: "scroll.down", keys: ["j"], context: "页面", desc: "向下滚动" },
  { action: "scroll.up", keys: ["k"], context: "页面", desc: "向上滚动" },
  { action: "scroll.half_down", keys: ["d"], context: "页面", desc: "向下半页" },
  { action: "scroll.half_up", keys: ["u"], context: "页面", desc: "向上半页" },
  { action: "scroll.top", keys: ["gg"], context: "页面", desc: "页面顶部" },
  { action: "scroll.bottom", keys: ["G"], context: "页面", desc: "页面底部" },
  { action: "history.back", keys: ["H"], context: "页面", desc: "后退" },
  { action: "history.forward", keys: ["L"], context: "页面", desc: "前进" },
  { action: "link.prev", keys: ["[["], context: "页面", desc: "上一页" },
  { action: "link.next", keys: ["]]"], context: "页面", desc: "下一页" },
  { action: "hint.start", keys: ["f"], context: "页面", desc: "提示模式" },
  { action: "sheet.open", keys: ["?"], context: "页面", desc: "按键帮助" },
  { action: "mode.escape", keys: ["<Esc>"], context: "页面", desc: "退出模式" },
];
