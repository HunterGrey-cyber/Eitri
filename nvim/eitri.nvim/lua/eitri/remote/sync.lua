local M = {}
local api, uv = vim.api, vim.uv or vim.loop
local ns = api.nvim_create_namespace("eitri-sync")
local limit = 8 * 1024 * 1024

-- Names and notices are displayed on one line. Their original strings travel only as argv or API data.
local function shown(text)
  return (
    vim.fn
      .strtrans(tostring(text))
      :gsub("[\226][\128][\139-\143\168-\174]", function(c)
        return ("<U+%04X>"):format(vim.fn.char2nr(c))
      end)
      :gsub("[\226][\129][\160-\175]", function(c)
        return ("<U+%04X>"):format(vim.fn.char2nr(c))
      end)
  )
end

local function notify(text)
  vim.notify(shown(text), vim.log.levels.WARN)
end

local function show_output(output)
  for line in output:gmatch("[^\n]+") do
    vim.notify(shown(line), vim.log.levels.INFO)
  end
end

function M.label(st)
  local detail = ""
  if st.phase == "paused" or type(st.paused) == "table" then
    detail = "paused"
  elseif #(st.conflicts or {}) > 0 then
    detail = #st.conflicts .. " conflicts"
  elseif st.phase == "history" then
    detail = "history " .. tostring(st.git_pct) .. " %"
  elseif st.phase ~= "watching" then
    detail = st.phase
  end
  return "⇅ " .. shown(st.host) .. (detail ~= "" and " · " .. shown(detail) or "")
end

local function abs(s, path)
  return s.root:gsub("/$", "") .. "/" .. path
end

function M.mark(s)
  local paths = {}
  for _, c in ipairs(s.status and s.status.conflicts or {}) do
    paths[abs(s, c.path)] = true
    if s.real_root then paths[s.real_root .. "/" .. c.path] = true end
  end
  for _, b in ipairs(api.nvim_list_bufs()) do
    if api.nvim_buf_is_loaded(b) then
      local path = api.nvim_buf_get_name(b)
      if paths[path] then
        api.nvim_buf_set_extmark(
          b,
          ns,
          0,
          0,
          {
            id = 1,
            sign_text = "⇅",
            sign_hl_group = "DiagnosticError",
            virt_lines = {
              { { "File differs on " .. shown(s.status.host) .. " · :EitriSync", "DiagnosticError" } },
            },
          }
        )
        vim.b[b].eitri_sync_mark = s.root
      elseif vim.b[b].eitri_sync_mark == s.root then
        api.nvim_buf_clear_namespace(b, ns, 0, -1)
        vim.b[b].eitri_sync_mark = nil
      end
    end
  end
end

local function report(s)
  local unsaved, files = {}, {}
  local prefix = s.root:gsub("/$", "") .. "/"
  for _, b in ipairs(api.nvim_list_bufs()) do
    if api.nvim_buf_is_loaded(b) and vim.bo[b].buftype == "" then
      local name = api.nvim_buf_get_name(b)
      local resolved = name ~= "" and vim.fn.resolve(name) or ""
      local path = name:sub(1, #prefix) == prefix and name or resolved
      local real_prefix = (s.real_root or s.root):gsub("/$", "") .. "/"
      if path:sub(1, #real_prefix) == real_prefix then
        path = prefix .. path:sub(#real_prefix + 1)
      end
      if path:sub(1, #prefix) == prefix then
        if vim.bo[b].modified then
          table.insert(unsaved, path)
        end
        local st, err, code = uv.fs_stat(path)
        if st then
          table.insert(files, { path, st.mtime.sec * 1000 + math.floor(st.mtime.nsec / 1000000) })
        elseif code ~= "ENOENT" then
          error(err)
        end
      end
    end
  end
  return vim.json.encode({ unsaved = unsaved, files = files })
end

function M.report(s, heartbeat)
  if not s.job or s.job <= 0 then
    return
  end
  local ok, line = pcall(report, s)
  if not ok then
    line = '{"unsaved":null}'
  end
  if heartbeat or line ~= s.report then
    if pcall(vim.fn.chansend, s.job, line .. "\n") then
      s.report = line
    end
  end
end

local function argv(s, args)
  local cmd = { s.cmd, "host", "sync" }
  vim.list_extend(cmd, args)
  return cmd
end

local function failure(result)
  return vim.trim(result.stderr ~= "" and result.stderr or result.stdout or "")
end

local function run(s, args, callback, input)
  local ok, why = pcall(
    vim.system,
    argv(s, args),
    { cwd = s.root, stdin = input },
    vim.schedule_wrap(function(r)
      if r.code ~= 0 then
        notify(failure(r))
        return
      end
      callback(r.stdout or "", r.stderr or "")
    end)
  )
  if not ok then
    notify(why)
  end
end

local draw, refresh
function M.start(s, on_shim, on_exit)
  local partial, errors = "", {}
  local function failed(reason)
    s.job, s.status = nil, nil
    if not s.failure and vim.v.exiting == vim.NIL then notify(reason) end
    s.failure = reason
    on_exit()
    M.mark(s)
    draw(s)
  end
  local function message(line)
    local ok, msg = pcall(vim.json.decode, line)
    if not ok or type(msg) ~= "table" then
      notify(line)
      return
    end
    if msg.type == "status" and type(msg.status) == "table" then
      local previous = s.status
      s.status, s.failure = msg.status, nil
      if msg.shim and not s.env then
        local env = {}
        for _, pair in ipairs(msg.shim) do
          env[pair[1]] = pair[2]
        end
        if env.PATH then
          s.env = env
          on_shim(env)
        end
      end
      M.mark(s)
      vim.cmd.redrawstatus()
      if
        not previous
        or not vim.deep_equal(previous.conflicts, s.status.conflicts)
        or previous.phase == "recreating"
        or s.status.phase == "recreating"
      then
        refresh(s)
      else
        draw(s)
      end
    elseif msg.type == "line" then
      if s.notices[#s.notices] ~= msg.text then
        table.insert(s.notices, msg.text)
        notify(msg.text)
      end
      if #s.notices > 100 then
        table.remove(s.notices, 1)
      end
      draw(s)
    end
  end
  local started, job = pcall(vim.fn.jobstart, argv(s, { "watch", "--json" }), {
    cwd = s.root,
    on_stdout = function(_, lines)
      lines[1] = partial .. (lines[1] or "")
      partial = table.remove(lines) or ""
      for _, line in ipairs(lines) do
        if line ~= "" then
          message(line)
        end
      end
    end,
    on_stderr = function(_, lines)
      for _, line in ipairs(lines) do
        if line ~= "" then
          errors[1] = line
        end
      end
    end,
    on_exit = function(_, code)
      if s.timer then
        s.timer:stop()
        s.timer:close()
      end
      failed(errors[1] or ("sync watch exited " .. code))
    end,
  })
  if not started or job <= 0 then
    failed("could not start eitri host sync watch")
    return
  end
  s.job, s.report = job, nil
  M.report(s, true)
  s.timer = uv.new_timer()
  s.timer:start(
    5000,
    5000,
    vim.schedule_wrap(function()
      M.report(s, true)
    end)
  )
end

local function scratch(s)
  local b = api.nvim_create_buf(false, true)
  vim.bo[b].bufhidden = "wipe"
  vim.bo[b].swapfile = false
  vim.bo[b].modeline = false
  vim.b[b].eitri_sync_root = s.root
  return b
end

-- Let nvim encode the buffer with its own fileformat, encoding, BOM and final-newline settings.
local function write_bytes(buf)
  local name = vim.fn.tempname()
  local ok, bytes = pcall(function()
    api.nvim_buf_call(buf, function()
      api.nvim_cmd(
        { cmd = "write", bang = true, args = { name }, mods = { noautocmd = true, keepalt = true, silent = true } },
        {}
      )
    end)
    local f = assert(io.open(name, "rb"))
    local data = f:read(limit + 1) or ""
    f:close()
    assert(#data <= limit, "a merge is limited to 8 MiB")
    return data
  end)
  os.remove(name)
  if not ok then
    error(bytes)
  end
  return bytes
end

function M.merge(s, c)
  run(s, { "show-server", c.path, "--with-hashes" }, function(bytes, metadata)
    if #bytes > limit then
      notify("a merge is limited to 8 MiB")
      return
    end
    local ok, hashes = pcall(vim.json.decode, metadata:match("eitri%-sync%-hashes ([^\r\n]+)"))
    if not ok or type(hashes) ~= "table" or not hashes.server or not hashes.laptop then
      notify("The conflict read returned no version hashes; look again")
      return
    end
    local laptop = vim.fn.bufadd(abs(s, c.path))
    local function merge_path(name)
      return name == abs(s, c.path) or (s.real_root and name == s.real_root:gsub("/$", "") .. "/" .. c.path)
    end
    vim.bo[laptop].modeline = false
    vim.fn.bufload(laptop)
    vim.cmd.tabnew()
    api.nvim_win_set_buf(0, laptop)
    vim.cmd.diffthis()
    local win = api.nvim_get_current_win()
    local server = scratch(s)
    api.nvim_buf_set_name(server, "eitri-sync://server/" .. server .. "/" .. shown(c.path))
    local lines = vim.split(bytes, "\n", { plain = true })
    if #lines > 1 and lines[#lines] == "" then
      table.remove(lines)
    end
    api.nvim_buf_set_lines(server, 0, -1, false, lines)
    vim.bo[server].modifiable, vim.bo[server].readonly = false, true
    vim.cmd.vsplit()
    api.nvim_win_set_buf(0, server)
    vim.cmd.diffthis()
    api.nvim_set_current_win(win)
    local group = api.nvim_create_augroup("EitriSyncMerge" .. laptop, { clear = true })
    -- A range or append write (`:1,20w!`, `:w >>`) to the merged file would replace it without a backup or the
    -- server's check: refused. To another file it is written as asked. The name is the person's own, typed here.
    for _, event in ipairs({ "FileWriteCmd", "FileAppendCmd" }) do
      api.nvim_create_autocmd(event, {
        group = group,
        buffer = laptop,
        callback = function(ev)
          local dest = vim.fn.fnamemodify(ev.match, ":p")
          if merge_path(dest) then
            error("write the whole buffer (:w) to merge; a range or append write to the merged file is refused")
          end
          local bang = vim.v.cmdbang == 1 and "!" or ""
          local append = event == "FileAppendCmd" and ">> " or ""
          vim.cmd("silent noautocmd keepalt '[,']write" .. bang .. " " .. append .. vim.fn.fnameescape(dest))
        end,
      })
    end
    api.nvim_create_autocmd("BufWriteCmd", {
      group = group,
      buffer = laptop,
      callback = function(ev)
        local dest = vim.fn.fnamemodify(ev.match, ":p")
        if not merge_path(dest) then
          api.nvim_cmd({
            cmd = "write",
            args = { dest },
            bang = vim.v.cmdbang == 1,
            mods = { noautocmd = true, keepalt = true },
          }, {})
          return
        end
        if not merge_path(api.nvim_buf_get_name(laptop)) then
          error("the merge buffer changed its path; open the conflict again")
        end
        local data = write_bytes(laptop)
        vim.bo[laptop].modified = true
        local r = vim
          .system(
            argv(
              s,
              { "merge-write", c.path, "--server-sha256", hashes.server, "--laptop-sha256", hashes.laptop }
            ),
            { cwd = s.root, stdin = data }
          )
          :wait(60000)
        local writing, written = false, nil
        if r then
          local diagnostic = {}
          for line in (r.stderr or ""):gmatch("[^\r\n]+") do
            local digest = line:match("^eitri%-sync%-laptop%-written ([0-9a-f]+)$")
            if digest and #digest == 64 then
              written = digest
            elseif line == "eitri-sync-laptop-writing" then
              writing = true
            else
              table.insert(diagnostic, line)
            end
          end
          r.stderr = table.concat(diagnostic, "\n")
        end
        if written then
          hashes.laptop = written
          vim.bo[laptop].modified = false
        end
        if not r or r.code == 124 then
          local laptop_state = "laptop write did not start"
          if written then
            laptop_state = "laptop file was written"
          elseif not r or writing then
            laptop_state = "laptop write unconfirmed"
          end
          error("sync merge timed out; outcome unknown; " .. laptop_state)
        end
        if r.code ~= 0 then
          error(shown(failure(r)))
        end
        vim.bo[laptop].modified = false
        api.nvim_clear_autocmds({ group = group })
        vim.cmd("checktime " .. laptop)
        show_output(r.stdout or "")
        M.report(s, true)
      end,
    })
    api.nvim_create_autocmd("BufWipeout", {
      group = group,
      buffer = server,
      once = true,
      callback = function()
        pcall(api.nvim_del_augroup_by_id, group)
        notify("Closed server diff for " .. c.path .. "; writes now save only the laptop copy")
      end,
    })
  end)
end

local function action(s, args, confirm)
  return function(still_selected)
    if confirm and vim.fn.confirm(confirm, "&Yes\n&No", 2) ~= 1 then
      return
    end
    if not still_selected() then return end
    run(s, args, function(output)
      show_output(output)
      refresh(s)
    end, confirm and "y\n" or nil)
  end
end

draw = function(s)
  if not s.view or not api.nvim_buf_is_valid(s.view) then
    return
  end
  local st, d = s.status, s.details or {}
  local lines, actions = {}, {}
  local function row(text, callback, identity)
    table.insert(lines, text)
    if callback then actions[#lines] = { text = text, identity = identity, apply = callback } end
  end
  if not st then
    row(s.failure and shown(s.failure) or "Waiting for sync status")
  else
    row(M.label(st))
    if type(st.notice) == "string" then
      row(shown(st.notice))
    end
    if d.notice then
      row(shown(d.notice))
    end
    local paused = type(st.paused) == "table" and st.paused or nil
    if paused then
      row(shown(paused.detail))
      if paused.reason == "tripwire" then
        row("  Allow this batch", action(s, { "allow" }), paused.detail)
        if type(paused.path) == "string" then
          row("  Always allow " .. shown(paused.path), action(s, { "allow", paused.path }), paused.path)
          row("  Leave on server", action(s, { "exclude", paused.path }), paused.path)
        end
      else
        row("  Resume sync", action(s, { "resume" }, shown(paused.detail) .. "\nResume sync?"), paused.detail)
      end
    end
    for _, c in ipairs(st.conflicts or {}) do
      row(
        "Conflict: "
          .. shown(c.path)
          .. " (laptop: "
          .. shown(c.laptop_kind)
          .. ", server: "
          .. shown(c.server_kind)
          .. ")"
      )
      local opt = d.choices and d.choices[c.path]
      if opt and opt.reason then
        row("  " .. shown(opt.reason))
      elseif opt and not paused and st.phase ~= "recreating" and st.phase ~= "paused" then
        if opt.keep_server then
          row("  Keep the server's", action(s, { "resolve", c.path, "--keep", "server" },
            "Keep " .. shown(c.path) .. " on " .. shown(st.host) .. " and remove the laptop copy?"), c.path)
        end
        if opt.keep_laptop then
          row("  Keep the laptop's", action(s, { "resolve", c.path, "--keep", "laptop" },
            "Keep " .. shown(c.path) .. " on the laptop and remove the copy on " .. shown(st.host) .. "?"), c.path)
        end
        if opt.merge then
          row("  Merge", function()
            M.merge(s, c)
          end, c.path)
        end
      end
    end
    for _, list in ipairs({ st.problems or {}, d.problems or {} }) do
      for _, p in ipairs(list) do
        row(shown(p.path) .. " (" .. shown(p.side) .. ") · not synced: " .. shown(p.text))
      end
    end
    for _, path in ipairs(st.copied_back or {}) do
      row("Copied back: " .. shown(path) .. " · dismiss", action(s, { "dismiss", abs(s, path) }), path)
    end
    if (st.copied_back_more or 0) > 0 then
      row("And " .. st.copied_back_more .. " more copied back")
    end
    if #(st.copied_back or {}) > 0 or (st.copied_back_more or 0) > 0 then
      row("Dismiss all copied back", action(s, { "dismiss", "--all" },
        "Dismiss all copied-back notices for " .. shown(s.root) .. "? Both sides keep their files."),
        { st.copied_back, st.copied_back_more })
    end
    for _, p in ipairs(st.kept_here or {}) do
      row("No longer synced; this laptop keeps its copy: " .. shown(p))
    end
    for _, p in ipairs(d.excludes or {}) do
      row("Exclude rule: " .. shown(p))
    end
    for _, p in ipairs(d.excludes_added or {}) do
      row("Excluded here: " .. shown(p))
    end
    for _, p in ipairs(d.includes or {}) do
      row("Included here: " .. shown(p))
    end
    for _, p in ipairs(d.allowed or {}) do
      row("Tripwire allows: " .. shown(p))
    end
  end
  for _, line in ipairs(s.notices) do
    row(shown(line))
  end
  row(
    "an edit made on one side beats a delete on the other without a conflict; turn review on the server shows a deleted file that came back"
  )
  -- A redraw may put a different conflict's identically labelled action under a stationary cursor.
  -- Move such cursors to the heading so Enter requires a fresh selection.
  local cursors = {}
  for _, win in ipairs(vim.fn.win_findbuf(s.view)) do
    cursors[win] = api.nvim_win_get_cursor(win)
  end
  local old = s.actions or {}
  vim.bo[s.view].modifiable = true
  api.nvim_buf_set_lines(s.view, 0, -1, false, lines)
  vim.bo[s.view].modifiable = false
  s.actions = actions
  for win, cursor in pairs(cursors) do
    local n = math.min(cursor[1], #lines)
    local before, after = old[cursor[1]], actions[n]
    if after and (not before or before.text ~= after.text or not vim.deep_equal(before.identity, after.identity)) then
      n = 1
    end
    api.nvim_win_set_cursor(win, { n, cursor[2] })
  end
end

refresh = function(s)
  if not s.view or not api.nvim_buf_is_valid(s.view) then
    return
  end
  s.details = nil
  draw(s)
  if not s.job then return end
  if s.fetching then
    s.again = true
    return
  end
  s.fetching = true
  local ok, why = pcall(
    vim.system,
    argv(s, { "status", "--json", "--details", s.root }),
    { cwd = s.root },
    vim.schedule_wrap(function(r)
      s.fetching = false
      if s.again then
        s.again = false
        refresh(s)
        return
      end
      if r.code ~= 0 then
        notify(failure(r))
        return
      end
      local parsed, details = pcall(vim.json.decode, r.stdout)
      if not parsed then
        notify("unreadable sync details")
        return
      end
      s.details = details
      draw(s)
    end)
  )
  if not ok then
    s.fetching = false
    notify(why)
  end
end

function M.open(s)
  if not s.view or not api.nvim_buf_is_valid(s.view) then
    s.view = scratch(s)
    vim.keymap.set("n", "<CR>", function()
      local selected = s.actions[api.nvim_win_get_cursor(0)[1]]
      local function still_selected()
        return selected
          and api.nvim_get_current_buf() == s.view
          and s.actions[api.nvim_win_get_cursor(0)[1]] == selected
          and api.nvim_get_current_line() == selected.text
      end
      if still_selected() then
        selected.apply(still_selected)
      end
    end, { buffer = s.view, desc = "Apply sync action" })
    vim.keymap.set("n", "q", function()
      vim.cmd.close()
    end, { buffer = s.view })
  end
  if api.nvim_get_current_buf() ~= s.view then
    vim.cmd.split()
    api.nvim_win_set_buf(0, s.view)
  end
  refresh(s)
end
return M
