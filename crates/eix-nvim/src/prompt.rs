//! Lua injected into the embedded Neovim.
//!
//! This is the "prompt profile": sanitize the buffer for use as an input box,
//! and install buffer-local mappings. Semantics live in Neovim, not in the host,
//! so `pumvisible()` / cmdline / completion-plugin state is judged by Neovim
//! itself. The mapping notifies the host over `g:eix_host_chan`.

pub const PROFILE_LUA: &str = r#"
local M = {}

local function has(mod)
  local ok, m = pcall(require, mod)
  if ok then return m end
  return nil
end

function M.menu_visible()
  if vim.fn.pumvisible() == 1 then return true end
  if vim.fn.getcmdtype() ~= "" then return true end
  local cmp = has("cmp")
  if cmp and cmp.core and cmp.core.menu and cmp.core.menu:visible() then
    return true
  end
  local blink = has("blink.cmp")
  if blink and type(blink.is_menu_visible) == "function" and blink.is_menu_visible() then
    return true
  end
  return false
end

local function confirm_completion()
  local cmp = has("cmp")
  if cmp and type(cmp.confirm) == "function" then
    cmp.confirm({ select = true })
    return true
  end
  local blink = has("blink.cmp")
  if blink and type(blink.accept) == "function" then
    blink.accept()
    return true
  end
  return false
end

-- If something already owned <CR> before us (e.g. a completion plugin), keep
-- a handle so the menu can still be confirmed.
local previous = vim.fn.maparg("<CR>", "i", false, true)
local previous_rhs = type(previous) == "table" and previous.rhs or nil
local previous_callback = type(previous) == "table" and previous.callback or nil

local function feed_previous()
  if previous_callback then
    pcall(previous_callback)
    return true
  end
  if previous_rhs and previous_rhs ~= "" then
    local keys = vim.api.nvim_replace_termcodes(previous_rhs, true, false, true)
    vim.api.nvim_feedkeys(keys, "n", false)
    return true
  end
  return false
end

local buf = vim.api.nvim_get_current_buf()

function M.submit()
  local lines = vim.api.nvim_buf_get_lines(buf, 0, -1, false)
  vim.rpcnotify(vim.g.eix_host_chan, "eix_submit", table.concat(lines, "\n"))
end

-- Sanitize the buffer for use as a compact input box, even when the user's
-- full config (statusline, numbers, diagnostics, ...) is loaded.
vim.bo[buf].buftype = "nofile"
vim.bo[buf].bufhidden = "hide"
vim.bo[buf].swapfile = false
vim.bo[buf].filetype = "text"
local function sanitize_win()
  vim.wo.number = false
  vim.wo.relativenumber = false
  vim.wo.signcolumn = "no"
  vim.wo.foldcolumn = "0"
  vim.wo.wrap = true
  vim.wo.linebreak = true
  vim.wo.breakindent = true
  vim.wo.list = false
  vim.wo.conceallevel = 0
  vim.wo.spell = false
  vim.wo.fillchars = "eob: "
end

sanitize_win()

-- User configs often flip window options from autocmds (`InsertEnter` is a
-- common one for "smart" line numbers). Re-assert ours afterwards: this
-- buffer-local autocmd is registered later, so it runs last.
local aug = vim.api.nvim_create_augroup("eix_prompt", { clear = true })
vim.api.nvim_create_autocmd({ "InsertEnter", "InsertLeave", "TextChanged" }, {
  group = aug,
  buffer = buf,
  callback = sanitize_win,
  desc = "eix: keep the prompt window chrome-free",
})

pcall(vim.diagnostic.enable, false, { buf = buf })
vim.o.laststatus = 0
vim.o.showtabline = 0
-- No message row: the whole grid belongs to the prompt (nvim >= 0.9).
vim.o.cmdheight = vim.fn.has("nvim-0.9") == 1 and 0 or 1
vim.o.showmode = false
vim.o.ruler = false
vim.o.mouse = ""
-- Suppress the "intro" splash for an empty unnamed buffer.
vim.opt.shortmess:append("I")

-- Animation plugins repaint cells on a timer; those frames compete with the
-- prompt's own redraws and make the input flicker (smear-cursor is the common
-- offender). Disable them for this instance, including the ones lazily loaded
-- on `VeryLazy`.
local function calm_animations()
  pcall(function() require("smear_cursor").enabled = false end)
end
calm_animations()
vim.api.nvim_create_autocmd("User", {
  group = aug,
  pattern = "VeryLazy",
  callback = calm_animations,
  desc = "eix: disable cursor animation in the embedded prompt",
})

local function on_cr()
  if M.menu_visible() then
    if vim.fn.pumvisible() == 1 then
      vim.api.nvim_feedkeys(
        vim.api.nvim_replace_termcodes("<C-y>", true, false, true), "n", false)
      return
    end
    if confirm_completion() then return end
    if feed_previous() then return end
    vim.api.nvim_feedkeys(
      vim.api.nvim_replace_termcodes("<C-y>", true, false, true), "n", false)
    return
  end
  M.submit()
end

-- Submit on `<CR>` in insert mode only: normal mode keeps Vim's native
-- `<CR>` motion so the prompt can be edited like any other buffer.
vim.keymap.set("i", "<CR>", on_cr, {
  buffer = buf,
  noremap = true,
  silent = true,
  desc = "eix: submit",
})

-- `@` at the start of a word asks the host for a file picker. Anything else
-- inserts a literal `@` so mail addresses and decorators keep working.
local function at_word_start()
  local line = vim.api.nvim_get_current_line()
  local col = vim.api.nvim_win_get_cursor(0)[2]
  local head = line:sub(1, col)
  return head == "" or head:match("%s$") ~= nil
end

local function insert_literal_at()
  -- Insert synchronously at the cursor (not via feedkeys): feedkeys would be
  -- queued after the rest of the current `nvim_input` batch and reorder the
  -- typed text.
  local row, col = unpack(vim.api.nvim_win_get_cursor(0))
  vim.api.nvim_buf_set_text(0, row - 1, col, row - 1, col, { "@" })
  vim.api.nvim_win_set_cursor(0, { row, col + 1 })
end

vim.keymap.set("i", "@", function()
  if M.menu_visible() then
    insert_literal_at()
    return
  end
  if at_word_start() then
    vim.rpcnotify(vim.g.eix_host_chan, "eix_file_picker")
  else
    insert_literal_at()
  end
end, {
  buffer = buf,
  noremap = true,
  silent = true,
  desc = "eix: file picker",
})

-- Mirror the draft to the host so it can persist `draft.set` and rebuild the
-- buffer after a crash or `:q`. The text is short, so a full copy per change is
-- cheaper than diffing.
local function buffer_text()
  return table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
end

vim.api.nvim_create_autocmd({ "TextChanged", "TextChangedI", "InsertLeave" }, {
  group = aug,
  buffer = buf,
  callback = function()
    pcall(vim.rpcnotify, vim.g.eix_host_chan, "eix_text", buffer_text())
  end,
  desc = "eix: mirror the prompt draft",
})

-- `:q` (or any exit) tells the host to restart the prompt and restore the
-- draft instead of leaving a dead input box behind.
vim.api.nvim_create_autocmd("VimLeavePre", {
  group = aug,
  callback = function()
    pcall(vim.rpcnotify, vim.g.eix_host_chan, "eix_quit", buffer_text())
  end,
  desc = "eix: report prompt exit",
})

vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "" })
vim.api.nvim_win_set_cursor(0, { 1, 0 })
vim.cmd("startinsert!")

-- Exposed for the host's `--probe` self-check.
_G.eix_prompt = M
"#;
