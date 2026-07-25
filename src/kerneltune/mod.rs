//! Linux kernel tuning: a curated best-practices catalog of sysctl/sysfs/
//! ulimit tunables (`catalog`), a local-or-remote command runner (`exec`),
//! the apply/persist/revert logic (`engine`), and a local undo log
//! (`store`). The TUI screen (`tui::kerneltune_screen`) is the only
//! consumer of all four.

pub mod catalog;
pub mod engine;
pub mod exec;
pub mod store;
