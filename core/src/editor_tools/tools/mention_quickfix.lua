-- The quickfix list the editor holds now: its title and its entries in order, as far as args.scan_max. Changes
-- nothing. Rust judges every name and record before an entry is told, and cuts what it tells.
-- One byte more than the host tells, so the host can see that it cut and say so.
local TEXT_MAX = 513

local list = vim.fn.getqflist({ title = 1, size = 1, items = 1 })
local items, taken = {}, 0
for _, item in ipairs(list.items or {}) do
  if taken >= args.scan_max then
    break
  end
  taken = taken + 1
  local entry = {
    lnum = item.lnum or 0,
    end_lnum = item.end_lnum or 0,
    col = item.col or 0,
    kind = item.type or '',
    valid = item.valid == 1,
    text = (item.text or ''):sub(1, TEXT_MAX),
  }
  local buf = item.bufnr or 0
  if buf > 0 and api.nvim_buf_is_valid(buf) then
    local name = api.nvim_buf_get_name(buf)
    entry.path = name
    -- A loaded buffer with no record keeps none; one not loaded has only its name.
    if api.nvim_buf_is_loaded(buf) then
      entry.read_from = read_from(buf)
    else
      entry.read_from = name
    end
  end
  items[#items + 1] = entry
end

return { title = (list.title or ''):sub(1, 201), size = list.size or 0, items = items, left_out = math.max((list.size or 0) - taken, 0) }
