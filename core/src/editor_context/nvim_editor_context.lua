-- neovibe editor-context feed (wire 1). Loaded by `shell` with `--cmd`, before the user's own
-- config, so it only installs autocommands and a timer; it changes no setting and no keymap.
--
-- It writes one JSON line per update to NEOVIBE_EDITOR_SOCKET: which file is focused, where the
-- cursor is, and -- only while a visual selection is actually live -- the selected lines and their
-- text.

local socket = vim.env.NEOVIBE_EDITOR_SOCKET
if not socket or socket == "" then
  return
end

-- Send at most this often. CursorMoved fires on every cursor motion, so a write per event means a
-- connect/write/close per keypress while a key repeats -- the theme feed has no equivalent problem
-- because ColorScheme is rare. A dirty flag plus one repeating timer bounds the rate no matter how
-- fast the user types, and 150ms is far below the time it takes to move a hand to the panel.
local INTERVAL_MS = 150

-- Never fetch more than this many lines of a selection. The host truncates the text at 2000
-- characters anyway, so fetching a 100,000-line selection would be work done only to be thrown
-- away -- and measured at ~33ms on nvim's own loop, which is where the editor's input latency
-- lives. The reported line range is always the TRUE one; only the text is bounded.
local MAX_SELECTION_LINES = 400

local dirty = true

local function selection()
  local mode = vim.fn.mode()
  -- Visual (v), visual-line (V) and visual-block (\22) only. `'<` and `'>` are deliberately not
  -- consulted: they are a side effect of LEAVING visual mode, and in neovibe the user never leaves
  -- it -- they click the WebView, and nvim is never told it lost focus. Measured: the marks stay
  -- [0,0,0,0] for the whole life of a live selection.
  if mode ~= "v" and mode ~= "V" and mode ~= "\22" then
    return nil
  end
  local anchor = vim.fn.getpos("v")
  local cursor = vim.fn.getpos(".")
  local first, last = anchor, cursor
  if first[2] > last[2] or (first[2] == last[2] and first[3] > last[3]) then
    first, last = cursor, anchor
  end
  local fetch_last = { last[1], last[2], last[3], last[4] }
  if fetch_last[2] - first[2] >= MAX_SELECTION_LINES then
    fetch_last[2] = first[2] + MAX_SELECTION_LINES - 1
    fetch_last[3] = 2147483647
  end
  local ok, lines = pcall(vim.fn.getregion, first, fetch_last, { type = mode })
  if not ok or type(lines) ~= "table" then
    return nil
  end
  return { start_line = first[2], end_line = last[2], text = table.concat(lines, "\n") }
end

local function send()
  local ok, payload = pcall(function()
    local buf = vim.api.nvim_get_current_buf()
    return vim.json.encode({
      v = 1,
      file = vim.api.nvim_buf_get_name(buf),
      line = vim.fn.line("."),
      selection = selection(),
    })
  end)
  if not ok or not payload then
    return
  end
  -- Blocking connect/write/close, inside the timer callback, exactly as the theme feed does: the
  -- host accepts one connection per line and reads it whole, so the line is normally already in the
  -- socket when it is accepted.
  local pipe = vim.uv.new_pipe(false)
  pipe:connect(socket, function(err)
    if err then
      pipe:close()
      return
    end
    pipe:write(payload .. "\n", function()
      pipe:close()
    end)
  end)
end

local group = vim.api.nvim_create_augroup("NeovibeEditorContext", { clear = true })
vim.api.nvim_create_autocmd(
  { "CursorMoved", "CursorMovedI", "ModeChanged", "BufEnter", "BufFilePost", "VimEnter" },
  {
    group = group,
    callback = function()
      dirty = true
    end,
  }
)

local timer = vim.uv.new_timer()
timer:start(INTERVAL_MS, INTERVAL_MS, function()
  if not dirty then
    return
  end
  dirty = false
  -- `vim.fn.*` and `nvim_api` are not allowed in a libuv callback (a "fast event"); hop to the
  -- main loop first. Getting this wrong fails with E5560 at runtime, never at load.
  vim.schedule(send)
end)
