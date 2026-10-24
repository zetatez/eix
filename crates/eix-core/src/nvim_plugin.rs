//! The Lua frontend used by `eix nvim`.
//!
//! Neovim is the client: it connects to the daemon over the same newline-JSON
//! protocol as the TUI and renders the transcript in a scratch buffer. The
//! plugin is embedded here so `eix nvim` works without installing anything, and
//! e2e tests can write the same file for headless runs.

use std::fs;
use std::path::{Path, PathBuf};

/// File name inside the data directory.
pub const FILE_NAME: &str = "eix-nvim.lua";

pub const LUA: &str = r####"
-- eix nvim frontend. Talks newline-delimited JSON-RPC to eixd.
local uv = vim.uv or vim.loop

local state = {
  id = 1,
  pending = {},
  lines = {},
  stream = nil,
  buffer = nil,
  session = nil,
  connected = false,
  attached = false,
  pending_attach = nil,
  socket = vim.env.EIX_NVIM_SOCKET or "",
  tcp = vim.env.EIX_NVIM_TCP or "",
  token = vim.env.EIX_NVIM_TOKEN or "",
  initial_session = vim.env.EIX_NVIM_SESSION or "",
  cwd = vim.fn.getcwd(),
}
_G.eix_nvim = state

local function notify(msg, level)
  vim.schedule(function()
    vim.notify("eix: " .. msg, level or vim.log.levels.INFO)
  end)
end

local function encode(value)
  if vim.json and vim.json.encode then
    return vim.json.encode(value)
  end
  return vim.fn.json_encode(value)
end

local function decode(text)
  if vim.json and vim.json.decode then
    return vim.json.decode(text)
  end
  return vim.fn.json_decode(text)
end

local send_raw
local dispatch
local redraw
local attach_target
local open_palette
local attach
local session_picker

local function request(method, params, cb)
  local id = state.id
  state.id = id + 1
  state.pending[id] = cb or function() end
  send_raw({ id = id, method = method, params = params or vim.empty_dict() })
end

local function commit_text(text)
  if text == nil or text == "" then
    return
  end
  for _, line in ipairs(vim.split(text, "\n", { plain = true })) do
    table.insert(state.lines, line)
  end
end

local function flush_stream()
  if state.stream ~= nil then
    commit_text(state.stream)
    state.stream = nil
  end
end

local function one_line(text)
  if text == nil then
    return ""
  end
  local first = text:match("^[^\n]*") or ""
  if #first > 160 then
    first = first:sub(1, 157) .. "..."
  end
  return first
end

local function render_event(event)
  local kind = event.kind
  local p = event.payload or {}
  if kind == "user_message" then
    flush_stream()
    table.insert(state.lines, "")
    table.insert(state.lines, "## you")
    commit_text(p.text)
  elseif kind == "assistant_message" then
    if state.stream ~= nil then
      -- The streamed deltas are the same text; avoid drawing it twice.
      state.stream = p.text or state.stream
      flush_stream()
    else
      table.insert(state.lines, "")
      table.insert(state.lines, "## assistant")
      commit_text(p.text)
    end
  elseif kind == "tool_call" then
    flush_stream()
    table.insert(state.lines, "")
    table.insert(state.lines, "· " .. (p.tool or "tool") .. " " .. one_line(encode(p.arguments or {})))
  elseif kind == "tool_result" then
    table.insert(state.lines, (p.ok and "  → " or "  ✗ ") .. one_line(p.output))
  elseif kind == "patch_proposed" then
    table.insert(state.lines, "  patch " .. (p.path or "") .. " · apply or reject from any client")
  elseif kind == "error" then
    table.insert(state.lines, "  ! " .. (p.message or "error"))
  end
end

local function on_delta(params)
  if state.session == nil or params.session_id ~= state.session.id then
    return
  end
  if state.stream == nil then
    table.insert(state.lines, "")
    table.insert(state.lines, "## assistant")
    state.stream = ""
  end
  state.stream = state.stream .. (params.delta or "")
  redraw()
end

dispatch = function(method, params)
  if method == "session.event" then
    if state.session ~= nil and params.session_id ~= state.session.id then
      return
    end
    render_event(params)
    redraw()
  elseif method == "message.delta" then
    on_delta(params)
  elseif method == "turn.started" then
    if state.session ~= nil and params.session_id == state.session.id then
      state.busy = true
    end
  elseif method == "turn.completed" then
    if state.session ~= nil and params.session_id == state.session.id then
      flush_stream()
      state.busy = false
      redraw()
    end
  elseif method == "session.updated" then
    if state.session ~= nil and params.id == state.session.id then
      state.session = params
    end
  elseif method == "session.attention" then
    if state.session ~= nil and params.session_id == state.session.id then
      notify("session " .. tostring(params.status), vim.log.levels.WARN)
    end
  elseif method == "agent.spawned" then
    table.insert(state.lines, "  ↳ sub-agent " .. tostring(params.name or params.agent_id or ""))
    redraw()
  end
end

local function handle(message)
  if message.id ~= nil then
    local cb = state.pending[message.id]
    state.pending[message.id] = nil
    if cb then
      cb(message.result, message.error)
    end
  elseif message.method ~= nil then
    dispatch(message.method, message.params or {})
  end
end

local function on_data(err, data)
  if err then
    notify(tostring(err), vim.log.levels.ERROR)
    return
  end
  if not data then
    return
  end
  state.inbox = (state.inbox or "") .. data
  while true do
    local nl = state.inbox:find("\n", 1, true)
    if not nl then
      break
    end
    local line = state.inbox:sub(1, nl - 1)
    state.inbox = state.inbox:sub(nl + 1)
    if line ~= "" then
      local ok, message = pcall(decode, line)
      if ok then
        handle(message)
      else
        notify("bad message: " .. tostring(message), vim.log.levels.ERROR)
      end
    end
  end
end

send_raw = function(value)
  if state.handle == nil then
    notify("not connected", vim.log.levels.ERROR)
    return
  end
  local ok, data = pcall(encode, value)
  if not ok then
    notify("encode failed: " .. tostring(data), vim.log.levels.ERROR)
    return
  end
  state.handle:write(data .. "\n")
end

redraw = function()
  vim.schedule(function()
    vim.api.nvim_buf_set_option(state.buffer, "modifiable", true)
    local all = {}
    vim.list_extend(all, state.lines)
    if state.stream ~= nil then
      vim.list_extend(all, vim.split(state.stream, "\n", { plain = true }))
    end
    if #all == 0 then
      all = { "eix: waiting for events..." }
    end
    vim.api.nvim_buf_set_lines(state.buffer, 0, -1, false, all)
    vim.api.nvim_buf_set_option(state.buffer, "modifiable", false)
    local ok = pcall(vim.api.nvim_win_set_cursor, 0, { #all, 0 })
    if not ok then
      -- The buffer may not be visible yet.
    end
  end)
end

local function create_buffer(name)
  state.buffer = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_name(state.buffer, name)
  vim.bo[state.buffer].buftype = "nofile"
  vim.bo[state.buffer].bufhidden = "wipe"
  vim.bo[state.buffer].swapfile = false
  vim.bo[state.buffer].modifiable = true
  vim.api.nvim_set_current_buf(state.buffer)
  vim.api.nvim_buf_set_keymap(state.buffer, "n", "<CR>", ":Eix prompt<CR>", {
    noremap = true,
    silent = true,
  })
  if vim.keymap and vim.keymap.set then
    vim.keymap.set("n", "<leader>a", function()
      if open_palette then
        open_palette()
      end
    end, { buffer = state.buffer, desc = "eix actions" })
    vim.keymap.set("n", "<C-g><C-g>", function()
      if state.previous_session ~= nil then
        attach(state.previous_session)
      end
    end, { buffer = state.buffer, desc = "eix previous session" })
    vim.keymap.set("n", "<C-g>", function()
      session_picker()
    end, { buffer = state.buffer, desc = "eix sessions" })
  end
  redraw()
end

-- Buffer APIs are forbidden in libuv callbacks, so every buffer check and
-- creation goes through `vim.schedule`.
local function show_buffer(name)
  vim.schedule(function()
    if state.buffer == nil or not vim.api.nvim_buf_is_valid(state.buffer) then
      create_buffer(name)
    else
      vim.api.nvim_set_current_buf(state.buffer)
      redraw()
    end
  end)
end

attach = function(id)
  request("session.attach", { session_id = id }, function(result, err)
    if err then
      notify("attach: " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    if state.session ~= nil and state.session.id ~= result.session.id then
      state.previous_session = state.session.id
    end
    state.session = result.session
    state.attached = true
    -- This frontend only renders one session; stop the daemon from sending
    -- every other session's events. `:Eix list` still fetches on demand.
    request("session.subscribe", { session_ids = { result.session.id } }, function() end)
    state.lines = {}
    state.stream = nil
    for _, event in ipairs(result.events) do
      render_event(event)
    end
    show_buffer("eix://" .. result.session.id:sub(1, 8))
  end)
end

-- `<C-g>` session picker and `<C-g><C-g>` back to the previous session.
session_picker = function()
  request("session.list", {}, function(result, err)
    if err then
      notify("sessions: " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    vim.schedule(function()
      local sessions = result.sessions or {}
      if #sessions == 0 then
        notify("no sessions")
        return
      end
      vim.ui.select(sessions, {
        prompt = "eix sessions",
        format_item = function(session)
          local mark = (state.session ~= nil and session.id == state.session.id) and "* " or "  "
          return string.format(
            "%s%s %s  %s",
            mark,
            session.status,
            session.name,
            session.id:sub(1, 8)
          )
        end,
      }, function(session)
        if session ~= nil then
          attach(session.id)
        end
      end)
    end)
  end)
end

-- Split a `picker.query` provider's rows into a `vim.ui.select`, then hand the
-- chosen item back. Previews are not shown (nvim's select has no side pane);
-- the TUI is the rich picker, this is the lightweight one.
local function pick_data(kind, prompt, on_choice)
  if state.session == nil then
    notify("attach a session first", vim.log.levels.ERROR)
    return
  end
  request("picker.query", {
    kind = kind,
    session_id = state.session.id,
    limit = 2000,
  }, function(result, err)
    if err then
      notify(kind .. ": " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    vim.schedule(function()
      local items = result.items or {}
      if #items == 0 then
        notify("no " .. kind .. " rows in this workspace")
        return
      end
      vim.ui.select(items, {
        prompt = prompt,
        format_item = function(item)
          if item.detail ~= nil and item.detail ~= "" then
            return string.format("%s  %s", item.label or item.value, item.detail)
          end
          return item.label or item.value
        end,
      }, function(item)
        if item ~= nil then
          on_choice(item)
        end
      end)
    end)
  end)
end

-- `@` file picker: ask the daemon for workspace files, then hand one back.
local function pick_files(on_choice)
  pick_data("files", "eix files", function(item)
    on_choice(item.value)
  end)
end

-- Insert a picked value at the cursor of the current buffer.
local function insert_picked(text)
  local row, col = unpack(vim.api.nvim_win_get_cursor(0))
  vim.api.nvim_buf_set_text(0, row - 1, col, row - 1, col, { text })
  vim.api.nvim_win_set_cursor(0, { row, col + #text })
end

-- Pick a provider row and insert its value (worktrees, branches, files).
local function pick_insert(kind, prompt)
  pick_data(kind, prompt, function(item)
    insert_picked(item.value)
  end)
end

-- Render `lines` (with a `title` header) in a scratch split.
local function show_scratch(title, lines)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.bo[buf].buftype = "nofile"
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.list_extend({ title, "" }, lines))
  vim.cmd("botright 12split")
  vim.api.nvim_win_set_buf(0, buf)
end

local function create_session(descend)
  request("session.create", { cwd = state.cwd, worktree = descend == true }, function(result, err)
    if err then
      notify("create: " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    attach(result.session.id)
  end)
end

attach_target = function(target)
  if not state.connected then
    state.pending_attach = target
    return
  end
  request("session.list", {}, function(result, err)
    if err then
      notify("list: " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    local chosen = nil
    if target ~= nil and target ~= "" then
      for _, session in ipairs(result.sessions) do
        if session.id == target or session.name == target or session.id:sub(1, #target) == target then
          chosen = session
          break
        end
      end
    end
    if chosen == nil then
      chosen = result.sessions[1]
    end
    if chosen == nil then
      create_session(false)
    else
      attach(chosen.id)
    end
  end)
end

local function list_sessions()
  request("session.list", {}, function(result, err)
    if err then
      notify("list: " .. tostring(err.message), vim.log.levels.ERROR)
      return
    end
    vim.schedule(function()
      local lines = { "## sessions" }
      for _, session in ipairs(result.sessions) do
        table.insert(lines, string.format(
          "%-4s %-8s %-24s %s",
          session.status,
          session.id:sub(1, 8),
          session.name,
          session.cwd
        ))
      end
      local buf = vim.api.nvim_create_buf(false, true)
      vim.bo[buf].buftype = "nofile"
      vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
      vim.cmd("botright 12split")
      vim.api.nvim_win_set_buf(0, buf)
    end)
  end)
end

local function open_prompt()
  vim.schedule(function()
    local buf = vim.api.nvim_create_buf(false, true)
    vim.bo[buf].buftype = "nofile"
    vim.bo[buf].filetype = "markdown"
    vim.cmd("botright 8split")
    vim.api.nvim_win_set_buf(0, buf)
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "" })
    vim.api.nvim_buf_set_keymap(buf, "n", "<CR>", "", {
      noremap = true,
      silent = true,
      callback = function()
        local line = vim.api.nvim_get_current_line()
        if line ~= "" and state.session ~= nil then
          request("prompt.submit", { session_id = state.session.id, text = line }, function(_, err)
            if err then
              notify("submit: " .. tostring(err.message), vim.log.levels.ERROR)
            end
          end)
          vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "" })
        end
      end,
    })
    -- `@` at the start of a word opens the workspace file picker; mid-word it
    -- stays literal.
    local function insert_at(at)
      local row, col = unpack(vim.api.nvim_win_get_cursor(0))
      vim.api.nvim_buf_set_text(buf, row - 1, col, row - 1, col, { at })
      vim.api.nvim_win_set_cursor(0, { row, col + #at })
      vim.cmd("startinsert")
    end
    vim.api.nvim_buf_set_keymap(buf, "i", "@", "", {
      noremap = true,
      silent = true,
      callback = function()
        local line = vim.api.nvim_get_current_line()
        local col = vim.api.nvim_win_get_cursor(0)[2]
        local head = line:sub(1, col)
        if head == "" or head:match("%s$") ~= nil then
          local win = vim.api.nvim_get_current_win()
          pick_files(function(value)
            vim.api.nvim_set_current_win(win)
            insert_at("@" .. value)
          end)
        else
          insert_at("@")
        end
      end,
    })
    vim.cmd("startinsert")
  end)
end

local function send_prompt(text)
  if state.session == nil then
    notify("no session attached", vim.log.levels.ERROR)
    return
  end
  if text == nil or text == "" then
    text = vim.api.nvim_get_current_line()
  end
  if text == "" then
    return
  end
  request("prompt.submit", { session_id = state.session.id, text = text }, function(_, err)
    if err then
      notify("submit: " .. tostring(err.message), vim.log.levels.ERROR)
    end
  end)
end

local function require_session()
  if state.session == nil then
    notify("no session attached", vim.log.levels.ERROR)
    return false
  end
  return true
end

-- `<leader>a` action palette for the eix buffer.
open_palette = function()
  local actions = {
    { label = "switch session (picker)", run = session_picker },
    { label = "list sessions", run = list_sessions },
    { label = "new session", run = function() create_session(false) end },
    { label = "new session in a worktree", run = function() create_session(true) end },
    { label = "pick a file (insert path)", run = function() pick_insert("files", "eix files") end },
    { label = "pick a worktree (insert path)", run = function() pick_insert("worktrees", "eix worktrees") end },
    { label = "pick a branch (insert name)", run = function() pick_insert("branches", "eix branches") end },
    { label = "send a prompt", run = open_prompt },
    { label = "stop the running turn", run = function()
        if require_session() then
          request("prompt.cancel", { session_id = state.session.id })
        end
      end },
    { label = "fork this session", run = function() vim.cmd("Eix fork") end },
    { label = "rewind this session", run = function() vim.cmd("Eix rewind") end },
    { label = "background processes", run = function() vim.cmd("Eix ps") end },
    { label = "permission…", run = function() vim.cmd("Eix permission") end },
    { label = "doctor", run = function() vim.cmd("Eix doctor") end },
  }
  vim.ui.select(actions, {
    prompt = "eix",
    format_item = function(item)
      return item.label
    end,
  }, function(item)
    if item then
      vim.schedule(function()
        item.run()
      end)
    end
  end)
end

vim.api.nvim_create_user_command("Eix", function(opts)
  local args = vim.split(opts.args, "%s+", { trimempty = true })
  local sub = args[1] or "attach"
  if sub == "attach" then
    attach_target(args[2])
  elseif sub == "list" or sub == "ls" then
    list_sessions()
  elseif sub == "new" then
    request("session.create", {
      name = args[2],
      cwd = state.cwd,
      worktree = args[3] == "worktree",
    }, function(result, err)
      if err then
        notify("create: " .. tostring(err.message), vim.log.levels.ERROR)
        return
      end
      attach(result.session.id)
    end)
  elseif sub == "send" then
    send_prompt(table.concat(args, " ", 2))
  elseif sub == "prompt" then
    open_prompt()
  elseif sub == "stop" then
    if require_session() then
      request("prompt.cancel", { session_id = state.session.id })
    end
  elseif sub == "fork" then
    if require_session() then
      request("session.fork", {
        session_id = state.session.id,
        at_seq = tonumber(args[2]),
      }, function(result, err)
        if err then
          notify("fork: " .. tostring(err.message), vim.log.levels.ERROR)
          return
        end
        attach(result.session.id)
      end)
    end
  elseif sub == "rewind" then
    if require_session() then
      request("session.rewind", {
        session_id = state.session.id,
        at_seq = tonumber(args[2]),
      }, function(result, err)
        if err then
          notify("rewind: " .. tostring(err.message), vim.log.levels.ERROR)
          return
        end
        for _, warning in ipairs(result.warnings or {}) do
          notify(warning, vim.log.levels.WARN)
        end
        attach(result.session.id)
      end)
    end
  elseif sub == "ps" then
    request("process.list", {}, function(result, err)
      if err then
        notify("ps: " .. tostring(err.message), vim.log.levels.ERROR)
        return
      end
      vim.schedule(function()
        local lines = { "## processes" }
        for _, process in ipairs(result.processes) do
          table.insert(lines, string.format(
            "%-8s %-6s %s",
            process.id:sub(1, 8),
            process.status,
            process.command
          ))
        end
        if #lines == 1 then
          lines = { "no background processes" }
        end
        local buf = vim.api.nvim_create_buf(false, true)
        vim.bo[buf].buftype = "nofile"
        vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
        vim.cmd("botright 12split")
        vim.api.nvim_win_set_buf(0, buf)
      end)
    end)
  elseif sub == "pick" then
    local kind = args[2] or "sessions"
    if kind == "sessions" or kind == "" then
      session_picker()
    elseif kind == "files" then
      pick_insert("files", "eix files")
    elseif kind == "worktrees" then
      pick_insert("worktrees", "eix worktrees")
    elseif kind == "branches" then
      pick_insert("branches", "eix branches")
    else
      notify("unknown picker kind: " .. kind .. " (sessions|files|worktrees|branches)")
    end
  elseif sub == "doctor" then
    local endpoint = state.socket
    if state.tcp ~= "" then
      endpoint = "tcp://" .. state.tcp
    end
    local session = "-"
    if state.session ~= nil then
      session = string.format("%s (%s)", state.session.name, state.session.id:sub(1, 8))
    end
    local lines = {
      "## eix doctor",
      "nvim       " .. tostring(vim.version()),
      "endpoint   " .. endpoint,
      "connected  " .. tostring(state.connected),
      "attached   " .. tostring(state.attached),
      "auth       " .. (state.token ~= "" and "token" or "none"),
      "session    " .. session,
      "cwd        " .. state.cwd,
      "buffer     " .. tostring(state.buffer),
      "pending    " .. tostring(vim.tbl_count(state.pending)),
    }
    vim.schedule(function()
      local buf = vim.api.nvim_create_buf(false, true)
      vim.bo[buf].buftype = "nofile"
      vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
      vim.cmd("botright 12split")
      vim.api.nvim_win_set_buf(0, buf)
    end)
  elseif sub == "permission" then
    if require_session() then
      local level = args[2]
      if not level then
        notify(string.format("permission: %s", state.session.permission or "workspace-write"))
        return
      end
      request("session.permission", {
        session_id = state.session.id,
        permission = level,
      }, function(result, err)
        if err then
          notify("permission: " .. tostring(err.message), vim.log.levels.ERROR)
          return
        end
        state.session = result.session
        notify(string.format("permission: %s", result.session.permission))
      end)
    end
  else
    notify("unknown :Eix subcommand " .. sub, vim.log.levels.ERROR)
  end
end, { nargs = "*", desc = "eix daemon frontend" })

local function on_connect()
  state.connected = true
  state.handle:read_start(on_data)
  local function hello()
    request("hello", {}, function()
      -- Callbacks run in a libuv fast context, where `vim.env` is not
      -- available; use the values captured at load time.
      attach_target(state.pending_attach or state.initial_session)
      state.pending_attach = nil
    end)
  end
  if state.token ~= "" then
    request("auth", { token = state.token }, function(_, err)
      if err then
        notify("auth: " .. tostring(err.message), vim.log.levels.ERROR)
        return
      end
      hello()
    end)
  else
    hello()
  end
end

if state.tcp ~= "" then
  local host, port = state.tcp:match("^(.*):(%d+)$")
  state.handle = uv.new_tcp()
  state.handle:connect(host, tonumber(port), function(err)
    if err then
      notify(tostring(err), vim.log.levels.ERROR)
      return
    end
    on_connect()
  end)
else
  state.handle = uv.new_pipe(false)
  state.handle:connect(state.socket, function(err)
    if err then
      notify("cannot connect to " .. state.socket .. ": " .. tostring(err), vim.log.levels.ERROR)
      return
    end
    on_connect()
  end)
end
"####;

/// Write the plugin into `data_dir` and return its path. Rewriting only happens
/// when the embedded script changed, so an editor with the file open does not
/// see pointless churn.
pub fn write(data_dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join(FILE_NAME);
    if fs::read_to_string(&path).ok().as_deref() != Some(LUA) {
        fs::write(&path, LUA)?;
    }
    Ok(path)
}
