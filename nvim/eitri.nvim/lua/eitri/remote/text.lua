-- Text that came from outside (a host's name, a path, a server's or a program's words) as eitri.nvim shows it: every
-- character that could move the cursor, retitle the terminal, end the line or make a name read as another is written
-- out as `\u{<hex>}`, the way Rust's `char::escape_unicode` writes it.
local M = {}

-- Besides C0 and DEL, by their UTF-8 bytes: C1 (U+0080-U+009F), the Arabic letter mark (U+061C), the zero-width spaces
-- and joiners and the direction marks (U+200B-U+200F), the line and paragraph separators and the embeddings and
-- overrides (U+2028-U+202E), the isolates (U+2066-U+2069) and the byte-order mark (U+FEFF). That is the set the Rust
-- side escapes (`remote_sync::terminal_text`, through `agent_bridge::is_control_like`) and three more zero-width
-- characters with the byte-order mark: they do nothing on a terminal, and they still hide inside a name.
local MULTIBYTE = {
  "\194[\128-\159]",
  "\216\156",
  "\226\128[\139-\143\168-\174]",
  "\226\129[\166-\169]",
  "\239\187\191",
}

-- The code point of one UTF-8 sequence of two or three bytes (all the patterns above match).
local function codepoint(sequence)
  local first, second, third = sequence:byte(1, 3)
  if third then
    return (first % 16) * 4096 + (second % 64) * 64 + (third % 64)
  end
  return (first % 32) * 64 + (second % 64)
end

local function escaped(value)
  return ("\\u{%x}"):format(value)
end

--- `text` with every hidden character written out as `\u{<hex>}`. Pure Lua: safe in a callback that may not call
--- `vim.fn`.
function M.escape(text)
  text = tostring(text):gsub("[%z\1-\31\127]", function(c)
    return escaped(c:byte())
  end)
  for _, pattern in ipairs(MULTIBYTE) do
    text = text:gsub(pattern, function(c)
      return escaped(codepoint(c))
    end)
  end
  return text
end

return M
