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
| `search_skills`         | `query` (≥2)      | `limit` (≤50), `profession`, `slot`, `weapon_type`  | Fuzzy name search over the local skills index. |
| `search_traits`         | `query` (≥2)      | `limit`, `specialization`, `tier`                   | Fuzzy name search over the local traits index. |
| `search_specializations`| `query` (≥2)      | `limit`, `profession`, `elite`                      | Fuzzy name search over the local specializations index. |
| `search_items`          | `query` (≥2)      | `limit`, `type`, `rarity`, `min_level`, `max_level`, `weight_class` | Fuzzy name search over the local items index. **Opt-in** — only populated when started with `--with-items`. |
| `search_achievements`   | `query` (≥2)      | `limit`, `type`                                     | Fuzzy name search over the local achievements index. |
| `get_index_status`      | —                 | —                                                   | Per-kind row counts, last-refreshed timestamps, build number stamped on the index. |

### Build-source coverage

| Source       | Coverage                | Mechanism                                |
|--------------|-------------------------|------------------------------------------|
| `discretize` | Fractals (T4 + CMs)     | GitHub raw markdown + YAML front-matter  |
| `metabattle` | All gamemodes (Meta tier) | MediaWiki API                          |
| `snowcrows`  | Raids/strikes meta      | On-demand HTML scrape (no bulk listing — respects `ai-train=no`); slug shape `<category>/<profession>/<build-slug>` |

Resource: `gw2://currencies` — full currency list as JSON.

## Search index

The `search_*` tools are backed by an on-disk SQLite index (FTS5 with
diacritic-folded `unicode61` tokeniser). On first launch the server
spawns a background task that enumerates every skill / trait /
specialization / achievement via the GW2 API and populates the index.
Subsequent searches are fully local — no upstream calls.

**Cache location** (override with `--cache-dir <path>` or
`GW2_CACHE_DIR=...`):

| OS      | Default path                                                |
|---------|-------------------------------------------------------------|
| macOS   | `~/Library/Caches/net.adamcharnock.gw2-mcp/index.sqlite`    |
| Linux   | `~/.cache/gw2-mcp/index.sqlite`                             |
| Windows | `%LOCALAPPDATA%\adamcharnock\gw2-mcp\cache\index.sqlite`    |

**Population time** on first launch (over a typical home connection):
- Skills: ~30 s
- Traits + specializations + achievements: ~30 s combined
- **Items (opt-in via `--with-items`)**: ~5 minutes — ~85k entries.

**Disk usage**:
- Without items: ~5–10 MB
- With items: ~50 MB

**Cache invalidation**: the indexer stamps each refresh with the GW2
build number returned by `/v2/build`. On startup it asks the API for the
current build; if it matches the stamp, no refresh runs. Game patches
(which always bump the build number) trigger a fresh re-index. Force a
full rebuild any time with `--rebuild-index`.

**CLI flags**:
- `--cache-dir <path>` — override the cache directory.
- `--no-search-index` — disable entirely; `search_*` tools return a
  `SearchDisabled` error. Useful in ephemeral / read-only environments.
- `--with-items` — include items in the background pass (off by
  default).
- `--rebuild-index` — force a full re-index on startup.

**Docker note**: bind-mount a host directory into the container so the
index survives across restarts, e.g.

```bash
docker run --rm -v "$HOME/.cache/gw2-mcp:/cache" \
  -e GW2_CACHE_DIR=/cache gw2-mcp
```

While the index is still populating, `search_*` calls return a typed
"still indexing" error so the LLM can switch to `get_*` (which works
with explicit ids) or retry shortly.

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
