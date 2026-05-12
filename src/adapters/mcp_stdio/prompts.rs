//! Prompt registry + renderers for the MCP server.
//!
//! Prompts are client-visible slash-command shortcuts. Each renderer is
//! a pure function: it takes the user-supplied args and produces a
//! `GetPromptResult` whose body is a templated user message telling the
//! LLM which tools to call.
//!
//! Visibility note: only `build_prompts`, `render_prompt`, and
//! `PromptError` need to be visible to the parent module. Everything
//! else (the `PROMPT_*` name constants, the per-prompt `render_*`
//! helpers, the small `require_arg`/`optional_arg`/`finish_prompt`
//! utilities) stays private to this file.

use rmcp::model::{GetPromptResult, Prompt, PromptArgument, PromptMessage, PromptMessageRole};

// 10 prompts × ~12 lines each — table-driven would be denser at the cost
// of readability per prompt; each prompt benefits from sitting next to
// its argument list.
#[allow(clippy::too_many_lines)]
pub(super) fn build_prompts() -> Vec<Prompt> {
    fn arg(name: &str, description: &str, required: bool) -> PromptArgument {
        PromptArgument::new(name)
            .with_description(description)
            .with_required(required)
    }

    vec![
        Prompt::new(
            PROMPT_ANALYZE_CHARACTER,
            Some(
                "Fetch a character's build and equipment, resolve all IDs to names, and produce \
                 a build summary.",
            ),
            Some(vec![
                arg("character", "GW2 character name (case-sensitive).", true),
                arg(
                    "api_key",
                    "GW2 API key with `account`, `characters`, and `builds` scopes. Generate at \
                     https://account.arena.net/applications.",
                    true,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_COMPARE_TO_META,
            Some(
                "Compare a character's current build to curated meta builds for their profession.",
            ),
            Some(vec![
                arg("character", "GW2 character name.", true),
                arg(
                    "api_key",
                    "GW2 API key with `account`, `characters`, and `builds` scopes.",
                    true,
                ),
                arg(
                    "gamemode",
                    "Optional. One of: `fractals`, `raids`, `open_world`, `pvp`, `wvw`. If \
                     omitted the prompt asks the user.",
                    false,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_DECODE_AND_EXPLAIN,
            Some("Decode a Guild Wars 2 build chat code and explain what the build does."),
            Some(vec![arg(
                "code",
                "GW2 build chat code, including the surrounding `[& ... ]` brackets.",
                true,
            )]),
        ),
        Prompt::new(
            PROMPT_RECOMMEND_BUILD,
            Some("Recommend a curated meta build for a profession + gamemode and explain it."),
            Some(vec![
                arg(
                    "profession",
                    "One of the nine professions: `guardian`, `warrior`, `engineer`, `ranger`, \
                     `thief`, `elementalist`, `mesmer`, `necromancer`, `revenant`.",
                    true,
                ),
                arg(
                    "gamemode",
                    "One of: `fractals`, `raids`, `open_world`, `pvp`, `wvw`.",
                    true,
                ),
                arg(
                    "experience",
                    "Optional. One of: `beginner`, `intermediate`, `expert`. Tunes the \
                     explanation depth.",
                    false,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_DAILY_ROUTINE,
            Some(
                "Plan today's GW2 routine: daily achievements, dungeon paths, raid clears, \
                 fused with what the AI remembers about the player.",
            ),
            Some(vec![
                arg(
                    "api_key",
                    "GW2 API key with `account` + `progression` scopes.",
                    true,
                ),
                arg(
                    "character",
                    "Optional character name to also pull the active build for.",
                    false,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_NEXT_ZONE,
            Some(
                "Recommend the next zone for a character to explore based on their build, \
                 expansion access, and mastery progress.",
            ),
            Some(vec![
                arg(
                    "api_key",
                    "GW2 API key with `account` + `progression` scopes.",
                    true,
                ),
                arg("character", "Character name (case-sensitive).", true),
            ]),
        ),
        Prompt::new(
            PROMPT_NEXT_COLLECTION,
            Some("Find the most rewarding achievement collection to finish next."),
            Some(vec![arg(
                "api_key",
                "GW2 API key with `account` + `progression` scopes.",
                true,
            )]),
        ),
        Prompt::new(
            PROMPT_MOUNT_PROGRESSION,
            Some(
                "Plan the next mount unlock based on expansion access and Mount Mastery progress.",
            ),
            Some(vec![arg(
                "api_key",
                "GW2 API key with `account` + `progression` scopes.",
                true,
            )]),
        ),
        Prompt::new(
            PROMPT_LEGENDARY_PROGRESS,
            Some(
                "Track legendary crafting progress: gold, currencies, raids/WvW gates, recipe steps.",
            ),
            Some(vec![
                arg(
                    "api_key",
                    "GW2 API key with `account` + `wallet` + `progression` scopes.",
                    true,
                ),
                arg(
                    "legendary",
                    "Optional. The legendary the player is working toward (e.g. `Twilight`, `The Predator`).",
                    false,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_WEEKLY_ROUNDUP,
            Some("Summarise what's left this reset week: raids, fractals, WvW, dungeons."),
            Some(vec![arg(
                "api_key",
                "GW2 API key with `account` + `progression` scopes.",
                true,
            )]),
        ),
    ]
}

const PROMPT_ANALYZE_CHARACTER: &str = "analyze-character";
const PROMPT_COMPARE_TO_META: &str = "compare-to-meta";
const PROMPT_DECODE_AND_EXPLAIN: &str = "decode-and-explain";
const PROMPT_RECOMMEND_BUILD: &str = "recommend-build";

// Tier 6A — PvE coaching prompts. Each fuses a live API call (or three)
// with what the *AI* remembers about the player from prior conversations
// — the server itself stores nothing user-specific. The exact wording
// matters: phrases like "recall what you know" steer the model toward
// using its own conversation memory rather than implying server-side
// state.
const PROMPT_DAILY_ROUTINE: &str = "daily-routine";
const PROMPT_NEXT_ZONE: &str = "next-zone";
const PROMPT_NEXT_COLLECTION: &str = "next-collection";
const PROMPT_MOUNT_PROGRESSION: &str = "mount-progression";
const PROMPT_LEGENDARY_PROGRESS: &str = "legendary-progress";
const PROMPT_WEEKLY_ROUNDUP: &str = "weekly-roundup";

#[derive(Debug)]
pub(super) enum PromptError {
    NotFound(String),
    MissingArg {
        prompt: &'static str,
        arg: &'static str,
    },
}

/// Render a prompt name + args into a `GetPromptResult`. Pure function: no
/// I/O, no state — each prompt boils down to a templated user message that
/// instructs the LLM which tools to call. Exposed at module scope so the
/// integration tests can pin the rendered body without spinning up a full
/// `McpServer`.
fn require_arg(
    args: &serde_json::Map<String, serde_json::Value>,
    prompt: &'static str,
    arg: &'static str,
) -> Result<String, PromptError> {
    args.get(arg)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(PromptError::MissingArg { prompt, arg })
}

fn optional_arg(args: &serde_json::Map<String, serde_json::Value>, arg: &str) -> Option<String> {
    args.get(arg)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn finish_prompt(description: &str, body: String) -> GetPromptResult {
    GetPromptResult::new(vec![PromptMessage::new_text(PromptMessageRole::User, body)])
        .with_description(description)
}

fn render_analyze_character(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let character = require_arg(args, PROMPT_ANALYZE_CHARACTER, "character")?;
    let api_key = require_arg(args, PROMPT_ANALYZE_CHARACTER, "api_key")?;
    let body = format!(
        "You are analysing a Guild Wars 2 character build.\n\n\
         Steps to follow:\n\n\
         1. Use the `get_character_build` tool with `character=\"{character}\"` and \
         `api_key=\"{api_key}\"`. The response now pre-resolves skill, trait, and \
         specialization names — you do not need to call `get_skills` / `get_traits` \
         / `get_specializations` again for those ids.\n\
         2. For any equipment ids that come back unresolved (slot entries with `id` \
         fields under `equipment`), use the `get_items` tool with the full list of \
         ids in one call.\n\
         3. Summarise the build: profession + elite spec, the role it plays, the \
         gamemode it likely fits, rotation hints derived from the chosen skills, and \
         gear quality (ascended vs exotic, sigils, runes, infusions).\n\
         4. Call out anything missing or unusual (empty equipment slots, mismatched \
         stat sets, unexpected utility-skill choices) so the user knows what to look \
         at."
    );
    Ok(finish_prompt(
        "Fetch a character's build and equipment, resolve all IDs to names, and produce a build \
         summary.",
        body,
    ))
}

fn render_compare_to_meta(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let character = require_arg(args, PROMPT_COMPARE_TO_META, "character")?;
    let api_key = require_arg(args, PROMPT_COMPARE_TO_META, "api_key")?;
    let body = match optional_arg(args, "gamemode") {
        Some(gm) => format!(
            "You are comparing a Guild Wars 2 character to curated meta builds.\n\n\
             Steps to follow:\n\n\
             1. Call `get_character_build` with `character=\"{character}\"` and \
             `api_key=\"{api_key}\"` to capture the current build (the response \
             pre-resolves skill/trait/spec names).\n\
             2. Pick the catalog source that fits `gamemode=\"{gm}\"`: \
             `discretize` for fractals, `snowcrows` for raids / open_world / pvp / wvw \
             (curated meta picks), `metabattle` for anything else or as a fallback when \
             Snow Crows returns no builds for that profession.\n\
             3. Call `list_catalog_builds` with that source, the character's \
             profession (lower-cased), and `gamemode=\"{gm}\"`. Pick the best match \
             (highest rating if present, otherwise the closest role/elite-spec match).\n\
             4. Call `get_catalog_build` with the chosen `source` and `slug` to get \
             the canonical build details.\n\
             5. Produce a gap analysis: traits + skills that differ, equipment / stat \
             differences, sigils/runes, and any rotation steps the character can't \
             execute with its current setup. Recommend the smallest set of changes that \
             would close the gap."
        ),
        None => format!(
            "You are comparing a Guild Wars 2 character to curated meta builds, but the \
             gamemode was not provided.\n\n\
             Step 1: Ask the user which gamemode they're targeting (one of `fractals`, \
             `raids`, `open_world`, `pvp`, `wvw`) and wait for their answer before \
             proceeding.\n\n\
             Once the user responds, follow the standard flow:\n\n\
             1. Call `get_character_build` with `character=\"{character}\"` and \
             `api_key=\"{api_key}\"`.\n\
             2. Pick the catalog source that fits the user's gamemode: `discretize` for \
             fractals, `snowcrows` for raids / open_world / pvp / wvw, `metabattle` for \
             strikes or as a fallback when Snow Crows is empty for that profession.\n\
             3. Call `list_catalog_builds` with that source plus the profession and \
             gamemode filters, then `get_catalog_build` with the best match's slug.\n\
             4. Produce a gap analysis: traits + skills that differ, equipment / stat \
             differences, and the smallest changes that would close the gap."
        ),
    };
    Ok(finish_prompt(
        "Compare a character's current build to curated meta builds for their profession.",
        body,
    ))
}

fn render_decode_and_explain(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let code = require_arg(args, PROMPT_DECODE_AND_EXPLAIN, "code")?;
    let body = format!(
        "You are explaining a Guild Wars 2 build chat code in plain English.\n\n\
         Steps to follow:\n\n\
         1. Call `decode_build_code` with `code=\"{code}\"` to extract the \
         profession byte, specialization ids, trait choices, and palette skill ids \
         (with their resolved api skill ids and profession name).\n\
         2. Collect the resolved trait ids across all three specialization slots and \
         pass them to `get_traits` in one call. Pass the resolved api skill ids to \
         `get_skills` in one call. If the build references specialization ids you \
         want descriptions for, pass them to `get_specializations`.\n\
         3. Produce a plain-English explanation of what the build does: profession + \
         elite spec (named, not numeric), the role it fills, key trait synergies, \
         and what each skill in the bar contributes. Keep it readable for a player \
         who has not seen this build before."
    );
    Ok(finish_prompt(
        "Decode a Guild Wars 2 build chat code and explain what the build does.",
        body,
    ))
}

fn render_recommend_build(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let profession = require_arg(args, PROMPT_RECOMMEND_BUILD, "profession")?;
    let gamemode = require_arg(args, PROMPT_RECOMMEND_BUILD, "gamemode")?;
    let experience = optional_arg(args, "experience").unwrap_or_else(|| "intermediate".to_owned());
    // Source routing:
    // - fractals → Discretize (authoritative).
    // - raids / open_world / pvp / wvw → Snow Crows (curated meta picks).
    // - strikes → MetaBattle (Snow Crows dropped its top-level strikes page).
    // - anything else → MetaBattle (broadest coverage).
    // The prompt body instructs the LLM to fall back to MetaBattle if the
    // primary source returns nothing for the requested profession.
    let source = match gamemode.as_str() {
        "fractals" => "discretize",
        "raids" | "open_world" | "open-world" | "pvp" | "wvw" => "snowcrows",
        _ => "metabattle",
    };
    let body = format!(
        "You are recommending a Guild Wars 2 meta build.\n\n\
         Steps to follow:\n\n\
         1. Call `list_catalog_builds` with `source=\"{source}\"`, \
         `profession=\"{profession}\"`, and `gamemode=\"{gamemode}\"`. Pick the \
         top-rated match (or the closest role match if the catalog doesn't carry \
         ratings). If the result is empty, retry once with `source=\"metabattle\"` \
         — its community-wiki coverage is broader.\n\
         2. Call `get_catalog_build` with the chosen `source` and `slug` to \
         fetch the full build detail.\n\
         3. Explain the build to a {experience} player. For `beginner`, lead with \
         the role the build plays and avoid jargon; describe the rotation as a \
         short, ordered list. For `intermediate`, include trait synergies and key \
         boon outputs. For `expert`, include CC priorities, edge-case rotations, \
         and the trade-offs vs adjacent builds in the same role.\n\
         4. Always finish with the chat code (if the catalog provided one) and the \
         source URL so the user can verify."
    );
    Ok(finish_prompt(
        "Recommend a curated meta build for a profession + gamemode and explain it.",
        body,
    ))
}

fn render_daily_routine(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_DAILY_ROUTINE, "api_key")?;
    let character = optional_arg(args, "character");

    let character_step = if let Some(ref name) = character {
        format!(
            "5. Call `get_character_build` with `character=\"{name}\"` and \
             `api_key=\"{api_key}\"` to know what setup the player will be running.\n\
             "
        )
    } else {
        "5. (No character was specified — skip the build lookup; if a particular \
         activity needs a build choice, ask the user which character to run.)\n"
            .to_owned()
    };

    let body = format!(
        "You are helping the user plan today's Guild Wars 2 routine. The server itself \
         has no memory of this user — anything you 'know' about them comes from your \
         own conversation history with them. Workflow:\n\n\
         1. Call `get_dailies` with `api_key=\"{api_key}\"` (default `which=\"daily\"`) \
         for today's Wizard's Vault objectives. Each objective embeds `title`, `track` \
         (PvE/PvP/WvW), Astral Acclaim `acclaim`, and `progress_current`/\
         `progress_complete`/`claimed` — so you can directly see what's left to do.\n\
         2. (Optional) Call `get_dailies` again with `which=\"weekly\"` for the weekly \
         track — useful when the user has time for a longer session.\n\
         3. Call `get_account_achievements` with `api_key=\"{api_key}\"` and the default \
         `summary=true` to see the player's broader in-flight achievement progress — \
         not strictly required for daily routine but lets you spot collections that are \
         close to completion.\n\
         4. Call `get_account_raids` and `get_account_dungeons` (both with \
         `api_key=\"{api_key}\"`) to see what's been cleared this week (raids) and \
         today (dungeons). Note the cadence difference: raids reset Monday 07:30 UTC, \
         dungeon paths reset daily.\n\
         {character_step}\
         6. **Recall what you know about this player from your previous conversations \
         with them**: their preferred game modes, how much time they typically have, \
         what long-term goal they're working toward (legendary, mastery, achievement, \
         collection). Do NOT ask the API server for this — it stores nothing about \
         users.\n\
         7. Produce a checklist for today, ordered by reward-per-time, scoped to the \
         player's available time and stated preferences. Mark items they have already \
         completed (`claimed=true`).\n\n\
         If you have no prior context for this player, ask: \"How much time do you have \
         today, and what do you feel like — open world, instanced PvE, PvP, WvW?\" \
         before producing the checklist."
    );
    Ok(finish_prompt(
        "Plan today's GW2 routine: daily achievements, dungeon paths, raid clears, fused with \
         what the AI remembers about the player.",
        body,
    ))
}

fn render_next_zone(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_NEXT_ZONE, "api_key")?;
    let character = require_arg(args, PROMPT_NEXT_ZONE, "character")?;
    let body = format!(
        "You are recommending the next zone for {character} to explore in Guild Wars 2. \
         The server has no memory of this player — anything you 'know' about them comes \
         from your own conversation history with them.\n\n\
         Steps to follow:\n\n\
         1. Call `get_character_build` with `character=\"{character}\"` and \
         `api_key=\"{api_key}\"` to see profession, elite spec, and current setup.\n\
         2. Call `get_account` with `api_key=\"{api_key}\"` to see character level (via \
         age proxy), expansion `access` flags (HoT/PoF/EoD/SotO/JW/...), and fractal \
         level.\n\
         3. Call `get_account_masteries` with `api_key=\"{api_key}\"` to see mastery \
         track progress — many zones gate behind specific masteries (gliding, mounts, \
         jade-bot, skiff, fishing, etc.).\n\
         4. Call `get_account_achievements` with `api_key=\"{api_key}\"` (default \
         `summary=true`) to spot any in-progress map-completion or zone-related \
         collections.\n\
         5. **Recall this player's stated progression goals from your previous \
         conversations**: legendary they're crafting, achievement set they're chasing, \
         story chapter they want to finish, mount they want, etc. Do NOT ask the server \
         — it stores nothing about users.\n\
         6. Suggest one specific zone with a one-paragraph rationale tying it to (a) \
         the character's level/build appropriateness, (b) the masteries they have \
         unlocked, and (c) what they have said they're working toward.\n\n\
         If you have no prior context for this player, ask: \"Are you working toward \
         something specific (legendary, mount, story, achievement), or do you just want \
         a fun map you haven't done?\" before recommending."
    );
    Ok(finish_prompt(
        "Recommend the next zone for a character to explore based on their build, expansion \
         access, and mastery progress.",
        body,
    ))
}

fn render_next_collection(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_NEXT_COLLECTION, "api_key")?;
    let body = format!(
        "You are finding the most rewarding Guild Wars 2 achievement collection for the \
         player to finish next. The server itself has no memory of this user — anything \
         you 'know' about them comes from your own conversation history.\n\n\
         Steps to follow:\n\n\
         1. Call `get_account_achievements` with `api_key=\"{api_key}\"` and \
         `summary=false` so you see the full in-progress + completed list (you need the \
         raw `current` / `max` numbers to rank by closeness-to-completion).\n\
         2. Mentally filter to entries that look like collections — bits-based \
         achievements with high `current` relative to `max` are the ripest. Discard the \
         ones the player has not started.\n\
         3. **Recall what this player has said in previous conversations about their \
         interest profile**: legendaries, skins, titles, masteries, gen3 weapons, \
         specific story content. Use that to weight your candidates.\n\
         4. Suggest the best 1–3 candidate collections with their current progress \
         (`X / Y` complete) and what completing each one unlocks (precursor, skin, \
         title, mastery point, legendary chunk, etc.).\n\n\
         If you have no prior context for this player, ask which axis matters most to \
         them — legendary progress, skin/wardrobe completion, title/AP, or mastery \
         points — before ranking the candidates."
    );
    Ok(finish_prompt(
        "Find the most rewarding achievement collection to finish next.",
        body,
    ))
}

fn render_mount_progression(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_MOUNT_PROGRESSION, "api_key")?;
    let body = format!(
        "You are helping the player plan their next Guild Wars 2 mount unlock. The \
         server itself has no memory of this user — anything you 'know' about them \
         comes from your own conversation history.\n\n\
         Steps to follow:\n\n\
         1. Call `get_account` with `api_key=\"{api_key}\"` and confirm expansion \
         access from the `access` array. Mounts gate behind specific expansions: PoF \
         unlocks the original five (raptor, springer, skimmer, jackal, griffon), \
         beetle/skyscale come from PoF + LWS4, EoD adds the siege turtle and skiff, \
         SotO adds the skyscale rework and the warclaw is unlocked via WvW.\n\
         2. Call `get_account_masteries` with `api_key=\"{api_key}\"` to see Mount \
         Mastery track progress — many of the more-advanced mount masteries (Beetle's \
         Roll, Skimmer underwater, Skyscale Air mastery, etc.) need spending mastery \
         points to unlock skills.\n\
         3. **Recall** which mounts the player has previously mentioned having \
         unlocked, which they've said they want, and which content they're heading \
         toward (raids, open-world meta events, achievement hunting). Do NOT ask the \
         server — it stores nothing about users.\n\
         4. Suggest the next mount-related step: which collection or mastery to push, \
         what it unlocks, where to start (zone + collection + first achievement). If \
         the player owns no mount-bearing expansion, recommend that first.\n\n\
         If you have no prior context, ask which mount they have already unlocked and \
         which one they're most excited about before recommending."
    );
    Ok(finish_prompt(
        "Plan the next mount unlock based on expansion access and Mount Mastery progress.",
        body,
    ))
}

fn render_legendary_progress(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_LEGENDARY_PROGRESS, "api_key")?;
    let legendary = optional_arg(args, "legendary");

    let intro = if let Some(ref name) = legendary {
        format!("You are tracking the player's progress toward {name}.\n\n")
    } else {
        "You are tracking the player's legendary crafting progress. If you can recall \
         which legendary they have been working toward in your previous conversations \
         with them, use that. Otherwise, defer the deep dive and offer choices (see \
         the bottom of these instructions).\n\n"
            .to_owned()
    };

    let body = format!(
        "{intro}\
         The server itself stores nothing about this user — anything you 'know' comes \
         from your own conversation history with them.\n\n\
         Steps to follow:\n\n\
         1. **Recall** which legendary the player has been chasing across your previous \
         conversations (or use the `legendary` arg if it was supplied). Hold that \
         choice in mind for the rest of these steps.\n\
         2. Call `get_wallet` with `api_key=\"{api_key}\"` for gold + the key currencies \
         this legendary needs (mystic clovers, philosopher's stones, spirit shards, \
         provisioner tokens, etc.).\n\
         3. Call `get_account_achievements` with `api_key=\"{api_key}\"` (default \
         `summary=true`) to surface gift-related collections in flight — every \
         legendary needs at least one collection, and some need multiple.\n\
         4. Call `get_account_raids` and `get_account_dungeons` with \
         `api_key=\"{api_key}\"` if the legendary needs LI/LD or dungeon tokens.\n\
         5. Call `get_account` with `api_key=\"{api_key}\"` if the legendary's gifts \
         need WvW rank (Gift of Battle is unrelated, but some pieces want WvW track \
         currencies) or a fractal level.\n\
         6. Use `wiki_search` for the legendary's recipe page if you need authoritative \
         numbers for any single material — the wiki is the source of truth.\n\
         7. Produce a 'what's left' breakdown grouped by acquisition path: \
         gold-buyable now, achievement-locked (with the next achievement to chase), \
         raid- or instance-locked (with how many weeks of clears remain), and \
         time-gated (mystic clovers, provisioner tokens, etc.). End with a concrete \
         next-action suggestion.\n\n\
         If you have no prior context and no `legendary` arg was passed, list 3–4 \
         commonly-pursued first legendaries (e.g. The Predator, Bolt, Twilight, Bifrost) \
         with brief difficulty + theme notes and ask which one fits the player."
    );
    Ok(finish_prompt(
        "Track legendary crafting progress: gold, currencies, raids/WvW gates, recipe steps.",
        body,
    ))
}

fn render_weekly_roundup(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let api_key = require_arg(args, PROMPT_WEEKLY_ROUNDUP, "api_key")?;
    let body = format!(
        "You are summarising what's left this reset week in Guild Wars 2 for the \
         player. The server itself has no memory of this user — anything you 'know' \
         about them comes from your own conversation history.\n\n\
         Steps to follow:\n\n\
         1. Call `get_account_raids` with `api_key=\"{api_key}\"` for the encounter \
         ids cleared this week. Raids reset every Monday 07:30 UTC.\n\
         2. Call `get_account` with `api_key=\"{api_key}\"` for `fractal_level` and \
         `wvw_rank` — use these to scope the fractal/WvW recommendations to a sensible \
         tier.\n\
         3. Call `get_account_dungeons` with `api_key=\"{api_key}\"` for dungeon paths \
         cleared today. **Flag this distinction explicitly to the user**: dungeons \
         reset daily, NOT weekly, so 'left this week' for dungeons means 'left today \
         × the days remaining until next Monday'.\n\
         4. **Recall** which content this player typically clears each week from your \
         previous conversations: which raid wings, which strikes, T4 fractal CMs, \
         WvW participation rank, etc. Skip recommendations for content they've told \
         you they don't run.\n\
         5. Produce a brief 'done / left' list partitioned into raids, fractals \
         (T4 + CMs separately), strikes, dungeons (with the daily caveat), and WvW \
         participation. Suggest the highest-value remaining items first.\n\n\
         If you have no prior context for this player, ask which categories they \
         actually run before listing every possible weekly clear — many players \
         deliberately skip whole pillars."
    );
    Ok(finish_prompt(
        "Summarise what's left this reset week: raids, fractals, WvW, dungeons.",
        body,
    ))
}

pub(super) fn render_prompt(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    match name {
        PROMPT_ANALYZE_CHARACTER => render_analyze_character(args),
        PROMPT_COMPARE_TO_META => render_compare_to_meta(args),
        PROMPT_DECODE_AND_EXPLAIN => render_decode_and_explain(args),
        PROMPT_RECOMMEND_BUILD => render_recommend_build(args),
        PROMPT_DAILY_ROUTINE => render_daily_routine(args),
        PROMPT_NEXT_ZONE => render_next_zone(args),
        PROMPT_NEXT_COLLECTION => render_next_collection(args),
        PROMPT_MOUNT_PROGRESSION => render_mount_progression(args),
        PROMPT_LEGENDARY_PROGRESS => render_legendary_progress(args),
        PROMPT_WEEKLY_ROUNDUP => render_weekly_roundup(args),
        other => Err(PromptError::NotFound(other.to_owned())),
    }
}
