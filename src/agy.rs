//! Antigravity (AGY) local storage, directory resolution, settings I/O, SQLite sniffing,
//! protobuf decoding, quota snapshot persistence, and loopback Hub RPC probing.

pub mod db;
pub mod hub;
pub mod paths;
pub mod protobuf;
pub mod quota;
pub mod settings;
