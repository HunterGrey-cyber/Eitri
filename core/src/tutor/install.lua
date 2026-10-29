-- Defines :EitriTutor in the window's own nvim. The command writes Eitri's lessons into their directory
-- when a file is missing or does not hold its text, puts the directory on 'runtimepath' while nvim's tutor framework looks the
-- lessons up as `eitri`, and takes it off again. Called with one table,
-- `{ dir = <absolute path>, uid = <the owner the directory must have>, files = { [<path inside>] = <text> } }`;
-- every value is an argument, the paths written are this file's own list, and nothing here is built from text
-- it receives other than `dir`. Runs on LuaJIT and PUC 5.1.
local o = ...

local NAME = 'EitriTutor'
-- nvim reports a Lua command's description as its `definition`, which is how a later call tells this
-- command from one the user defined under the same name.
local DESC = 'eitri tutor: the window keys, interactively'
-- Every file of the lessons' directory, by its path inside it; each needs its text in `o.files`.
local FILES = {
  'tutor/en/eitri.tutor',
  'tutor/en/eitri.tutor.json',
  'tutor/zh/eitri.tutor',
  'tutor/zh/eitri.tutor.json',
}
local SUBDIRS = { 'tutor', 'tutor/en', 'tutor/zh' }
local PRIVATE_DIR = 448 -- 0700
local PRIVATE_FILE = 384 -- 0600

if type(o) ~= 'table' or type(o.dir) ~= 'string' or o.dir:sub(1, 1) ~= '/' then
  error('tutor: install needs an absolute directory')
end
if o.dir:find('[\\%[%]%*%?{}%$~]') then
  error('tutor: the directory has a character runtimepath does not take literally')
end
if type(o.uid) ~= 'number' or type(o.files) ~= 'table' then
  error('tutor: install needs the owner and the lessons')
end
for _, rel in ipairs(FILES) do
  if type(o.files[rel]) ~= 'string' then
    error('tutor: install has no text for ' .. rel)
  end
end

-- Before VimEnter the user's config may not have defined its own commands yet, and one defined here first
-- would make theirs fail or silently replace this one. Nothing is changed; the host asks again.
if vim.v.vim_did_enter == 0 then
  return { ok = false, early = true }
end

local uv = vim.uv or vim.loop
local dir = o.dir
local uid = o.uid
local texts = o.files

-- A real directory (not a link) of `uid`, with no permission for anyone else and all of the owner's.
local function private_dir(path)
  local st = uv.fs_lstat(path)
  return st ~= nil and st.type == 'directory' and st.uid == uid and st.mode % 512 == PRIVATE_DIR
end

-- Makes `path` unless something is there already; either way it must then be a private directory of `uid`.
-- `mkdir` refuses an existing entry, a link included, and the umask may have narrowed the mode it set.
local function ensure_dir(path)
  if uv.fs_mkdir(path, PRIVATE_DIR) then
    uv.fs_chmod(path, PRIVATE_DIR)
  end
  if not private_dir(path) then
    return nil, path .. ' is not a directory of this user closed to everyone else'
  end
  return true
end

-- Whether `path` already holds exactly `text`: a regular file of `uid` with no permission for anyone else,
-- so a file cut short by an earlier crash or an older build does not count.
local function holds(path, text)
  local st = uv.fs_lstat(path)
  if st == nil or st.type ~= 'file' or st.uid ~= uid or st.size ~= #text or st.mode % 64 ~= 0 then
    return false
  end
  local fd = uv.fs_open(path, 'r', PRIVATE_FILE)
  if not fd then
    return false
  end
  local got = uv.fs_read(fd, #text + 1, 0)
  uv.fs_close(fd)
  return got == text
end

-- Puts `text` at `path` whole or not at all: it is written in full to a new `path .. '.part'` (an exclusive
-- create, after clearing what a crashed run left there) and moved into place, which replaces whatever entry is
-- at `path`, a link included, without writing through it. A failure removes the `.part` file.
local function write_whole(path, text)
  local part = path .. '.part'
  uv.fs_unlink(part)
  local fd, err = uv.fs_open(part, 'wx', PRIVATE_FILE)
  if not fd then
    return nil, err
  end
  local written, werr = uv.fs_write(fd, text, 0)
  uv.fs_close(fd)
  if written ~= #text then
    uv.fs_unlink(part)
    return nil, werr or ('short write to ' .. part)
  end
  local moved, merr = uv.fs_rename(part, path)
  if not moved then
    uv.fs_unlink(part)
    return nil, merr
  end
  return true
end

-- The directory is checked before anything goes in it or on 'runtimepath'; the directory above it was checked
-- when it was made, so nobody else can have put another in its place since.
local function prepare()
  local ok, err = ensure_dir(dir)
  if not ok then
    return nil, err
  end
  for _, sub in ipairs(SUBDIRS) do
    ok, err = ensure_dir(dir .. '/' .. sub)
    if not ok then
      return nil, err
    end
  end
  for _, rel in ipairs(FILES) do
    local path = dir .. '/' .. rel
    if not holds(path, texts[rel]) then
      ok, err = write_whole(path, texts[rel])
      if not ok then
        return nil, 'could not write ' .. path .. ': ' .. tostring(err)
      end
    end
  end
  return true
end

-- 'runtimepath' separates entries with commas, and a comma inside a path is escaped with a backslash. A
-- directory with a character 'runtimepath' or globpath() would read otherwise was refused above.
local entry = ',' .. dir:gsub(',', '\\,')

-- Takes the last `entry` that is a whole entry off `rtp`.
local function without_entry(rtp)
  local from, last = 1, nil
  while true do
    local s, e = rtp:find(entry, from, true)
    if not s then
      break
    end
    if e == #rtp or rtp:sub(e + 1, e + 1) == ',' then
      last = s
    end
    from = s + 1
  end
  if last == nil then
    return rtp
  end
  return rtp:sub(1, last - 1) .. rtp:sub(last + #entry)
end

local function open_lessons()
  local ok, err = prepare()
  if not ok then
    vim.notify('EitriTutor: ' .. err .. '; not opening the lessons', vim.log.levels.ERROR)
    return
  end
  -- Appended, so the user's own runtime directories and nvim's stay in front, and only for the lookup: the tutor
  -- opens the file by its path and reads its checks beside it, so nothing needs the directory afterwards.
  vim.o.runtimepath = vim.o.runtimepath .. entry
  -- The tutor plugin's own entry point, not `:Tutor`: a config may skip that plugin (LazyVim does) while its
  -- autoload functions, filetype and syntax stay in nvim's runtime. It picks the language as `:Tutor` does.
  local opened, why = pcall(vim.fn['tutor#TutorCmd'], 'eitri')
  vim.o.runtimepath = without_entry(vim.o.runtimepath)
  if not opened then
    error(why, 0)
  end
end

local global = vim.api.nvim_get_commands({ builtin = false })
local in_buffer = vim.api.nvim_buf_get_commands(0, { builtin = false })
local mine = global[NAME] ~= nil and global[NAME].definition == DESC
if (global[NAME] ~= nil and not mine) or in_buffer[NAME] ~= nil then
  return { ok = true, skipped = { NAME } }
end

local made = pcall(vim.api.nvim_create_user_command, NAME, open_lessons, { nargs = 0, desc = DESC })
if not made then
  -- A name nvim keeps for itself cannot be defined; it is reported like one the user has.
  return { ok = true, skipped = { NAME } }
end
return { ok = true, skipped = {} }
