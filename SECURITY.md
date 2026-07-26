# Security Policy

## Supported Versions

`atk` is currently pre-1.0 and distributed only as source (`main` branch).
Security fixes are only made against the latest commit on `main` — there
are no maintained release branches yet.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report it privately by emailing **admin@or1k.net** with:

- A description of the vulnerability and its potential impact
- Steps to reproduce (a minimal repro is ideal)
- Any relevant logs, config, or environment details (redact secrets/tokens)

You should get an initial response within a few days. Once a fix is ready,
it will be pushed to `main` and credited to you in the changelog/commit
message unless you ask to stay anonymous.

## Scope notes

`atk` handles sensitive material by design — SSH credentials, database
passwords, DNS API tokens, and (via the Kernel Tuner) live kernel
parameters on whatever host it's pointed at. Particularly relevant reports:

- Anything that could leak or weaken the encryption of secrets stored under
  `~/.config/admintoolkit/` (see `src/secret.rs`)
- Anything that could cause a write to a system file (`~/.ssh/config`,
  `/etc/sysctl.d/*`, etc.) beyond what the UI explicitly requested
- Command/argument injection in any code path that shells out (SSH exec,
  `sysctl`, etc.)
- Kernel Tuner behavior that persists a change to disk without the
  explicit opt-in the UI requires
