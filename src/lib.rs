#![forbid(unsafe_code)]
//! Safe Rust statusline for Antigravity CLI (`agy`).
//!
//! Reads AGY statusline JSON from stdin, inspects local SQLite conversation
//! metadata for decode/e2e speed, TTFT, thinking tokens, and compaction limits,
//! calibrates rolling quota pools, and renders a customizable multi-row
//! statusline (`Plain`, `Pill`, or `Powerline`) alongside local usage stats
//! (`agy-statusline stats`), pricing sync (`agy-statusline pricing`), and an
//! interactive `ratatui` configurator (`agy-statusline config`).

pub mod apply;
pub mod calibration;
pub mod db;
pub mod doctor;
pub(crate) mod editor;
pub mod format;
pub(crate) mod fsutil;
pub mod git;
pub mod layout;
pub(crate) mod metrics;
pub mod paths;
pub(crate) mod procutil;
pub mod protobuf;
pub mod render;
pub mod stats;
pub mod statusline;
pub(crate) mod theme;
pub(crate) mod timeutil;
pub mod tui;
