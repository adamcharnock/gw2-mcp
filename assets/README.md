# Vendored assets

## `professions_palette.json`

Per-profession palette-id → skill-id mapping. Used by
`adapters/build_code_chatr.rs` to enrich decoded build chat codes with the
real GW2 API skill IDs (the chat-code binary format only carries palette
IDs, not skill IDs).

**Source:** `chatr` crate, `src/professions.json`. Dual-licensed MIT/Apache-2.0.

**Why vendor:** The GW2 `/v2/professions` endpoint dropped its
`skills_by_palette` field, so there's no first-party way to fetch this
mapping. `chatr` ships it bundled.

**Refresh:**

```bash
# Make sure the chatr crate is installed in your local cargo cache:
cargo build
# Then copy:
cp $(find ~/.cargo/registry/src -type d -name 'chatr-*' | head -1)/src/professions.json \
   assets/professions_palette.json
```

Bump cargo's `chatr` dep before refreshing if you want post-SotO additions.
