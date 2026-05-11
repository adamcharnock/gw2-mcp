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

## Install

> **Why native binaries?** The Mumble Link navigation tools
> (`get_my_location`, `find_nearby`, `describe_facing`, `get_directions`)
> read live in-game state from a shared-memory region the GW2 client
> writes every frame. That only works when the MCP server runs on the
> **same host** as the game — Docker containers can't see the host's
> shared memory. The pre-built binaries are therefore the recommended
> way to run gw2-mcp; the Docker image is retained for headless / API-
> only deployments where Mumble Link doesn't matter.

Download the binary for your platform from the latest
[GitHub Release](https://github.com/adamcharnock/gw2-mcp/releases/latest):

| OS                | Architecture           | Archive                                                   |
|-------------------|------------------------|-----------------------------------------------------------|
| Linux             | x86_64                 | `gw2-mcp-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`          |
| Linux             | aarch64 (arm64 / Pi)   | `gw2-mcp-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`         |
| Windows           | x86_64                 | `gw2-mcp-vX.Y.Z-x86_64-pc-windows-msvc.zip`               |
| macOS             | aarch64 (Apple Silicon)| `gw2-mcp-vX.Y.Z-aarch64-apple-darwin.tar.gz`              |
| macOS             | x86_64 (Intel)         | `gw2-mcp-vX.Y.Z-x86_64-apple-darwin.tar.gz`               |

Each archive contains a single `gw2-mcp` (or `gw2-mcp.exe`) binary plus a
sibling `.sha256` checksum. **macOS tarballs additionally ship
`gw2-mcp-holder.exe`** (the in-bottle Mumble Link helper); leave it next to
the main binary — the server discovers it as a sibling and copies it into
your CrossOver bottle automatically. See [macOS / CrossOver specifics](#macos--crossover-specifics)
below for details.

On Linux / macOS:

```bash
tar -xzf gw2-mcp-vX.Y.Z-<triple>.tar.gz
./gw2-mcp --version
```

On macOS, the first run may be blocked by Gatekeeper — clear the
quarantine attribute on both binaries:

```bash
xattr -d com.apple.quarantine ./gw2-mcp ./gw2-mcp-holder.exe
```

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

Or with the Docker image (no Mumble Link — see note above):

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

### Docker image (secondary)

For headless / containerised deployments where the Mumble Link
navigation tools aren't needed, an image is published to **GitHub
Container Registry** on every push to `main` and on `vX.Y.Z` tags:

- Image: `ghcr.io/adamcharnock/gw2-mcp`
- Architectures: `linux/amd64`, `linux/arm64` (Apple Silicon native)
- Base: distroless `cc-debian12:nonroot` (~25 MB, runs as non-root, no shell)
- Provenance + SBOM attestations are published alongside each manifest

Available tags:

| Tag                      | When                                    |
|--------------------------|-----------------------------------------|
| `latest`                 | Latest push to `main`                   |
| `main`                   | Latest push to `main`                   |
| `X.Y.Z`, `X.Y`           | Pushed when a `vX.Y.Z` git tag lands    |
| `sha-<short>`            | Every commit (use this for hard pinning) |

For production deployments, prefer pinning to a `vX.Y.Z` or `sha-<short>` tag
rather than `latest`.

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
| `get_my_location`       | —                 | —                                                   | Live position + map + facing direction via Mumble Link. Requires GW2 running on the same host. |
| `get_directions`        | `from`, `to`      | —                                                   | Bearing (16-point compass) + distance between two points. Each accepts `{coords:[x,y]}`, `{poi_name, map_id}`, or `{here:true}`. |
| `find_nearby`           | —                 | `filter` (waypoint/poi/vista/hero_point/task/any), `around`, `limit` | Closest POIs to a point (defaults to player's location). |
| `describe_facing`       | —                 | —                                                   | Plain-English description of which way the player is facing + nearest landmark in that direction. |
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

## Navigation (live position via Mumble Link)

The four navigation tools (`get_my_location`, `get_directions`, `find_nearby`,
`describe_facing`) read live in-game state from the Guild Wars 2 client over
[Mumble Link](https://wiki.guildwars2.com/wiki/API:MumbleLink), a shared-memory
region the game writes every frame.

| Host | Mechanism | Status |
|------|-----------|--------|
| Windows | Named file mapping `MumbleLink` via `OpenFileMappingW` | Works |
| Linux / Steam Proton | `/dev/shm/MumbleLink` (tmpfs) | Works |
| macOS (CrossOver) | Bundled `gw2-mcp-holder.exe` runs in-bottle via `cxstart`, pre-creates the `MumbleLink` Section, mirrors snapshots to `<bottle>/drive_c/users/Public/gw2-mcp/mumble.bin`, which the macOS server reads. **Auto-managed** — no extra setup. | Works |
| macOS (Whisky) | Same approach as CrossOver, launched via Whisky's bundled `wine64` with `WINEPREFIX=<bottle>`. **Auto-managed** — no extra setup. | Works |
| macOS (Parallels VM) | Not reachable from the host | Use the Windows side directly |
| Docker / headless | Not applicable | Pass `--no-mumble-link` to silence the auto-probe |

The server **never fails** at startup when no Mumble Link is reachable —
it wires a stub that returns a clear "not connected" error from the four
navigation tools while everything else keeps working. Pass `--no-mumble-link`
to deliberately disable Mumble Link (useful in Docker, CI, headless deployments).
Pass `--no-mumble-holder` (macOS only) to skip the in-bottle holder spawn
without disabling the reader, useful if you're managing the holder yourself.

### macOS / CrossOver & Whisky specifics

GW2's Mumble Link writer is *opener-only* — it writes to the named mapping
if it exists but never creates one. On Windows the Mumble voice client (or
anything else) creates it; on Linux/Wine, Burrito and jokolink play that
role. On macOS no one would, so the macOS tarball ships a tiny helper
(`gw2-mcp-holder.exe`) that the server launches inside your bottle
automatically.

What the supervisor does on first launch:

1. **Auto-discovers** any CrossOver or Whisky bottle that contains
   `Gw2-64.exe` at the standard `Program Files` install location. No
   specific bottle name is required. CrossOver bottles are preferred when
   both runners contain GW2. Override with `GW2_BOTTLE="My Bottle Name"`
   if auto-discovery picks the wrong one (or if GW2 is installed in a
   non-standard path inside an otherwise-recognisable bottle).
2. Copies `gw2-mcp-holder.exe` (the sibling file in the tarball) into
   `<bottle>/drive_c/users/Public/gw2-mcp/holder.exe` (replacing it on
   sha256 mismatch so a fresh release ships an updated holder transparently).
3. Spawns it via `cxstart --bottle <name> --no-wait …` for CrossOver, or
   via Whisky's bundled `wine64` with `WINEPREFIX=<bottle-root>` for Whisky.
4. The holder runs for the lifetime of `gw2-mcp` and is killed on exit.

**Prerequisites**: CrossOver *or* Whisky installed, with Guild Wars 2 in
a bottle. The supervisor doesn't install GW2 — it just plugs into your
existing bottle.

If anything fails (no CrossOver/Whisky, no bottle, missing launcher), the
server logs a warning and continues without nav-tool support — every other
tool keeps working. Run `gw2-mcp doctor` (see below) for a structured
diagnosis.

### Diagnostics and config helpers

Two zero-server-startup subcommands help with setup:

```bash
gw2-mcp doctor          # macOS Mumble Link diagnostics — prints a checklist
gw2-mcp print-config    # emit a Claude Desktop config snippet for this binary
```

`doctor` reports each step independently (CrossOver detected? Whisky detected?
bottles found? GW2 located? holder installed? mirror file fresh? GW2 actually
writing live frames?) so you can see exactly where things fall over. It exits
non-zero if any step fails — handy in shell pipelines.

`print-config` writes ready-to-paste JSON for `~/Library/Application Support/Claude/claude_desktop_config.json`:

```bash
gw2-mcp print-config | pbcopy   # macOS — copy straight to clipboard
gw2-mcp print-config --api-key "AAA...-BBB...-CCC..." --bottle "My Bottle"
```

`--api-key` and `--bottle` are optional; they inject `GW2_API_KEY` /
`GW2_BOTTLE` into the snippet's `env` block.

### Coordinate convention

GW2 map coordinates use a Y-down convention (Y grows southward, like screen
coords). The `bearing` math handles the inversion internally; literal coords
passed to `get_directions` should match what `/v2/continents/.../maps/{id}`
returns. Distances are reported in raw GW2 units (~1 inch each) and metres.

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
