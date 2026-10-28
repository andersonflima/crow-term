-- Crow Term CI overrides: no network or toolchain downloads after bootstrap.
return {
  -- No language-server/tool installer.
  { "mason-org/mason.nvim", enabled = false },
  { "mason-org/mason-lspconfig.nvim", enabled = false },
  -- Pure-Lua fuzzy matcher instead of the downloaded Rust binary.
  { "saghen/blink.cmp", opts = { fuzzy = { implementation = "lua" } } },
  -- nvim-treesitter `main` needs the tree-sitter CLI to build parsers. Use the
  -- fixed parser set bundled with Neovim instead (c, lua, markdown, query,
  -- vim, vimdoc) through Neovim's own treesitter highlighter.
  { "nvim-treesitter/nvim-treesitter", enabled = false },
  { "nvim-treesitter/nvim-treesitter-textobjects", enabled = false },
  { "windwp/nvim-ts-autotag", enabled = false },
  {
    "LazyVim/LazyVim",
    init = function()
      vim.api.nvim_create_autocmd("FileType", {
        pattern = { "c", "lua", "markdown", "query", "vim", "help" },
        callback = function(event)
          pcall(vim.treesitter.start, event.buf)
        end,
      })
    end,
  },
}
