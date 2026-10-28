-- Adapted from LazyVim/starter@803bc181d7c0 lua/config/lazy.lua (Apache-2.0,
-- see ../../../LICENSE.starter). Changes for Crow Term's offline E2E:
-- lazy.nvim is never cloned here (tests/lazyvim_bootstrap.py installs it at the
-- locked commit), plugins are installed only during bootstrap, LazyVim is
-- pinned, and the update checker / change detection / news are off.
local lazypath = vim.fn.stdpath("data") .. "/lazy/lazy.nvim"
if not (vim.uv or vim.loop).fs_stat(lazypath) then
  error("lazy.nvim missing at " .. lazypath .. "; run tests/lazyvim_bootstrap.py first")
end
vim.opt.rtp:prepend(lazypath)

local bootstrap = vim.env.CROW_LAZYVIM_BOOTSTRAP == "1"

require("lazy").setup({
  spec = {
    { "LazyVim/LazyVim", import = "lazyvim.plugins", tag = "v16.0.1", opts = { news = { lazyvim = false, neovim = false } } },
    { import = "plugins" },
  },
  defaults = { lazy = false, version = false },
  install = { missing = bootstrap, colorscheme = { "tokyonight", "habamax" } },
  checker = { enabled = false },
  change_detection = { enabled = false },
  rocks = { enabled = false },
  performance = {
    rtp = {
      disabled_plugins = { "gzip", "tarPlugin", "tohtml", "tutor", "zipPlugin" },
    },
  },
})
