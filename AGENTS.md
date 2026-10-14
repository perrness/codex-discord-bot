# Repository Guidelines

## Project Structure & Module Organization
This Linux Rust service connects Discord to a local Codex App Server. `src/main.rs` bootstraps the service; `src/lib.rs` exposes modules. Discord commands, rendering, and delivery live in `src/discord/`; protocol and transport live in `src/codex/`. Session lifecycle and recovery use `src/sessions.rs` and `src/sessions/`. PostgreSQL access and schema live in `src/persistence.rs` and `migrations/`. Integration tests are in `tests/`; deployment files are in `deploy/` and `scripts/`. Read `docs/protocol.md` before changing protocol or recovery behavior.

## Build, Test, and Development Commands
Use Rust 1.89+ (edition 2024), Git, and PostgreSQL 14+ for runtime.

- `cargo build --release --locked`: build the release binary using `Cargo.lock`.
- `cargo run --locked -- run`: start locally with configured environment as a non-root user.
- `cargo run --locked -- migrate`: apply embedded migrations; requires only `DATABASE_URL`.
- `cargo test --locked`: run default tests without Discord or PostgreSQL.
- `cargo fmt --all -- --check`: verify formatting.
- `cargo clippy --all-targets --locked -- -D warnings`: reject lint warnings.

## Coding Style & Naming Conventions
Follow rustfmt with four-space indentation. Use `snake_case` for modules, functions, and tests; `PascalCase` for types; `SCREAMING_SNAKE_CASE` for constants. Keep Tokio operations asynchronous and add context to fallible operations with `anyhow::Context`.

## Testing Guidelines
Use Rust’s `#[test]` and `#[tokio::test]`. Place unit tests in each module’s `#[cfg(test)]` block and name tests after expected behavior. No numeric coverage threshold is configured; cover changed behavior and failure paths.

- PostgreSQL: set `TEST_DATABASE_URL` to a disposable database with `CREATE SCHEMA` permission, then run `cargo test --test integration --locked -- --include-ignored`.
- Protocol: set `CODEX_TEST_BIN` to Codex CLI 0.162.1, then run `cargo test --test codex_smoke --locked -- --include-ignored`.

## Commit & Pull Request Guidelines
Git history is unavailable in this checkout, so commit conventions cannot be verified. Use concise, imperative subjects such as `Fix stale approval handling`. PRs should explain behavior changes, link relevant issues, report checks run, and document migration or configuration changes.

## Security & Configuration
Use `.env.example` as the configuration reference; environment files are not loaded automatically. Never commit credentials. Keep `STATE_DIR` and `CODEX_HOME` outside `REPOS_ROOT`. Preserve repository locking, approval correlation, and recovery without automatic task replay.
