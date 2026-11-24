#![forbid(unsafe_code)]
//! Safe Rust statusline and local usage analytics for Antigravity CLI (`agy`).
//!
//! Architecture pillars:
//! - [`agy`] — local Antigravity storage paths, read-only SQLite sniffing, and protobuf decoding
//! - [`cli`] — non-TUI CLI command handlers (`apply`, `doctor`)
//! - [`config`] — layout configuration, built-in presets, and the 3-pane `config` TUI
//! - [`statusline`] — stdin JSON pipeline, git/quota/speed metrics, and multi-row ANSI renderer
//! - [`stats`] — local usage analytics, `models.dev` pricing sync, and unified `stats`/`conversations` TUI
//! - [`sys`] — OS, filesystem, subprocess, terminal guard, and civil-time helpers

pub mod agy;
pub mod cli;
pub mod config;
pub mod stats;
pub mod statusline;
pub mod sys;

pub use sys::time::unix_now_secs;
