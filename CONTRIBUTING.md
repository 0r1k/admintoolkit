# Contributing to atk

Thanks for considering a contribution. `atk` is a single Rust binary, so the
bar to get a working dev loop going is low.

## Getting set up

```bash
git clone https://github.com/0r1k/admintoolkit.git
cd admintoolkit/admintk
cargo build
./target/release/atk       # after cargo build --release
```

No install script, no external services required to build — only a
reasonably recent stable Rust toolchain.

## Project layout

Each tool lives in its own top-level module (`easyssh_mgr/`, `sshuser/`,
`cloudflare/`, `godaddy/`, `mysql_mgr/`, `postgres_mgr/`, `clickhouse/`,
`logs_mgr/`, `kerneltune/`) that owns its config/API/business logic, and a
matching screen under `tui/` (`*_screen.rs`) that owns rendering and input
handling for that tool. `tui/mod.rs` holds the `Screen` enum and dispatches
key/mouse events and draw calls to whichever screen is active; `tui/home.rs`
is the launcher menu. Shared widgets, mouse hit-testing helpers, the host
picker, and the file picker live directly under `tui/`.

If you're adding a new tool, that same shape — a logic module plus a
`*_screen.rs` — is the expected pattern; wire it into `Screen`, `App`, and
the home menu's `DEFAULT_ITEMS` the same way the existing tools are.

## Before opening a PR

- `cargo build --release` should succeed with no warnings.
- `cargo fmt` for formatting.
- Keep keybindings consistent with the rest of the app (`Tab`/`Shift+Tab`
  between fields, `Esc` backs out one level, `F1`-`F4` for sub-tabs where a
  tool has them) — see the README's "Shared across all nine" section for
  the conventions every screen follows.
- If your change touches how a value is written to a live system (kernel
  parameters, DNS records, database users, SSH config), be conservative:
  prefer runtime-only/reversible operations, and never write to a shared
  config file without a backup step, matching how existing modules handle
  it.

## Reporting bugs / requesting features

Open an issue using the provided templates. For anything security-related,
see [SECURITY.md](SECURITY.md) instead of a public issue.

## License

By contributing, you agree that your contributions will be licensed under
the project's [GPL-3.0 license](LICENSE).
