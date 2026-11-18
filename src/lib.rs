#![forbid(unsafe_code)]
//! Safe Rust statusline for Antigravity CLI (`agy`).
//!
//! Reads AGY statusline JSON from stdin, queries the local SQLite conversation
//! database for the last completed turn's generation speed, and renders a
//! customizable multi-row statusline (`Plain`, `Pill`, or `Powerline`) with an
//! interactive `ratatui` configurator (`agy-statusline config`).

pub mod ansi;
pub mod apply;
pub mod calibration;
pub mod db;
pub mod editor;
pub mod format;
pub mod git;
pub mod layout;
pub mod metrics;
pub mod protobuf;
pub mod render;
pub mod stats;
pub mod statusline;
pub mod theme;
pub mod timeutil;
pub mod tui;
