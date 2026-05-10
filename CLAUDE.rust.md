# Rust Guidelines

Read at the start of every Rust session. Opinionated guide for small-to-medium Rust projects — single crate or thin workspace, CLI / service / daemon scale. American English throughout (`organize`, `color`, `toward`).

## Philosophy — use the compiler

Rust's strictness is a feature, not something to work around. Make illegal states unrepresentable. Push validation from runtime to compile time. Don't paper over compiler errors with `.clone()`, `.unwrap()`, or `&'static` lifetime hacks — rethink the type.

Functional core, imperative shell. Pure logic lives in `domain/`; IO lives at the edges in `adapters/`. The service layer orchestrates ports without ever importing concrete adapters.

## Project shape

Default to a single crate. Reach for a workspace only when you actually have a second binary or a genuinely shared library.

```
src/
  domain/        Pure types: entities, validation, no IO
  ports.rs       Trait defs: repositories, external clients, clock
  adapters/      Concrete implementations of ports
  service.rs     Orchestrate ports — business logic without IO
  main.rs        clap wiring; the only place that picks concrete adapters
migrations/      sqlx migrations, from day one
```

Domain code never imports `adapters/`. Dependencies flow inward. New transports — a daemon mode, an MCP server, an HTTP handler — are additional adapters wrapping the same service. If you ever add one and it feels like a free lunch, the architecture worked.

## Domain modeling

Newtypes for anything that could be confused with another `String` / `Uuid` / `PathBuf`:

```rust
pub struct UserId(Uuid);
pub struct EmailAddress(String);
pub struct ConfigPath(PathBuf);
```

Validate in constructors and return `Result<Self, DomainError>`. After construction, the value is trusted — no re-checking at usage sites. Two `PathBuf` arguments to one function is a bug waiting to happen.

## Errors

Libraries return `Result`. Panics only in `main` and tests. One enum per layer with `thiserror`:

- `DomainError` — validation failures
- `RepositoryError` — storage failures (one per repository if you have several)
- `ServiceError` — orchestration; wraps repo errors and external client errors

Rules from real bugs:

1. **`String` in error variants, never `&'static str`.** Static strings force a new variant for every bit of context.
2. **Add context at every `?`.** Say *what* failed on *which* resource: `.context(format!("Failed to fetch user {}", id))?`
3. **Preserve chains** with `#[source]`. Never `.to_string()` an error before re-wrapping.
4. **Include identifying info** — IDs, paths, names.
5. **Never silently swallow.** `.ok()`, `.unwrap_or_default()`, `let _ =` hide bugs. Use a `match` that logs with `warn!(error = ?e, ...)` before any fallback.
6. **Logging:** `error = ?e` (Debug) in WARN/ERROR preserves the chain. `error = %e` (Display) shows only the outermost — INFO only.

## SQLite

Default storage. Use `sqlx` with the sqlite feature. Migrations in `migrations/` from day one, even with one file — you will change the schema.

**Idempotency** — every meaningful operation is safe to repeat:

```rust
sqlx::query!(
    "INSERT INTO items (id, content_hash, ...) VALUES (?, ?, ...)
     ON CONFLICT (content_hash) DO NOTHING",
    ...
)
```

When entities have natural identity (content hash, email, external ID), put a unique constraint on it and use `ON CONFLICT`. This eliminates check-then-act races without locks.

**Source of truth.** When data lives in two places — filesystem and DB, cache and DB, two services — pick a winner on day one and write down which it is. Treat the loser as a derived view that can be rebuilt. Source-of-truth ambiguity will haunt you for the rest of the project.

## External services

Wrap every external call — HTTP API, LLM, email, queue — behind a port. The implementation in `adapters/` is the only place that touches `reqwest` or knows about API keys. The service layer takes `Arc<dyn ExternalClient>` and never imports the underlying transport.

This pays back twice: (1) it's the slow, expensive, flaky boundary you'll want to mock in tests, (2) swapping providers or running in offline mode becomes a one-file change.

When the external service returns structured data, ask for it structured. Use tool use / structured outputs / typed responses. Do not parse free-form text.

## Clock

```rust
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
```

`SystemClock` in production, `MockClock` in tests. Inject as `Arc<dyn Clock>` (or generic `<C: Clock>`). One trait, deterministic tests for anything date-related — and "anything date-related" is more code than you think.

## Testing

- **Unit tests** inline with `#[cfg(test)] mod tests`. Cover domain types and pure logic without IO.
- **Integration tests** in `tests/`. Use `tempfile::TempDir` for filesystem tests; `SqlitePool::connect(":memory:")` or a tempdir DB for repo tests. Fast, no docker.
- **Mock external clients** for service tests. Never hit a real API from `cargo test`.
- **At least one end-to-end test** per workflow: real fixture in, real artifacts out, real DB rows.

Do not mock the database. Mocked DB tests pass while migrations break — and migrations breaking is exactly the failure that bites you in week two.

## CLI

`clap` with derive macros. Support `--format json|table` everywhere that prints results. The CLI is a thin shell over `service.rs` — no business logic in command handlers, no `reqwest` or `sqlx` imports there either.

## Naming

| Item | Convention | Example |
|------|------------|---------|
| Types, traits, enums | `UpperCamelCase` | `UserRepository` |
| Functions, modules | `snake_case` | `fetch_user` |
| Constants | `SCREAMING_SNAKE_CASE` | `MAX_BODY_BYTES` |
| Getters | no `get_` prefix | `fn name(&self)` |
| Conversions | `as_*` cheap / `to_*` owned / `into_*` consuming | `as_str`, `to_path_buf`, `into_inner` |
| Repository traits | `*Repository` | `UserRepository` |
| SQLite impls | `Sqlite*` | `SqliteUserRepository` |

## Anti-patterns

- **Don't pre-build features that aren't asked for.** No auth, RBAC, audit logging, rate limiting, multi-user support, REST API, or web UI unless they're requirements. Leave a `TODO` with one line of context if you're tempted.
- **No `Option<Option<T>>`** for set/clear/no-change. Use a three-variant enum.
- **No swallowed errors.** Every `Result` is propagated, matched, or carries a comment explaining why ignoring is safe.
- **No mocking the database.** Use a real SQLite — `:memory:` or tempdir.
- **No check-then-act.** `if !exists { create }` races. Use `ON CONFLICT DO NOTHING`.

## Documenting decisions

When you make a non-obvious choice — content-hash dedup over filename, ingest-time work over query-time, filesystem as source of truth over DB — leave a brief comment with the *why*:

```rust
// Hash on file bytes (not path or mtime) so the same upload arriving via two
// channels dedupes to one row regardless of how it was named.
fn content_hash(bytes: &[u8]) -> ContentHash { ... }
```

Future-you and future-agent will both happily undo intentional decisions if the reasoning isn't in the code.

## Pre-commit checklist

- [ ] `cargo fmt`
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo test` passes
- [ ] No `.ok()` or `.unwrap_or_default()` without a comment
- [ ] Error variants are `String`, not `&'static str`
- [ ] New mutations are idempotent (re-running the command is safe)
- [ ] No `TODO` without a one-line explanation
