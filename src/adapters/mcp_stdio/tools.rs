//! Static tool registry — every MCP tool's JSON-schema literal lives
//! here. Splitting these out keeps `mod.rs` focused on dispatch; tool
//! authors edit one file and the schema declarations sit next to each
//! other (handy for spotting copy-paste errors across similar tools).
//!
//! Visibility: only `build_tools` needs to escape; the regex patterns
//! and annotation helpers are private to this file.

use rmcp::model::{Tool, ToolAnnotations};

/// API-key regex: GW2 keys are 5 hyphen-separated hex blocks of widths
/// 8-4-4-4-20 repeated twice — total 72 chars. Mirrors `ApiKey::new`'s
/// validation. Used in tool input schemas so clients can reject obvious
/// typos before a round-trip.
const API_KEY_PATTERN: &str = "^[A-F0-9]{8}-[A-F0-9]{4}-[A-F0-9]{4}-[A-F0-9]{4}-[A-F0-9]{20}-[A-F0-9]{8}-[A-F0-9]{4}-[A-F0-9]{4}-[A-F0-9]{4}-[A-F0-9]{20}$";

/// Chat-code regex: `[&` + base64 (URL-safe and standard alphabets both
/// accepted in the wild) + optional `=` padding + `]`. Loose intentionally —
/// strict validation lives in `BuildChatCode::new`.
const CHAT_CODE_PATTERN: &str = "^\\[&[A-Za-z0-9+/]+=*\\]$";

#[allow(clippy::too_many_lines)] // Each tool needs its own schema literal; refactoring into a
// table-driven form would obscure them more than help.
pub(super) fn build_tools() -> Vec<Tool> {
    let wiki_search: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query (e.g. 'Dragon Bash', 'wallet', 'Mystic Forge')."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 50,
                "default": 5,
                "examples": [5, 10],
                "description": "Maximum results to return."
            }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");
    let get_wallet: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "api_key": {
                "type": ["string", "null"],
                "default": null,
                "format": "password",
                "writeOnly": true,
                "pattern": API_KEY_PATTERN,
                "description": "GW2 API key with 'account' and 'wallet' scopes. \
                                Generate at https://account.arena.net/applications. \
                                Optional — falls back to the server-configured key \
                                (GW2_API_KEY env var) if omitted. Format: \
                                72-char hex with hyphens (8-4-4-4-20-8-4-4-4-20)."
            }
        }
    }))
    .expect("valid schema literal");
    let get_currencies: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "ids": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "maxItems": 200,
                "uniqueItems": true,
                "description": "Specific currency ids. Omit (or pass empty) to fetch all. \
                                Max 200 ids per call (matches the GW2 API's per-request cap)."
            }
        }
    }))
    .expect("valid schema literal");

    let by_required_ids_with_summary: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "ids": {
                    "type": "array",
                    "items": { "type": "integer", "minimum": 1 },
                    "minItems": 1,
                    "maxItems": 200,
                    "uniqueItems": true,
                    "description": "Ids to fetch. Required — there are 1000s of entries; pass only what you need. Max 200 per call (GW2 API cap)."
                },
                "summary": {
                    "type": "boolean",
                    "default": true,
                    "description": "When true (default), return a compact projection (id, name, description, slot/type) and drop facts[] + icon URLs. Set false for the full GW2 API shape."
                }
            },
            "required": ["ids"]
        }))
        .expect("valid schema literal");

    let get_character_build: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "api_key": {
                    "type": ["string", "null"],
                    "default": null,
                    "format": "password",
                    "writeOnly": true,
                    "pattern": API_KEY_PATTERN,
                    "description": "GW2 API key with 'account' + 'characters' + 'builds' scopes. Optional — falls back to the server-configured key (GW2_API_KEY env var) if omitted."
                },
                "character": { "type": "string", "description": "Character name (case-sensitive)." },
                "tab": {
                    "type": ["string", "integer"],
                    "default": "active",
                    "description": "Which tab(s) to return: \"active\" (default; the in-game-equipped one), \"all\", or a numeric tab index like 1, 2, 3."
                }
            },
            "required": ["character"]
        }))
        .expect("valid schema literal");

    let decode_build_code: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "code": {
                "type": "string",
                "pattern": CHAT_CODE_PATTERN,
                "examples": ["[&DQEQPyo6GzkmDyYPihJIAUgBLQH+ALkBtRI3AQAAAAAAAAAAAAAAAAAAAAACMgAjAAA=]"],
                "description": "Build chat code, e.g. `[&DQ...=]`. Decoded into structured JSON (profession, specs, palette skill ids with resolved API ids, traits with resolved trait ids, pets/legends, profession_name)."
            }
        },
        "required": ["code"]
    }))
    .expect("valid schema literal");

    let list_catalog_builds: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Browse a curated build source. Use list_catalog_sources to discover sources first. Pagination is cursor-based: pass `next_cursor` from the previous response to continue.",
        "properties": {
            "source": {
                "type": "string",
                "examples": ["discretize", "metabattle", "snowcrows"],
                "description": "Source name from list_catalog_sources (e.g. discretize, metabattle, snowcrows)."
            },
            "profession": {
                "type": "string",
                "enum": [
                    "guardian", "warrior", "engineer", "ranger", "thief",
                    "elementalist", "mesmer", "necromancer", "revenant"
                ],
                "description": "Optional profession filter (lower-case). Catalogs match case-insensitively."
            },
            "gamemode": {
                "type": "string",
                "enum": ["fractals", "raids", "strikes", "open_world", "wvw", "pvp"],
                "description": "Optional game-mode filter. Source coverage: `discretize` → fractals (authoritative); `snowcrows` → raids (authoritative) plus `open_world`/`pvp`/`wvw` (curated meta picks); `metabattle` → broadest community-wiki coverage across all gamemodes. Note: `list_catalog_builds(source=\"snowcrows\")` with no `gamemode` returns only raids — pass `gamemode` explicitly for the other Snow Crows categories. Both `open_world` and `open-world` spellings are accepted."
            },
            "page_size": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 25,
                "examples": [25, 50],
                "description": "Items per page. Default 25, max 100 (silently clamped). Page size must stay constant across the pagination — the cursor binds to (source, profession, gamemode), not page_size."
            },
            "cursor": {
                "type": "string",
                "description": "Opaque pagination cursor from a previous response's `next_cursor` field. Omit to start at the beginning. Cursors are bound to the (source, profession, gamemode) triple — changing any of them mid-paginate raises an error."
            }
        },
        "required": ["source"]
    }))
    .expect("valid schema literal");

    let get_catalog_build: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "source": {
                "type": "string",
                "examples": ["discretize", "metabattle", "snowcrows"],
                "description": "Source name (matches list_catalog_sources)."
            },
            "slug": {
                "type": "string",
                // Mirrors `BuildSlug` validation: lower-case alphanumeric +
                // `_-/` only, no path-traversal segments. Stops the LLM
                // from sending `../../etc/passwd` or newline-laced slugs
                // that would explode `url::Url::parse`.
                "pattern": "^[a-z0-9_\\-/]+$",
                "maxLength": 256,
                "examples": [
                    "guardian/power-dragonhunter",
                    "raids/guardian/heal-firebrand",
                    "guardian/power_dragonhunter"
                ],
                "description": "Build slug from list_catalog_builds. Format varies per source: discretize uses `<profession>/<build>`, metabattle uses `<profession>/<build>` (slugified), snowcrows uses `<category>/<profession>/<build>`. Always pass the literal slug returned by list_catalog_builds. Must match `^[a-z0-9_\\-/]+$`; ≤ 256 chars; no `..` segments."
            }
        },
        "required": ["source", "slug"]
    }))
    .expect("valid schema literal");

    let empty_args: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    }))
    .expect("valid schema literal");

    // Tier 6A — schemas reused across the new account/coaching tools.
    //
    // `authed_no_args` covers the half-dozen endpoints that take *only* an
    // optional API key (account, character list, masteries, raids, dungeons).
    // Keeping them on a shared schema avoids drift across them.
    let authed_no_args: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "api_key": {
                "type": ["string", "null"],
                "default": null,
                "format": "password",
                "writeOnly": true,
                "pattern": API_KEY_PATTERN,
                "description": "GW2 API key with the scopes the called tool documents (typically `account` + `progression`). Optional — falls back to the server-configured key (GW2_API_KEY) if omitted."
            }
        }
    }))
    .expect("valid schema literal");

    let get_account_achievements: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "api_key": {
                    "type": ["string", "null"],
                    "default": null,
                    "format": "password",
                    "writeOnly": true,
                    "pattern": API_KEY_PATTERN,
                    "description": "GW2 API key with `account` + `progression` scopes. Optional — falls back to the server-configured key."
                },
                "summary": {
                    "type": "boolean",
                    "default": true,
                    "description": "When true (default), drop entries that are fully complete (`done==true` or `current==max`) and entries with no progress yet (`current==0` or absent). What's left is roughly the player's in-flight work — typically 100–300 entries instead of 2000–3000. Pass false to get the raw account-achievement list."
                }
            }
        }))
        .expect("valid schema literal");

    let get_dailies: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["api_key"],
        "properties": {
            "api_key": {
                "type": "string",
                "description": "Guild Wars 2 API key with `account` + `progression` scopes. Wizard's Vault is a per-account endpoint, so the public-no-key behaviour of the old `/v2/achievements/daily` no longer applies."
            },
            "which": {
                "type": "string",
                "enum": ["daily", "weekly", "special"],
                "default": "daily",
                "description": "Which Wizard's Vault track to fetch. `daily` (default) hits `/v2/account/wizardsvault/daily`; `weekly` hits the weekly track; `special` hits the current limited-time / seasonal track."
            }
        }
    }))
    .expect("valid schema literal");

    // -- Tier 6B navigation tool schemas --------------------------------
    //
    // `LocationRef` is encoded as a JSON Schema oneOf with three shapes:
    //   {coords:[x,y], map_id?}   — literal coords (map_id optional)
    //   {poi_name, map_id}         — named POI on a known map
    //   {here:true}                — current Mumble Link position
    let location_ref_schema = serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["coords"],
                "properties": {
                    "coords": {
                        "type": "array",
                        "items": { "type": "number" },
                        "minItems": 2,
                        "maxItems": 2,
                        "description": "Continent-space [x, y] map coordinates."
                    },
                    "map_id": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Map id; falls back to the player's current map if omitted."
                    }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["poi_name", "map_id"],
                "properties": {
                    "poi_name": { "type": "string", "description": "Case-insensitive POI name (waypoint, landmark, vista, etc.)." },
                    "map_id": { "type": "integer", "minimum": 1 }
                }
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["here"],
                "properties": {
                    "here": { "type": "boolean", "const": true, "description": "Use the player's current Mumble Link position." }
                }
            }
        ]
    });

    let get_directions_schema: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "from": location_ref_schema,
                "to": location_ref_schema
            },
            "required": ["from", "to"]
        }))
        .expect("valid schema literal");

    let get_my_location_schema: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "include_neighbors": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, also include the curated map-adjacency list (same data `get_map_neighbors` returns) under `neighbors`. Useful for combined 'where am I and what's nearby?' planning."
                }
            }
        }))
        .expect("valid schema literal");

    let find_nearby_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "filter": {
                "type": "string",
                "enum": ["waypoint", "poi", "vista", "hero_point", "task", "any"],
                "default": "any",
                "description": "POI category to include. `poi` matches landmarks + unlocks; `waypoint` matches travel waypoints only."
            },
            "around": location_ref_schema.clone(),
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 25,
                "default": 5,
                "description": "Maximum results to return."
            }
        }
    }))
    .expect("valid schema literal");

    let get_map_neighbors_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["map_id"],
        "properties": {
            "map_id": {
                "type": "integer",
                "minimum": 1,
                "description": "GW2 map id (pair with `get_my_location` to get the current map's id)."
            }
        }
    }))
    .expect("valid schema literal");

    let refresh_account_cache_schema: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "api_key": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Optional Guild Wars 2 API key whose cached data to invalidate. Falls back to the server's default key."
                }
            }
        }))
        .expect("valid schema literal");

    let plan_route_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["from", "to"],
        "properties": {
            "from": {
                "oneOf": [
                    { "type": "object", "additionalProperties": false, "required": ["id"],   "properties": { "id":   { "type": "integer", "minimum": 1 } } },
                    { "type": "object", "additionalProperties": false, "required": ["name"], "properties": { "name": { "type": "string", "minLength": 1, "description": "Map name (case-insensitive; substring matches too)." } } },
                    { "type": "object", "additionalProperties": false, "required": ["here"], "properties": { "here": { "type": "boolean", "enum": [true], "description": "Resolve via Mumble Link — uses the map the player is currently on." } } }
                ]
            },
            "to": {
                "oneOf": [
                    { "type": "object", "additionalProperties": false, "required": ["id"],   "properties": { "id":   { "type": "integer", "minimum": 1 } } },
                    { "type": "object", "additionalProperties": false, "required": ["name"], "properties": { "name": { "type": "string", "minLength": 1 } } }
                ]
            },
            "k": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10,
                "default": 3,
                "description": "How many distinct shortest-hop routes to return. Capped at 10."
            },
            "prefer": {
                "type": "string",
                "enum": ["shortest", "walking", "gates"],
                "default": "shortest",
                "description": "Re-rank the K paths after K-shortest selects them. 'walking' favours routes with more `physical` edges; 'gates' favours routes with more `asura_gate` edges. Default 'shortest' is the natural Yen's order (ascending hop count). Bump `k` to get more variety to re-rank from."
            },
            "exclude_connections": {
                "type": "array",
                "items": { "type": "string", "enum": ["physical", "asura_gate", "story_gate", "instance_portal", "guild_hall"] },
                "default": [],
                "description": "Skip edges whose `connection` matches any value in this list. Common use: `[\"story_gate\"]` to avoid routes the player may not be able to traverse yet."
            },
            "player_access": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Snake-case expansion names the player owns (e.g. [\"core\", \"heart_of_thorns\", \"path_of_fire\"]). Edges to maps whose `expansion` isn't in this list (with `core`/`festival`/`living_world_season1`/`living_world_season2` always permitted) are skipped. ABSENT (the default): auto-fetch from /v2/account using the supplied or configured API key; if that fails, no filtering. EMPTY ARRAY: explicitly disable access filtering (e.g. for hypothetical 'what would I need to buy?' planning)."
            },
            "api_key": {
                "type": "string",
                "minLength": 1,
                "description": "Optional Guild Wars 2 API key. Used only to auto-populate `player_access` when that arg is absent. Falls back to the server's default key if neither is supplied."
            }
        }
    }))
    .expect("valid schema literal");

    let list_maps_in_region_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["region"],
        "properties": {
            "region": {
                "oneOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id"],
                        "properties": {
                            "id": { "type": "integer", "minimum": 1, "description": "GW2 region id." }
                        }
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name"],
                        "properties": {
                            "name": { "type": "string", "minLength": 1, "description": "Region name (case-insensitive substring match: \"maguuma\" matches \"Maguuma Jungle\")." }
                        }
                    }
                ]
            }
        }
    }))
    .expect("valid schema literal");

    // ----- Tier 6C search-tool schemas -----
    // Common base used by all search_* tools — query + limit. Per-entity
    // schemas extend this with kind-specific filter properties.
    let search_skills_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Fuzzy name search over the local skills index. Returns lightweight refs (id, name, snippet); use get_skills with the returned ids for the full payload.",
        "properties": {
            "query": {
                "type": "string",
                "minLength": 2,
                "description": "Free-form search text. Tokenised with diacritic-folding; the last token is prefix-matched (\"wra\" finds \"Wrack\")."
            },
            "limit": {
                "type": "integer", "minimum": 1, "maximum": 50, "default": 10,
                "description": "Maximum results to return."
            },
            "profession": {
                "type": "string",
                "description": "Restrict to a specific profession (e.g. \"Mesmer\", \"Guardian\"). Case-sensitive — pass the canonical capitalised form GW2 uses."
            },
            "slot": {
                "type": "string",
                "description": "Skill slot, e.g. \"Heal\", \"Elite\", \"Weapon_1\", \"Profession_3\"."
            },
            "weapon_type": {
                "type": "string",
                "description": "Weapon name when filtering weapon skills (e.g. \"Greatsword\", \"Sword\")."
            }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");

    let search_traits_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Fuzzy name search over the local traits index. Returns lightweight refs; use get_traits for full details.",
        "properties": {
            "query": { "type": "string", "minLength": 2 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
            "specialization": { "type": "integer", "minimum": 1, "description": "Specialization id to scope results to." },
            "tier": { "type": "integer", "minimum": 1, "maximum": 3, "description": "1=Adept, 2=Master, 3=Grandmaster." }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");

    let search_specs_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Fuzzy name search over the local specializations index.",
        "properties": {
            "query": { "type": "string", "minLength": 2 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
            "profession": { "type": "string" },
            "elite": { "type": "boolean", "description": "true → only elite specs; false → only core; omit → both." }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");

    let search_items_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Fuzzy name search over the local items index. Available only when the server was started with --with-items (off by default; opt-in due to ~50MB disk usage).",
        "properties": {
            "query": { "type": "string", "minLength": 2 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
            "type": { "type": "string", "description": "Item top-level type (e.g. \"Weapon\", \"Armor\", \"Trinket\", \"Consumable\")." },
            "rarity": { "type": "string", "description": "\"Junk\" | \"Basic\" | \"Fine\" | \"Masterwork\" | \"Rare\" | \"Exotic\" | \"Ascended\" | \"Legendary\"." },
            "min_level": { "type": "integer", "minimum": 0, "maximum": 80 },
            "max_level": { "type": "integer", "minimum": 0, "maximum": 80 },
            "weight_class": { "type": "string", "description": "Armor weight class: \"Light\" | \"Medium\" | \"Heavy\"." }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");

    let search_achievements_schema: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Fuzzy name + requirement search over the local achievements index.",
        "properties": {
            "query": { "type": "string", "minLength": 2 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
            "type": { "type": "string", "description": "Achievement type, e.g. \"Daily\", \"Bonus\", \"WorldBoss\"." }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");

    vec![
        Tool::new(
            "wiki_search",
            "Search the Guild Wars 2 wiki and return enriched results (with prose extracts).",
            wiki_search,
        )
        .annotate(read_only_open_world("Search GW2 Wiki"))
        .with_output_schema::<crate::domain::SearchResponse>(),
        Tool::new(
            "get_wallet",
            "Fetch the user's wallet, including currency metadata. Requires an API key.",
            get_wallet,
        )
        .annotate(read_only_open_world("Get Wallet"))
        .with_output_schema::<crate::domain::WalletInfo>(),
        // get_currencies, get_skills, get_traits, get_specializations, and
        // get_items all return `BTreeMap<TypedId, T>`. The wire shape is a JSON
        // object keyed by stringified numeric ids — useful for the LLM but
        // schemars 1.x emits a non-`object`-rooted schema for maps with
        // non-string key types, which `Tool::with_output_schema` rejects per
        // the MCP spec. Keeping the wire shape (and Tier 1's tests) wins;
        // outputSchema is left off these five intentionally. See report.
        Tool::new(
            "get_currencies",
            "Fetch Guild Wars 2 currency metadata. Pass `ids` for specific currencies; omit to \
             fetch all.",
            get_currencies,
        )
        .annotate(read_only_open_world("Get Currencies")),
        Tool::new(
            "get_skills",
            "Resolve GW2 skill ids (e.g. those returned by get_character_build) into name + description. Returns a compact summary by default; pass `summary=false` for the full payload (facts[], icon URLs, etc.).",
            by_required_ids_with_summary.clone(),
        )
        .annotate(read_only_closed_world("Get Skills")),
        Tool::new(
            "get_traits",
            "Resolve GW2 trait ids into name + description. Returns a compact summary by default; pass `summary=false` for the full payload.",
            by_required_ids_with_summary.clone(),
        )
        .annotate(read_only_closed_world("Get Traits")),
        Tool::new(
            "get_specializations",
            "Resolve GW2 specialization ids (core + elite) into name, profession, and minor/major trait ids. Pass `summary=false` for the full payload.",
            by_required_ids_with_summary.clone(),
        )
        .annotate(read_only_closed_world("Get Specializations")),
        Tool::new(
            "get_items",
            "Resolve item ids → id, name, rarity, level, type, weight_class, chat_link. Pass `summary=false` for the full payload (icon URL, details, recipe data).",
            by_required_ids_with_summary,
        )
        .annotate(read_only_closed_world("Get Items")),
        Tool::new(
            "get_character_build",
            "Fetch a character's build + equipment, with skill/trait/specialization names pre-resolved. Defaults to the active tab; pass `tab=\"all\"` or a specific index for others. Requires an API key with 'builds' scope.",
            get_character_build,
        )
        .annotate(read_only_open_world("Get Character Build"))
        .with_output_schema::<crate::service::CharacterBuildSnapshot>(),
        Tool::new(
            "decode_build_code",
            "Decode a `[&Dw...]` build chat code into structured JSON. No auth required.",
            decode_build_code,
        )
        .annotate(read_only_closed_world("Decode Build Chat Code")),
        Tool::new(
            "list_catalog_sources",
            "List the registered curated-build catalog sources (Discretize, MetaBattle, Snow Crows, …).",
            empty_args.clone(),
        )
        .annotate(read_only_closed_world("List Catalog Sources")),
        // list_catalog_builds returns `{items: [...], next_cursor: ...}`.
        // outputSchema would be a thin wrapper around `Vec<BuildSummary>`;
        // schemars-emitting it for the wrapper buys us little vs the cost of
        // duplicating the cursor shape, so it's left off intentionally.
        Tool::new(
            "list_catalog_builds",
            "List builds from a curated catalog source. Returns lightweight summaries plus an opaque pagination cursor — pass `next_cursor` back as `cursor` to continue. Use get_catalog_build for the full per-build details.",
            list_catalog_builds,
        )
        .annotate(read_only_open_world("List Catalog Builds")),
        Tool::new(
            "get_catalog_build",
            "Fetch full details for a specific curated build by source + slug.",
            get_catalog_build,
        )
        .annotate(read_only_open_world("Get Catalog Build"))
        .with_output_schema::<crate::ports::BuildDetail>(),
        Tool::new(
            "get_info",
            "Return the server's usage runbook — how to choose tools, recipe per question type, gotchas, and the prompt + resource catalogue. Identical to the `instructions` field returned in MCP `initialize`; offered as a tool because some clients drop or truncate that field.",
            empty_args.clone(),
        )
        .annotate(read_only_closed_world("About this server")),
        // Tier 6A — PvE coaching surface.
        Tool::new(
            "get_account",
            "Fetch /v2/account snapshot: world, age, `access` list (expansions owned), guilds, fractal level, daily/monthly AP, WvW rank. Requires `account` scope.",
            authed_no_args.clone(),
        )
        .annotate(read_only_open_world("Get Account"))
        .with_output_schema::<crate::domain::Account>(),
        Tool::new(
            "list_characters",
            "List character names on the account. Cheap — names only, not builds. Pair with `get_character_build`. Requires `characters` scope.",
            authed_no_args.clone(),
        )
        .annotate(read_only_open_world("List Characters"))
        .with_output_schema::<crate::service::CharacterList>(),
        Tool::new(
            "get_account_achievements",
            "Per-account achievement progress, enriched with achievement name + description. `summary=true` (default) keeps only in-flight work (heavy: 2-3k entries otherwise). Requires `account` + `progression` scopes.",
            get_account_achievements,
        )
        .annotate(read_only_open_world("Get Account Achievements"))
        .with_output_schema::<crate::service::AccountAchievementsSnapshot>(),
        Tool::new(
            "get_account_masteries",
            "Mastery track progress, enriched with track name, region, current level name. Requires `account` + `progression` scopes.",
            authed_no_args.clone(),
        )
        .annotate(read_only_open_world("Get Account Masteries"))
        .with_output_schema::<crate::service::AccountMasteriesSnapshot>(),
        Tool::new(
            "get_account_raids",
            "Raid clears + full encounter list (each with `cleared: bool`), cleared/total counts, next weekly reset (Monday 07:30 UTC). Requires `account` + `progression` scopes.",
            authed_no_args.clone(),
        )
        .annotate(read_only_open_world("Get Account Raids")),
        Tool::new(
            "get_account_dungeons",
            "Dungeon path clears for today (same shape as get_account_raids, daily cadence). Next reset at 00:00 UTC. Requires `account` + `progression` scopes.",
            authed_no_args,
        )
        .annotate(read_only_open_world("Get Account Dungeons")),
        Tool::new(
            "get_dailies",
            "Wizard's Vault track (`daily` (default) | `weekly` | `special`). Each objective: title, track, acclaim, progress, claimed. Includes meta-chest progress + precomputed acclaim_earned/remaining/total. Requires `account` + `progression` scopes.",
            get_dailies,
        )
        .annotate(read_only_open_world("Get Dailies"))
        .with_output_schema::<crate::service::WizardsVaultSnapshot>(),
        // -- Tier 6B: navigation tools (Mumble Link + map data) -----------
        // All four are openWorld=true: Mumble Link state changes every
        // frame, and POI lookups touch the live GW2 API. They're still
        // read-only / non-destructive / idempotent — repeating the call
        // never changes server-side state.
        Tool::new(
            "get_my_location",
            "Live player state via Mumble Link: character, profession, map, coords, 16-point facing, mount (`mount.index == 0` = dismounted). Pass `include_neighbors:true` to inline get_map_neighbors. Requires GW2 running on the same host.",
            get_my_location_schema,
        )
        .annotate(read_only_open_world("Get My Location"))
        .with_output_schema::<crate::service::MyLocationSnapshot>(),
        Tool::new(
            "get_directions",
            "Return the bearing (16-point compass) and distance from `from` to `to`. Each endpoint can be literal coords `{coords:[x,y], map_id?}`, a named POI `{poi_name, map_id}`, or the player's current location `{here:true}`. POI names match case-insensitively against the map's waypoint/landmark/vista catalogue.",
            get_directions_schema,
        )
        .annotate(read_only_open_world("Get Directions"))
        .with_output_schema::<crate::service::DirectionsResult>(),
        Tool::new(
            "find_nearby",
            "List closest POIs to `around` (default: current location). `filter`: waypoint | poi | vista | hero_point | task | any. Up to 25 results sorted nearest-first.",
            find_nearby_schema,
        )
        .annotate(read_only_open_world("Find Nearby"))
        .with_output_schema::<crate::service::NearbySearchResult>(),
        Tool::new(
            "list_maps_in_region",
            "List maps in a region. `region: {name: \"...\"}` (case-insensitive substring) or `{id: 4}`. Sorted by min_level for progression order.",
            list_maps_in_region_schema,
        )
        .annotate(read_only_open_world("List Maps In Region"))
        .with_output_schema::<crate::service::RegionMapList>(),
        Tool::new(
            "get_map_neighbors",
            "Maps bordering `map_id`. Each neighbor has direction (16-pt compass, comma-separated for arcs), connection (physical | asura_gate | story_gate | instance_portal | guild_hall), gate_location, note. Covers open-world + 11 hub cities; excludes instances/fractals/dungeons/raids/WvW.",
            get_map_neighbors_schema,
        )
        .annotate(read_only_open_world("Get Map Neighbors"))
        .with_output_schema::<crate::service::MapNeighborsResponse>(),
        Tool::new(
            "refresh_account_cache",
            "Invalidate cached account data (account/wallet/characters/achievements/masteries/raids/dungeons/wizards-vault) so the next call refetches live. Use after buying an expansion or completing progress. Static catalogues (items/skills/traits) not touched.",
            refresh_account_cache_schema,
        )
        .annotate(read_only_open_world("Refresh Account Cache"))
        .with_output_schema::<crate::service::RefreshAccountCacheResult>(),
        Tool::new(
            "plan_route",
            "Top-K shortest-hop routes between two maps. `from`/`to`: `{id}` | `{name}` | `{here:true}`. Each hop carries connection, direction, gate_location, note. `prefer` re-ranks (shortest|walking|gates). `exclude_connections` blocks edge types. `player_access` filters by expansions owned — absent = auto-fetch from /v2/account; empty array disables filtering. Same coverage as get_map_neighbors.",
            plan_route_schema,
        )
        .annotate(read_only_open_world("Plan Route"))
        .with_output_schema::<crate::service::RoutePlan>(),
        Tool::new(
            "describe_facing",
            "Describe which way the player is facing in plain English plus the closest landmark in that direction. No arguments — reads live state from Mumble Link.",
            empty_args.clone(),
        )
        .annotate(read_only_open_world("Describe Facing"))
        .with_output_schema::<crate::service::FacingDescription>(),
        // -- Tier 6C: local fuzzy search over the cached corpus.
        Tool::new(
            "search_skills",
            "Fuzzy-search the local skills index by name. Returns lightweight refs (id, name, description snippet, slot, professions). Use the returned ids with get_skills for the full payload. Backed by an on-disk SQLite/FTS5 index that populates in the background on startup.",
            search_skills_schema,
        )
        .annotate(read_only_closed_world("Search Skills")),
        Tool::new(
            "search_traits",
            "Fuzzy-search the local traits index by name. Returns lightweight refs; use get_traits with the ids for full details. Filter by specialization id or tier (1=Adept, 2=Master, 3=Grandmaster).",
            search_traits_schema,
        )
        .annotate(read_only_closed_world("Search Traits")),
        Tool::new(
            "search_specializations",
            "Fuzzy-search the local specializations (core + elite) index by name. Filter by profession or `elite=true/false`.",
            search_specs_schema,
        )
        .annotate(read_only_closed_world("Search Specializations")),
        Tool::new(
            "search_items",
            "Fuzzy-search the local items index by name. Only available when the server was started with --with-items (opt-in due to ~5min initial population + ~50MB disk). If items aren't indexed, the response will say so.",
            search_items_schema,
        )
        .annotate(read_only_closed_world("Search Items")),
        Tool::new(
            "search_achievements",
            "Fuzzy-search the local achievements index by name + requirement text. Filter by achievement type (e.g. \"Daily\", \"WorldBoss\").",
            search_achievements_schema,
        )
        .annotate(read_only_closed_world("Search Achievements")),
        Tool::new(
            "get_index_status",
            "Inspect the search index: per-kind row counts, last-refreshed timestamps, current GW2 build number stamped into the index, and a `state` flag (`\"ready\"` once `indexed == total`; `\"indexing\"` while still populating). Use to distinguish \"empty corpus\" from \"still indexing\" when a `search_*` tool returns no hits. Also returns an `overall` rollup field.",
            empty_args,
        )
        .annotate(read_only_closed_world("Search Index Status"))
        .with_output_schema::<crate::ports::IndexStatusView>(),
    ]
}

/// Builder for a `ToolAnnotations` describing a read-only, idempotent,
/// non-destructive tool whose data crosses the network boundary (open world).
fn read_only_open_world(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .idempotent(true)
        .destructive(false)
        .open_world(true)
}

/// Builder for a `ToolAnnotations` describing a read-only, idempotent,
/// non-destructive tool whose dataset is finite and offline (closed world).
fn read_only_closed_world(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .idempotent(true)
        .destructive(false)
        .open_world(false)
}
