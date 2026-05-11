# Guild Wars 2 MCP server — runbook

You are talking to a read-only Guild Wars 2 (GW2) server. It exposes the
ArenaNet `/v2` API, the official wiki, three curated build catalogs
(Discretize, MetaBattle, Snow Crows), live in-game state via Mumble Link,
and an on-disk fuzzy search index over the GW2 reference corpus — as MCP
tools, resources, and prompts.

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

When the user asks about something **by name** ("Litany of Wrath", "Bolt"),
prefer the `search_*` tools to fish out the id — they query a local
SQLite/FTS5 index instead of round-tripping through the wiki.

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

**"How does the skill X work?"** (when the user names a skill but doesn't
have an id) → call `search_skills` with `query: "<name>"`. Filter by
`profession` if the user mentioned one. Take the top hit's `id`, feed into
`get_skills` for the full record. Same pattern for `search_traits`,
`search_specializations`, `search_items`, `search_achievements`.

**"Show me my wallet"** → call `get_wallet`. Gold, gems, karma, and every
named currency the user has any of. Currency metadata is enriched inline.

**"What's currency #N?" / "All currencies"** → call `get_currencies` with
specific `ids` for individual lookups, or omit `ids` for the full list.

**"What does my account look like?" / "What expansions do I own?"** →
`get_account` returns id, name, world, age, expansion access, fractal
level, daily/monthly AP, WvW rank, commander status. Cheap. Pair with
`list_characters` if you need the character roster (just names — call
`get_character_build` for any specific one).

**"What should I do today?"** → use the `daily-routine` prompt. Or
manually: `get_dailies` (today's PvE/PvP/WvW/fractal achievements) +
`get_account_achievements` (which dailies the player already finished) +
`get_account_raids` (this-week raid clears) + `get_account_dungeons`
(today's dungeon clears). Then **recall what you remember about this
player from previous conversations** — preferred game modes, time
budget, progression goals — and produce a checklist tuned to that.

**"What zone should I go to next?"** → use the `next-zone` prompt, or
manually: `get_character_build` (level + masteries needed) +
`get_account_masteries` (which mastery tracks they've progressed) +
`get_account_achievements` with `summary=false` filtered to map-completion
collections. Combine with **what you remember about their goals**.

**"Where am I?" / "What's around me?"** → navigation tools, only useful
when the GW2 client is running on the same host as this MCP server.
- `get_my_location` returns map + region + 2D coords + 16-point compass
  bearing the avatar is facing.
- `find_nearby` lists closest waypoints / POIs / vistas / hero points
  / renown hearts. Defaults to "around me".
- `get_directions(from, to)` for explicit point-to-point bearing +
  distance. Each endpoint accepts `{coords:[x,y]}`, `{poi_name, map_id}`,
  or `{here:true}`.
- `describe_facing` is the one-line "you're facing NE; nearest landmark
  in that direction is X" answer.

If `get_my_location` returns "Mumble Link not connected", the server
isn't on the same host as the game (e.g. a remote MCP deployment); say
so and stop offering navigation tools.

## Gotchas

**API keys.** Only `get_wallet`, `get_character_build`, and the
`get_account*` family need one. The key may be configured server-side
via `GW2_API_KEY` — try the call without `api_key` first; if it fails
with "no Guild Wars 2 API key available", ask the user for one and link
them to <https://account.arena.net/applications>. Required scopes:
`account` always; add `wallet` for `get_wallet`; `characters` + `builds`
+ `inventories` for `get_character_build`; `progression` for the
account-achievements / masteries / raids / dungeons tools. The key is
a 72-char hex string with hyphens (8-4-4-4-20-8-4-4-4-20). Treat it as
a secret — do not echo it back to the user, and do not put it in a
public chat log.

**`ids` arrays must be non-empty.** `get_skills`, `get_traits`,
`get_specializations`, and `get_items` all reject empty arrays. There are
thousands of entries in each table; do not try to enumerate. Always start
from a specific id source: `get_character_build`, `decode_build_code`, the
catalog detail responses, the search tools, or the user pasting an id
directly.

**Summary vs full payload.** All four id-resolution tools default to
`summary: true`, which trims ~70% of the bytes (drops `facts[]`, icon
URLs, etc.). Pass `summary: false` only when the user is asking about
something the summary leaves out — coefficients, fact-by-fact breakdowns,
icon assets.

**`get_account_achievements` summary mode.** Default `summary: true`
drops both completed entries (`done==true` or `current==max`) AND
not-started entries (`current==0` or absent). What's left is the
player's in-flight work — typically 100–300 entries instead of 2000–3000.
Pass `summary=false` only when checking for a specific id by hand.

**Raids vs dungeons reset cadence.** `get_account_raids` resets weekly
(every Monday 07:30 UTC). `get_account_dungeons` resets daily. The
endpoints look symmetrical but the data they return is on different
clocks — flag this when summarising weekly vs daily progress.

**Slug shapes vary by catalog.** Discretize uses `<profession>/<slug>`;
MetaBattle uses `<profession>/<slug>` (slugified from the page title);
Snow Crows uses `<category>/<profession>/<slug>` (e.g.
`raids/guardian/heal-firebrand`). Use `list_catalog_sources` to discover
sources and always pass the literal `slug` you got back from
`list_catalog_builds` to `get_catalog_build`.

**Search index may be populating.** On first server start, the
background indexer takes ~30 s for skills + traits + specializations +
achievements (~10k entries combined). Items are opt-in via a server
flag and take ~5 minutes if enabled. While a kind is still indexing,
`search_*` calls for it return a typed "still populating" error — fall
back to the typed `get_*` tools with explicit ids, or call
`get_index_status` to see progress and tell the user to retry shortly.

**Use the AI's own memory for player context.** The PvE coaching prompts
(`daily-routine`, `next-zone`, `next-collection`, `mount-progression`,
`legendary-progress`, `weekly-roundup`) are designed to **fuse live game
state with what you already remember about this player from previous
conversations** — their preferred game modes, time budget, progression
goals, mounts they've mentioned, builds they've talked about. The server
stores nothing about individual players. If you have no prior context
on the player, ASK them about their preferences before recommending —
don't guess. The whole point is that the recommendation is personal.

**Mumble Link is local-only.** `get_my_location`, `find_nearby` with
`{here:true}`, and `describe_facing` need the GW2 client running on the
same host as the MCP server. Linux/Wine, Windows, and macOS via CrossOver
are all supported; on macOS the server auto-launches a small in-bottle
helper (`gw2-mcp-holder.exe`) that pre-creates the Mumble Link mapping
GW2 writes into. The other navigation tools (`get_directions` with
explicit coords or named POIs, `find_nearby` with `around: {coords:[x,y]}`)
work everywhere.

## Prompts (slash-commands the user can pick from a UI)

Suggest these when they fit — many clients render them as one-click
shortcuts:

- `analyze-character` — fetch a character, resolve everything, summarise.
- `compare-to-meta` — character vs the catalog meta for a gamemode.
- `decode-and-explain` — chat code → plain-English breakdown.
- `recommend-build` — profession + gamemode → top catalog pick.
- `daily-routine` — fuse today's dailies + reset progress with what you
  remember about the player's habits and time budget; produce a
  prioritised checklist for the session.
- `next-zone` — recommend a specific zone tuned to character level,
  mastery readiness, and the player's stated progression goals.
- `next-collection` — surface achievement collections the player is
  closest to finishing, weighted by their interest profile (legendaries,
  skins, titles, masteries) from prior conversations.
- `mount-progression` — recommend the next mount unlock based on
  expansion access + mastery progress + the mounts the player has
  mentioned having.
- `legendary-progress` — produce a "what's left" breakdown for a
  specific legendary, factoring in wallet, achievements, raid/WvW/PvP
  rank.
- `weekly-roundup` — done-vs-left summary for raids / fractals / strikes
  / WvW participation this reset week.

The five PvE-coaching prompts (everything from `daily-routine` onward)
explicitly tell you to **recall prior context** about the player. If you
have none, ask first.

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
