# Guild Wars 2 MCP server — runbook

You are talking to a read-only Guild Wars 2 (GW2) server. It exposes the
ArenaNet `/v2` API, the official wiki, and three curated build catalogs
(Discretize, MetaBattle, Snow Crows) as MCP tools, resources, and prompts.

## Orientation

GW2 identifies skills, traits, specializations, items, and currencies by
**positive integer ids**. The user almost never knows them — your job is to
turn their natural-language question into the right id lookups.

Every character build is a tuple of:

- one of nine **professions** (`Guardian`, `Warrior`, `Engineer`, `Ranger`,
  `Thief`, `Elementalist`, `Mesmer`, `Necromancer`, `Revenant`);
- exactly **3 specialization slots** (each with 3 trait choices, one per
  tier — Adept / Master / Grandmaster);
- **5 skills** on the bar: 1 heal, 3 utilities, 1 elite;
- **equipment**: weapons, armor, trinkets, with stat-set / sigil / rune /
  infusion choices.

There are also two skill bars per character (terrestrial + aquatic) and
multiple stored "build tabs" the user can switch between in-game.

The four entry-point id-resolution tools are `get_skills`, `get_traits`,
`get_specializations`, and `get_items`. They all take an `ids` array (max
200; matches the GW2 API's per-request cap) and return name + description
by default. Pass `summary: false` if the user actually needs the raw
`facts[]` arrays (skill coefficients, condition durations, etc.).

## Workflow recipes by question type

**"What's my character running?" / "Show me my build"** → call
`get_character_build` with `character: "<name>"`. The response already has
skill, trait, and specialization names pre-resolved (since Tier 1) — you do
not need follow-up `get_skills` / `get_traits` / `get_specializations` calls
for those. The only follow-up you typically need is `get_items` for any
equipment ids that come back unresolved. Defaults to the active tab; pass
`tab: "all"` or `tab: 1` for others.

**"Decode this `[&...]`"** (any chat code that starts with `[&`) → call
`decode_build_code` with `code: "[&...]"`. The response includes:

- `profession` (byte) and `profession_name` (string),
- `specializations[]` with `id` and per-slot `traits` carrying both the
  raw column-position and a resolved `trait_id` (since Tier 1),
- `skills.healing.terrestrial.api_skill_id`, `skills.utility[].api_skill_id`,
  etc. — the resolved `/v2/skills` ids, not the in-game palette ids.

Chain to `get_traits` and `get_skills` (single batched call each) using
those resolved ids if the user wants prose explanations.

**"What's the meta build for X?" / "Recommend me a build"** → use the
catalog tools. Pick the source by gamemode:

- `discretize` → fractals only;
- `snowcrows` → raids and strikes only;
- `metabattle` → everything else (WvW, PvP, open-world; some fractals as
  secondary).

Workflow: `list_catalog_builds(source, profession, gamemode)` to find a
build, then `get_catalog_build(source, slug)` for the full detail.
Pagination is cursor-based: pass `next_cursor` from the previous response
back as `cursor` to continue. The cursor binds to (source, profession,
gamemode) — changing any of them mid-paginate raises an error; restart
from the beginning.

**"What is X in GW2?"** (any wiki concept — bosses, achievements, story
content, mechanics) → call `wiki_search` with the user's phrase as `query`.
Each result already carries a prose `extract` from the wiki's lead section.

**"Show me my wallet"** → call `get_wallet`. Gold, gems, karma, and every
named currency the user has any of. Currency metadata is enriched inline.

**"What's currency #N?" / "All currencies"** → call `get_currencies` with
specific `ids` for individual lookups, or omit `ids` for the full list.

## Gotchas

**API keys.** Only `get_wallet` and `get_character_build` need one. The
key may be configured server-side via `GW2_API_KEY` — try the call without
`api_key` first; if it fails with "no Guild Wars 2 API key available", ask
the user for one and link them to <https://account.arena.net/applications>.
Required scopes: `account` always; add `wallet` for `get_wallet`; add
`characters` + `builds` + `inventories` for `get_character_build`. The key
is a 72-char hex string with hyphens (8-4-4-4-20-8-4-4-4-20). Treat it as
a secret — do not echo it back to the user, and do not put it in a public
chat log.

**`ids` arrays must be non-empty.** `get_skills`, `get_traits`,
`get_specializations`, and `get_items` all reject empty arrays. There are
thousands of entries in each table; do not try to enumerate. Always start
from a specific id source: `get_character_build`, `decode_build_code`, the
catalog detail responses, or the user pasting an id directly.

**Summary vs full payload.** All four id-resolution tools default to
`summary: true`, which trims ~70% of the bytes (drops `facts[]`, icon
URLs, etc.). Pass `summary: false` only when the user is asking about
something the summary leaves out — coefficients, fact-by-fact breakdowns,
icon assets.

**Slug shapes vary by catalog.** Discretize uses `<profession>/<slug>`;
MetaBattle uses `<profession>/<slug>` (slugified from the page title);
Snow Crows uses `<category>/<profession>/<slug>` (e.g.
`raids/guardian/heal-firebrand`). Use `list_catalog_sources` to discover
sources and always pass the literal `slug` you got back from
`list_catalog_builds` to `get_catalog_build`.

## Prompts (slash-commands the user can pick from a UI)

Suggest these when they fit — many clients render them as one-click
shortcuts:

- `analyze-character` — fetch a character, resolve everything, summarise.
- `compare-to-meta` — character vs the catalog meta for a gamemode.
- `decode-and-explain` — chat code → plain-English breakdown.
- `recommend-build` — profession + gamemode → top catalog pick.

## Resources (URI references)

You can return resource URIs to the client and let it fetch them:

- Single ids: `gw2://skills/{id}`, `gw2://traits/{id}`,
  `gw2://specializations/{id}`, `gw2://items/{id}`.
- Curated builds: `gw2://builds/{source}/{slug}` (e.g.
  `gw2://builds/snowcrows/raids/guardian/heal-firebrand`).
- Concrete: `gw2://currencies` (full list), `gw2://builds/discretize`,
  `gw2://builds/metabattle`, `gw2://builds/snowcrows` (per-source listings).

If the user might want to revisit something, returning a URI is cheaper
than a full inline payload.
