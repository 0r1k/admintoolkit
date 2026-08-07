//! Config Syntax Checker: validates JSON/TOML/YAML/XML files, local or on
//! a remote host over SSH (`exec`), reads/writes/lists them safely
//! (`engine`), detects the format and runs the actual parse (`format`),
//! and — on request, after an explicit confirmation — attempts a
//! best-effort mechanical repair of common mistakes (`fixer`).

pub mod engine;
pub mod exec;
pub mod fixer;
pub mod format;
