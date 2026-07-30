//! SSL Certificate Manager: finds whatever's serving TLS on port 443 on a
//! local-or-remote host (`detect`), reading straight out of the live
//! nginx/apache config so domains and cert/key/CA paths are never
//! guessed, and lets the user push a replacement certificate (and,
//! separately, a replacement CA/chain file) through a config-test-before-
//! reload safety net (`engine`). The TUI screen
//! (`tui::sslcert_screen`) is the only consumer of both.

pub mod detect;
pub mod engine;
pub mod exec;
