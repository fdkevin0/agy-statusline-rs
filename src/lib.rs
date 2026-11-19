#![forbid(unsafe_code)]
//! Safe Rust statusline for Antigravity CLI (`agy`).
//!
//! Reads AGY statusline JSON from stdin, queries the local SQLite conversation
//! database for the last completed turn's generation speed, and renders a
//! customizable multi-row statusline (`Plain`, `Pill`, or `Powerline`) with an
//! interactive `ratatui` configurator (`agy-statusline config`).

pub(crate) mod ansi;
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
pub mod protobuf;
pub mod render;
pub mod stats;
pub mod statusline;
pub(crate) mod theme;
pub(crate) mod timeutil;
pub mod tui;
