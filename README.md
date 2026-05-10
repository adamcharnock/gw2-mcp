# gw2-mcp

Model Context Protocol (MCP) server that exposes Guild Wars 2 wiki search,
wallet, and currency data to LLM clients (Claude Desktop, LM Studio, Cursor,
etc.). Written in Rust, single binary, stdio transport.

## Features

- **Wiki search** — search the GW2 wiki, with prose extracts auto-fetched per hit.
- **Wallet** — read an account's wallet (requires a GW2 API key with `wallet` scope).
- **Currencies** — full or filtered currency metadata.
- **Smart caching** — long TTL for static data (currencies, wiki), short TTL for wallet.

## Architecture

Hexagonal:

```
src/
  domain/      Pure types and validation (no IO).
  ports.rs     Trait definitions: Cache, Clock, Gw2Api, Wiki.
  service.rs   Orchestration. Knows ports, never adapters.
  adapters/    Concrete impls: HTTP, in-memory cache, system clock, MCP/stdio.
  main.rs      CLI wiring — the only place that picks adapters.
tests/         Integration tests (wiremock for HTTP, in-memory fakes for service).
```

The service depends only on traits, so adding a new transport (HTTP/SSE,
daemon mode) is a one-file change in `adapters/`.

## Quick start

Requires [mise](https://mise.jdx.dev) (or Rust 1.91+ directly).

```bash
mise install              # install pinned Rust toolchain + tools
mise run install-hooks    # install git hooks (lefthook)
mise run build            # cargo build
mise run test             # cargo test --all-targets
mise run check-all        # fmt-check + clippy + test
```

Run the server (it speaks MCP over stdio):

```bash
mise run run
```

### Local secrets (.env)

`mise` auto-loads variables from a `.env` file in the repo root whenever you
`cd` into the project. Copy the example and fill in your GW2 API key for
local testing:

```bash
cp .env.example .env
# edit .env, set GW2_API_KEY=...
```

`.env` is gitignored. The example file documents every variable the binary
understands.

## MCP client config

```json
{
  "mcpServers": {
    "gw2-mcp": {
      "command": "/path/to/gw2-mcp"
    }
  }
}
```

Or with the Docker image:

```json
{
  "mcpServers": {
    "gw2-mcp": {
      "command": "docker",
      "args": ["run", "--rm", "-i", "ghcr.io/adamcharnock/gw2-mcp:latest"]
    }
  }
}
```

## Tools

| Tool                    | Required args     | Optional args                                       | Notes |
|-------------------------|-------------------|-----------------------------------------------------|-------|
| `wiki_search`           | `query`           | `limit` (1–50, default 5)                           | GW2 wiki search with prose extracts. |
| `get_wallet`            | —                 | `api_key` (falls back to `GW2_API_KEY`)             | Account wallet + currency metadata. |
| `get_currencies`        | —                 | `ids` (array, max 200; omit for all)                | Currency definitions. |
| `get_skills`            | `ids` (array)     | `summary` (default true)                            | Resolve skill ids to name + description (+ facts when `summary=false`). Max 200 ids/call. |
| `get_traits`            | `ids` (array)     | `summary` (default true)                            | Resolve trait ids. Max 200 ids/call. |
| `get_specializations`   | `ids` (array)     | `summary` (default true)                            | Resolve specialization ids (core + elite). Max 200 ids/call. |
| `get_items`             | `ids` (array)     | —                                                   | Resolve item / equipment ids. Max 200 ids/call. |
| `get_character_build`   | `character`       | `api_key`, `tab` ("active"/"all"/index)             | Full per-tab build + equipment for a character with names pre-resolved (needs `builds` scope). |
| `decode_build_code`     | `code` (`[&...]`) | —                                                   | Decode a build chat code into structured JSON. Resolved palette → API skill ids and trait positions → trait ids. No auth. |
| `list_catalog_sources`  | —                 | —                                                   | Lists registered curated-build catalog sources. |
| `list_catalog_builds`   | `source`          | `profession`, `gamemode`, `page_size` (≤100), `cursor` | Browse a curated source. Cursor-based pagination; pass back `next_cursor`. |
| `get_catalog_build`     | `source`, `slug`  | —                                                   | Fetch full details for a curated build. |
| `get_info`              | —                 | —                                                   | Returns the server's usage runbook (same as `initialize.instructions`). |
| `get_account`           | —                 | `api_key` (falls back to `GW2_API_KEY`)             | Account snapshot: name, world, age, expansion access, fractal level, AP, WvW rank. Needs `account` scope. |
| `list_characters`       | —                 | `api_key`                                           | Just the character names on the account. Cheap. Needs `characters` scope. |
| `get_account_achievements` | —              | `api_key`, `summary` (default true)                 | Per-account achievement progress. Summary mode drops completed + not-started entries. Needs `account` + `progression` scopes. |
| `get_account_masteries` | —                 | `api_key`                                           | Mastery track levels. Needs `account` + `progression` scopes. |
| `get_account_raids`     | —                 | `api_key`                                           | Raid encounter ids cleared this reset week (resets Mondays). Needs `account` + `progression` scopes. |
| `get_account_dungeons`  | —                 | `api_key`                                           | Dungeon-path ids cleared today (resets daily, not weekly). Needs `account` + `progression` scopes. |
| `get_dailies`           | —                 | `which` (`today` / `tomorrow`, default today)       | Today's or tomorrow's daily achievement IDs partitioned by category. Public — no key needed. |

### Build-source coverage

| Source       | Coverage                | Mechanism                                |
|--------------|-------------------------|------------------------------------------|
| `discretize` | Fractals (T4 + CMs)     | GitHub raw markdown + YAML front-matter  |
| `metabattle` | All gamemodes (Meta tier) | MediaWiki API                          |
| `snowcrows`  | Raids/strikes meta      | On-demand HTML scrape (no bulk listing — respects `ai-train=no`); slug shape `<category>/<profession>/<build-slug>` |

Resource: `gw2://currencies` — full currency list as JSON.

## Getting a GW2 API key

1. https://account.arena.net/applications
2. Create a key with `account` and `wallet` permissions (add `characters` +
   `builds` if you want `get_character_build`).
3. Either pass the key per-call as the `api_key` argument, **or** set
   `GW2_API_KEY=...` in your environment / `.env` file. When set, the server
   uses it as the default for `get_wallet` and `get_character_build`; explicit
   `api_key` arguments still override.

The key is hashed before caching and redacted in logs; the raw value never
reaches the cache or appears in stderr output.

## Development

| Command             | What it does |
|---------------------|--------------|
| `mise run fmt`      | `cargo fmt --all` |
| `mise run clippy`   | `cargo clippy --all-targets --all-features -- -D warnings` |
| `mise run test`     | `cargo test --all-targets` |
| `mise run audit`    | `cargo audit` |
| `mise run coverage` | HTML + text coverage via `cargo-llvm-cov` |
| `mise run check-all` | fmt-check + clippy + test |

Pre-commit hooks (via lefthook) gate on: rejecting unsigned commits, gitleaks,
`cargo fmt`, `cargo clippy -D warnings`, and `cargo test`.

## Logging

Logs go to **stderr only** — stdout is reserved for the MCP protocol.
Set `RUST_LOG` to control verbosity (e.g. `RUST_LOG=gw2_mcp=debug`).

## License

GNU Affero General Public License v3.0 — see [LICENSE](LICENSE).
