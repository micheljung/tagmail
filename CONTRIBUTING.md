# Contributing to Tagmail

Thanks for your interest in Tagmail. The project is pre-alpha, so please open
an issue to discuss larger changes before starting on them.

## Toolchain

- Rust (the version is pinned in `rust-toolchain.toml`; `rustup` installs it
  automatically)
- Node.js 22 or newer
- pnpm (the version is pinned in `package.json`; enable it with
  `corepack enable`)
- Docker, for integration tests against real IMAP servers

## Rust

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Development tasks live in the `xtask` crate:

```bash
cargo xtask --help
```

## Web

```bash
pnpm install
pnpm run lint
pnpm run check
pnpm run test
pnpm run build
pnpm --filter @tagmail/web dev
```

## Commits

- No conventional-commit prefix (`feat:`, `fix:`, ...)
- Title starts with an uppercase letter and is at most 72 characters
- Keep commits small and focused; add a body only when the change is not
  obvious

All contributions are licensed under the AGPL-3.0, the project's license.
