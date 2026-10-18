//! Reusable pieces of the embedded-Neovim prompt.
//!
//! The `eix-nvim` binary is the original spike; the TUI links this library to
//! render a real Neovim in the prompt area and forward keys to it.

pub mod embedded;
pub mod grid;
pub mod keys;
pub mod prompt;
