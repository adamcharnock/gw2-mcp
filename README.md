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

| Tool                      | Required args              | Optional args                              | Notes |
|---------------------------|----------------------------|--------------------------------------------|-------|
| `wiki_search`             | `query`                    | `limit` (1–50, default 5)                  | GW2 wiki search with prose extracts. |
| `get_wallet`              | `api_key`                  | —                                          | Account wallet + currency metadata. |
| `get_currencies`          | —                          | `ids` (array; omit for all)                | Currency definitions. |
| `get_skills`              | `ids` (array)              | —                                          | Resolve skill ids to name + description + facts. |
| `get_traits`              | `ids` (array)              | —                                          | Resolve trait ids. |
| `get_specializations`     | `ids` (array)              | —                                          | Resolve specialization ids (core + elite). |
| `get_character_build`     | `api_key`, `character`     | —                                          | Full per-tab build + equipment for a character (needs `builds` scope). |
| `decode_build_code`       | `code` (`[&Dw…]`)          | —                                          | Decode any build chat code into structured JSON. No auth. |
| `list_build_sources`      | —                          | —                                          | Lists registered curated-build sources. |
| `list_recommended_builds` | `source`                   | `profession`, `gamemode`, `limit`          | Browse a curated source. |
| `get_recommended_build`   | `source`, `slug`           | —                                          | Fetch full details for a curated build. |

### Build-source coverage

| Source       | Coverage                | Mechanism                                |
|--------------|-------------------------|------------------------------------------|
| `discretize` | Fractals (T4 + CMs)     | GitHub raw markdown + YAML front-matter  |
| `metabattle` | All gamemodes (Meta tier) | MediaWiki API                          |
| `snowcrows`  | Raids/strikes meta      | On-demand HTML scrape (no bulk listing — respects `ai-train=no`); slug shape `<category>/<profession>/<build-slug>` |

Resource: `gw2://currencies` — full currency list as JSON.

## Getting a GW2 API key

1. https://account.arena.net/applications
2. Create a key with `account` and `wallet` permissions.
3. Pass it to the `get_wallet` tool. The key is hashed before caching; the raw
   value never reaches the cache or logs.

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
