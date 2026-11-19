-- What the person's nvim holds under one directory, for sync's quiet check: every loaded buffer whose file is
-- under the root and has unsaved changes (hidden buffers included), and each loaded buffer's file under the root
-- with its modification time, so a save since an earlier look can be told.
--
-- Argument: the root's absolute path. It is only compared, never opened, and never becomes Ex or Lua source.
-- A buffer is under the root when its resolved full name is the root or starts with the root and a slash.
-- Times are milliseconds since the epoch (seconds and nanoseconds from the file system), which Lua's numbers
-- hold exactly.
local root, since = ...
if type(root) ~= 'string' or root == '' then
  return { unsaved = {}, files = {} }
end
root = root:gsub('/+$', '')
local prefix = root .. '/'
if since ~= nil then
  if not _G.__eitri_saves_since then return nil end
  return { files = _G.__eitri_saves_since(since, prefix) }
end

local function under(name)
  return name == root or name:sub(1, #prefix) == prefix
end

local unsaved, files = {}, {}
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].buftype == '' then
    local name = vim.api.nvim_buf_get_name(buf)
    if name ~= '' then
      local full = vim.fn.fnamemodify(name, ':p')
      local resolved = vim.fn.resolve(full)
      if under(full) or under(resolved) then
        -- The name under the root: a buffer opened through a link from elsewhere is reported by where it lands.
        local name_under = under(full) and full or resolved
        if vim.bo[buf].modified then
          table.insert(unsaved, name_under)
        end
        local st = vim.uv.fs_stat(full)
        if st then
          table.insert(files, { name_under, st.mtime.sec * 1000 + math.floor(st.mtime.nsec / 1000000) })
        end
      end
    end
  end
end
return { unsaved = unsaved, files = files }
