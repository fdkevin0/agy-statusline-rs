#![forbid(unsafe_code)]
//! Safe Rust implementation of `agy-statusline`.
//!
//! Reads AGY statusline JSON from stdin, queries the local SQLite conversation
//! database for the last turn's generation speed, and renders a color-coded
//! ANSI statusline.

pub mod config;
pub mod db;
pub mod format;
pub mod protobuf;
pub mod statusline;
