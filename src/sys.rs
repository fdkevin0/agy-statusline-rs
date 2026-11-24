//! Low-level OS, filesystem, formatting, subprocess, terminal, and civil-time primitives.

pub mod fmt;
pub(crate) mod fs;
pub(crate) mod process;
pub(crate) mod terminal;
pub mod time;
