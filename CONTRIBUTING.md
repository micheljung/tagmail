# Contributing to Tagmail

Thanks for your interest in Tagmail. Please open an issue to discuss larger
changes before starting on them. [docs/architecture.md](docs/architecture.md)
gives an overview of the code.

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

The screenshots in `docs/images/` come from the end-to-end test mock.
After a visible UI change, regenerate them with
`pnpm --filter @tagmail/web run build` and
`pnpm --filter @tagmail/web run screenshots`.

## Commits

- No conventional-commit prefix (`feat:`, `fix:`, ...)
- Title starts with an uppercase letter and is at most 72 characters
- Keep commits small and focused; add a body only when the change is not
  obvious

All contributions are licensed under the AGPL-3.0, the project's license.

## Required checks

Branch protection on `main` requires these checks, which report on every
pull request:

- CI: `rust`, `web`, `required-files`, `docs`
- E2E: `seed-readable`, `engine-dovecot`, `web-playwright`, `desktop-linux`
- `desktop-result`, `docker-result`, `nix-result`, `release-result`,
  `servers-result`

Desktop, Docker, Nix, Release and Servers skip their builds when a pull
request does not touch their paths; their `*-result` job passes when every
job passed or was skipped. `docs-external` is informational.

## Releases

Set the same version in `Cargo.toml` (workspace), `desktop/src-tauri/tauri.conf.json`,
`web/package.json` and `packages/api-types/package.json`, update `Cargo.lock`, and
push a `v<version>` tag. The **Release** workflow then publishes the image,
the server tarballs, the desktop installers and `SHA256SUMS`. Pre-release
versions must be numeric (`1.2.0-1`, not `1.2.0-rc.1`), because the Windows MSI
bundler rejects anything else; they are published as GitHub prereleases and
do not move the `latest` image tag. Before a release, run
[the desktop checklist](docs/dev/release-checklist.md).
