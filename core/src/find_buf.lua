-- Goes ahead of the chunk's own locals, so it names `vim.api` in full.
--
-- The buffer that shows `path`: a loaded one first -- of exactly that name, else one naming the
-- same (device, inode) through a link -- and only then an unloaded one, so a listed but unloaded
-- buffer of that name does not hide the loaded one the user is looking at. Never the `bufnr()`
-- function, which reads its argument as a file pattern (`%`, `#`, `*`, `[...]`) and can answer
-- with another file's buffer.
local function find_buf(path)
  local bufs = vim.api.nvim_list_bufs()
  local exact = nil
  for _, b in ipairs(bufs) do
    if vim.api.nvim_buf_get_name(b) == path then
      if vim.api.nvim_buf_is_loaded(b) then
        return b
      end
      exact = exact or b
    end
  end
  local want = vim.uv.fs_stat(path)
  local twin = nil
  if want then
    for _, b in ipairs(bufs) do
      local name = vim.api.nvim_buf_get_name(b)
      if b ~= exact and name ~= '' and name ~= path then
        local st = vim.uv.fs_stat(name)
        if st and st.dev == want.dev and st.ino == want.ino then
          if vim.api.nvim_buf_is_loaded(b) then
            return b
          end
          twin = twin or b
        end
      end
    end
  end
  return exact or twin
end
