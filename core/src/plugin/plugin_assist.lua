-- Sets whether nvim shows diagnostics and inlay hints for the buffers under one directory, and puts back
-- what was there. Called with (op, opts): op is 'apply' or 'clear'; every value is an argument. One owner
-- (a plugin id) holds one root at a time; several owners may hold the same buffer. Runs on LuaJIT and PUC 5.1.
local op, opts = ...

-- `owners`: owner -> its state. `buffers`: bufnr -> the one record of a buffer, shared by every owner holding it,
-- so the value put back after the last owner is the buffer's own and never another owner's setting. `seq` orders
-- applies, so the owner applied most recently is the one whose values stay when another clears. `last`: owner ->
-- the newest call run for it, `{ maker, order }` (see `made_last`).
local reg = rawget(_G, '__eitri_plugin_assist')
if reg == nil then
  reg = { owners = {}, buffers = {}, seq = 0, last = {} }
  rawset(_G, '__eitri_plugin_assist', reg)
end
-- A registry left by a copy of this chunk from before `last` existed.
reg.last = reg.last or {}
local all, shared = reg.owners, reg.buffers

-- Whether this call is the newest made for its owner, recording it if so. A window's calls can, rarely, reach nvim
-- in another order than they were made in; an apply landing after the clear made to undo it would then stay in
-- force. Each call carries `maker` (the Eitri process) and `order` (counting up in that process), and one older
-- than a call already run for the same owner and maker is skipped, so the call made last is the one in force. A
-- call from another maker, or without an order, always runs.
local function made_last(o)
  if o.order == nil then
    return true
  end
  local last = reg.last[o.owner]
  if last ~= nil and last.maker == o.maker and o.order < last.order then
    return false
  end
  reg.last[o.owner] = { maker = o.maker, order = o.order }
  return true
end

local function have_diag()
  return type(vim.diagnostic) == 'table'
    and type(vim.diagnostic.is_enabled) == 'function'
    and type(vim.diagnostic.enable) == 'function'
end

local function have_hint()
  local ih = vim.lsp and vim.lsp.inlay_hint
  return type(ih) == 'table' and type(ih.is_enabled) == 'function' and type(ih.enable) == 'function'
end

local function attached(bufnr)
  return vim.lsp ~= nil and type(vim.lsp.get_clients) == 'function' and #vim.lsp.get_clients({ bufnr = bufnr }) > 0
end

-- Configurations turn hints on from scheduled callbacks after an attach, and again when the server registers
-- capabilities, which can land seconds later; the setting is put back at each of these delays after an attach.
local RETRY_MS = { 100, 500, 1500, 4000 }

local function realpath(name)
  local uv = vim.uv or vim.loop
  return (uv and uv.fs_realpath(name)) or name
end

-- Whether the buffer's file is under `root`, compared with a trailing separator so /w/ex2 is not in /w/ex.
local function under(root, bufnr)
  if not vim.api.nvim_buf_is_valid(bufnr) then
    return false
  end
  local name = vim.api.nvim_buf_get_name(bufnr)
  if name == '' then
    return false
  end
  local path = realpath(vim.fs.normalize(name))
  return path:sub(1, #root + 1) == root .. '/'
end

local function read(bufnr)
  local d, h = nil, nil
  if have_diag() then
    d = vim.diagnostic.is_enabled({ bufnr = bufnr })
  end
  if have_hint() then
    h = vim.lsp.inlay_hint.is_enabled({ bufnr = bufnr })
  end
  return d, h
end

local function set(bufnr, d, h)
  if d ~= nil and have_diag() then
    vim.diagnostic.enable(d, { bufnr = bufnr })
  end
  if h ~= nil and have_hint() then
    vim.lsp.inlay_hint.enable(h, { bufnr = bufnr })
  end
end

-- Puts this owner's wanted values on one buffer, recording what the buffer had the first time any owner
-- touched it. Later applies, by this owner or another, keep that first baseline.
local function assert_on(state, bufnr)
  if state.torn or not under(state.root, bufnr) then
    return false
  end
  local b = shared[bufnr]
  if b == nil then
    local d, h = read(bufnr)
    -- Before any server attaches, hints read as off whatever the configuration will do; that is no value to
    -- restore, so the hint baseline waits for the first look after an attach.
    b = { base_d = d, base_h = h, hint_pending = h ~= nil and not attached(bufnr), owners = {} }
    shared[bufnr] = b
  end
  b.owners[state.owner] = true
  -- A pending hint baseline means the buffer's hint value still has to be learned from the configuration, so
  -- hints are left alone until settle: writing the owner's value now would be read back as the baseline, or
  -- would overwrite what the configuration enabled before it was read. Without a server there is nothing to show.
  local want_h = state.want_h
  if b.hint_pending then
    want_h = nil
  end
  set(bufnr, state.want_d, want_h)
  b.set_d = state.want_d
  b.set_h = want_h
  return true
end

-- Takes a pending hint baseline once a server is attached and the configuration's own attach handlers have
-- run, then puts this owner's values back. The owner has written no hint value while the baseline was pending,
-- so what is read here is the configuration's.
local function settle(state, bufnr)
  local b = shared[bufnr]
  if b ~= nil and b.hint_pending and attached(bufnr) then
    local _, h = read(bufnr)
    b.base_h = h
    b.hint_pending = false
  end
  assert_on(state, bufnr)
end

-- The owner applied most recently among those still holding the buffer.
local function latest_holder(b)
  local best = nil
  for owner in pairs(b.owners) do
    local state = all[owner]
    if state ~= nil and (best == nil or state.seq > best.seq) then
      best = state
    end
  end
  return best
end

-- Lets go of this owner's buffers. A buffer another owner still holds gets that owner's values back and
-- nothing restored. On a buffer this owner held last, each value that still holds what was set is restored to
-- the buffer's baseline; a value the user changed since is left, and counted. Per buffer and per setting.
local function clear(owner)
  local state = all[owner]
  if state == nil then
    return { restored = 0, left_alone = 0 }
  end
  state.torn = true
  all[owner] = nil
  pcall(vim.api.nvim_del_augroup_by_name, 'EitriPluginAssist_' .. owner)
  local restored, left = 0, 0
  for bufnr, b in pairs(shared) do
    if b.owners[owner] then
      b.owners[owner] = nil
      local holder = latest_holder(b)
      local valid = vim.api.nvim_buf_is_valid(bufnr)
      if holder == nil or not valid then
        shared[bufnr] = nil
      end
      if valid and holder ~= nil then
        assert_on(holder, bufnr)
      elseif valid then
        local d, h = read(bufnr)
        if b.set_d ~= nil and d ~= nil then
          if d == b.set_d then
            set(bufnr, b.base_d, nil)
            restored = restored + 1
          else
            left = left + 1
          end
        end
        if b.set_h ~= nil and h ~= nil then
          if h == b.set_h then
            set(bufnr, nil, b.base_h)
            restored = restored + 1
          else
            left = left + 1
          end
        end
      end
    end
  end
  return { restored = restored, left_alone = left }
end

local function apply(o)
  local owner = o.owner
  local state = all[owner]
  if state ~= nil and state.root ~= o.root then
    -- A new root: the old one is put back first.
    clear(owner)
    state = nil
  end
  if state == nil then
    state = { owner = owner, root = o.root, torn = false }
    all[owner] = state
  end
  reg.seq = reg.seq + 1
  state.seq = reg.seq
  state.want_d = o.diagnostics
  state.want_h = o.inlay_hints
  -- Configurations often turn hints on when a server attaches, from the attach itself, a scheduled call or a
  -- later capability registration: the setting is put back at once, after what was scheduled, and at each
  -- retry delay after an attach. A buffer whose hint baseline is still pending gets no hint value written until
  -- the first retry has read what the configuration wanted.
  local group = vim.api.nvim_create_augroup('EitriPluginAssist_' .. owner, { clear = true })
  vim.api.nvim_create_autocmd({ 'BufReadPost', 'BufEnter', 'LspAttach' }, {
    group = group,
    callback = function(ev)
      if state.torn then
        return
      end
      assert_on(state, ev.buf)
      vim.schedule(function()
        if not state.torn then
          assert_on(state, ev.buf)
        end
      end)
      if ev.event == 'LspAttach' then
        for _, ms in ipairs(RETRY_MS) do
          vim.defer_fn(function()
            if not state.torn then
              settle(state, ev.buf)
            end
          end, ms)
        end
      end
    end,
  })
  local count = 0
  for _, bufnr in ipairs(vim.api.nvim_list_bufs()) do
    if assert_on(state, bufnr) then
      count = count + 1
    end
  end
  return {
    diagnostics = o.diagnostics,
    inlay_hints = o.inlay_hints,
    buffers = count,
    partial = not (have_diag() and have_hint()),
  }
end

if op == 'apply' then
  if not made_last(opts) then
    -- Overtaken by a later call, which is what stays in force: nothing is touched.
    return {
      diagnostics = opts.diagnostics,
      inlay_hints = opts.inlay_hints,
      buffers = 0,
      partial = not (have_diag() and have_hint()),
      skipped = true,
    }
  end
  return apply(opts)
elseif op == 'clear' then
  if not made_last(opts) then
    return { restored = 0, left_alone = 0, skipped = true }
  end
  return clear(opts.owner)
end
error('plugin_assist: unknown op ' .. tostring(op))
