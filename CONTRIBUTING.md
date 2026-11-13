# Contributing to `zincio`

**Thank you for helping improve `zincio`!**

## Before you start

- Read the `README.md` for build and run basics.
- Follow the `CODE_OF_CONDUCT.md`.

## What to contribute

Contributions are welcome across:

- bug fixes
- new features
- performance improvements
- tests
- documentation updates

## Development setup

1. Fork the repository and clone your fork.
2. Create a branch from `main` for your changes.
3. Make your changes in focused commits.

## Repository layout

`zincio` lives in a Rust workspace (resolver "2"). Key directories include:

- `zincio/`: the main `zincio` runtime
- `zincio-hyper/`: the `zincio-hyper` crate that integrates `zincio` and `hyper` (HTTP server and client library)
- `zincio-quinn/`: the `zincio-quinn` crate that integrates `zincio` and `quinn` (QUIC server and client library)
- `examples/`: example code for `zincio`

## Build and check locally

Run from the repository root unless noted:

| Command                                                 | Purpose                                                                              |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `cargo build --workspace`                               | Build all workspace crates                                                           |
| `cargo test --workspace`                                | Run unit and inline tests                                                            |
| `cargo test -p <crate>`                                 | Run tests for a single crate                                                         |
| `cargo fmt --all --check`                               | Check formatting (the repo uses default `rustfmt` settings, no `.rustfmt.toml`)      |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint; warnings fail the check                                                        |
| `npx aislop scan`                                       | Scan for possible AI-generated issues (false positives are possible; requires `npm`) |

## Code conventions

- When you leave a stub implementation, add a `TODO` marker in the comment that explains the stub.
- When your stub implementation is intentional (because of some constraint that prevents de-stubbing), add a `STUB` marker in the comment that explains why.
- When you leave a comment about a known issue, add a `FIXME` marker.
- Some code conventions are enforced in Clippy lint configuration in `Cargo.toml` file (run `cargo clippy` to check for them)

## Pull request guidelines

- Open pull requests against `main` by default.
- Use a clear title and description explaining:
  - what changed
  - why it changed
  - how you validated it (commands you ran)
- Link related issues when applicable.
- Keep pull requests focused; separate unrelated changes.
- Ensure CI is green before requesting review.

## Commit guidance

- Commit messages follow Conventional Commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`).
- Keep commit messages descriptive and scoped.
- Avoid mixing refactors, behavior changes, and docs-only updates in one commit when possible.
- Do small, incremental commits that are easy to review, instead of large, monolithic commits.
- Update `CHANGELOG.md` under the unreleased section for user-facing `feat:` and `fix:` changes. Docs-only changes and subtle implementation details are exempt. New entries start with a "Breaking changes" section when applicable, followed by categorized sections (see `CHANGELOG.md` for the current layout). Use a bold inline header for each bullet.

## AI policy

[See Ferron's AI policy](https://github.com/ferronweb/ferron?tab=contributing-ov-file#ai-policy)
