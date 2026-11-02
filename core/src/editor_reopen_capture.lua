-- File names come from buffer APIs as absolute byte strings, independent of window-local directories.
local args = ...
local state = package.loaded['eitri.reopen']
if type(state) ~= 'table' or state.done ~= args.token then
  return nil
end

local api = vim.api
local prefixes = {}
for _, prefix in ipairs(args.prefixes) do
  prefixes[#prefixes + 1] = vim.base64.decode(prefix)
end

local function bounded(value, low, high)
  return math.max(low, math.min(value, high))
end

local names = {}
local function file(buf)
  if names[buf] ~= nil then
    return names[buf] or nil
  end
  names[buf] = false
  if vim.bo[buf].buftype ~= '' then
    return nil
  end
  local name = api.nvim_buf_get_name(buf)
  if #name == 0 or #name > 4096 or name:sub(1, 1) ~= '/' or name:find('\0', 1, true) then
    return nil
  end
  for _, prefix in ipairs(prefixes) do
    if name:sub(1, #prefix) == prefix then
      return nil
    end
  end
  local stat = vim.uv.fs_stat(name)
  if not stat or stat.type ~= 'file' then
    return nil
  end
  names[buf] = vim.base64.encode(name)
  return names[buf]
end

local function leaf(win, current)
  local name = file(api.nvim_win_get_buf(win))
  if not name then
    return nil
  end
  local view = api.nvim_win_call(win, vim.fn.winsaveview)
  return { leaf = {
    f = name,
    lnum = bounded(view.lnum, 1, 2147483648),
    col = bounded(view.col, 0, 2147483648),
    want = bounded(view.curswant, 0, 2147483648),
    top = bounded(view.topline, 1, 2147483648),
    left = bounded(view.leftcol, 0, 2147483648),
    w = bounded(api.nvim_win_get_width(win), 1, 10000),
    h = bounded(api.nvim_win_get_height(win), 1, 10000),
    cur = win == current and true or nil,
  } }
end

local record = { v = 1, root = args.root, tabs = {}, bufs = {} }
local tabs = api.nvim_list_tabpages()
local current_tab = api.nvim_get_current_tabpage()
local tabline = vim.o.showtabline == 2 or (vim.o.showtabline == 1 and #tabs > 1)
for index, tab in ipairs(tabs) do
  if #record.tabs >= 16 then
    break
  end
  local skip = false
  for name in pairs(vim.fn.gettabvar(index, '')) do
    if name:sub(1, 6) == 'eitri_' then
      skip = true
      break
    end
  end
  if not skip then
    local count = 0
    local current = api.nvim_tabpage_get_win(tab)
    local function tree(node, depth)
      if count >= 32 then
        return nil
      end
      if node[1] == 'leaf' then
        local saved = leaf(node[2], current)
        if saved then
          count = count + 1
        end
        return saved
      end
      local children = {}
      for _, child in ipairs(node[2]) do
        -- At the depth bound keep the first eligible descendant without another split wrapper.
        local saved = tree(child, math.min(depth + 1, 16))
        if saved then
          children[#children + 1] = saved
          if depth == 16 then
            return saved
          end
        end
        if count >= 32 then
          break
        end
      end
      if #children == 1 then
        return children[1]
      elseif #children > 1 then
        return { [node[1]] = children }
      end
    end
    local saved = tree(vim.fn.winlayout(index), 1)
    if saved then
      record.tabs[#record.tabs + 1] = {
        cols = vim.o.columns,
        lines = vim.o.lines - vim.o.cmdheight - (tabline and 1 or 0),
        tree = saved,
      }
      if tab == current_tab then
        record.tab = #record.tabs
      end
    end
  end
end

local in_windows = {}
local function remember(node)
  if node.leaf then
    in_windows[node.leaf.f] = true
  else
    for _, child in ipairs(node.row or node.col) do
      remember(child)
    end
  end
end
for _, tab in ipairs(record.tabs) do
  remember(tab.tree)
end
for _, buf in ipairs(api.nvim_list_bufs()) do
  if #record.bufs >= 256 then
    break
  end
  if vim.bo[buf].buflisted then
    local name = file(buf)
    if name and not in_windows[name] then
      record.bufs[#record.bufs + 1] = name
    end
  end
end

local json = vim.json.encode(record)
while #json > 224 * 1024 do
  if #record.bufs > 0 then
    table.remove(record.bufs)
  elseif #record.tabs > 1 then
    table.remove(record.tabs)
    if record.tab and record.tab > #record.tabs then
      record.tab = nil
    end
  else
    break
  end
  json = vim.json.encode(record)
end
return { json = json, same = json == state.restored }
