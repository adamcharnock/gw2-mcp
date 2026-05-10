//! Pin the prompt surface area exposed via MCP `prompts/list` + `prompts/get`.
//!
//! Prompt rendering is pure (no I/O), so we exercise the helper directly
//! rather than driving the stdio protocol. The MCP `ServerHandler` calls
//! the same renderer.

use gw2_mcp::adapters::McpServer;
use rmcp::model::PromptMessageContent;
use serde_json::json;

const PROMPT_NAMES: &[&str] = &[
    "analyze-character",
    "compare-to-meta",
    "decode-and-explain",
    "recommend-build",
    // Tier 6A — PvE coaching prompts.
    "daily-routine",
    "next-zone",
    "next-collection",
    "mount-progression",
    "legendary-progress",
    "weekly-roundup",
];

#[test]
fn list_prompts_publishes_all_four_with_descriptions_and_args() {
    let prompts = McpServer::list_prompts_for_test();
    let names: Vec<String> = prompts.iter().map(|p| p.name.clone()).collect();
    for expected in PROMPT_NAMES {
        assert!(
            names.iter().any(|n| n == expected),
            "missing prompt {expected}, got {names:?}"
        );
    }
    assert_eq!(prompts.len(), PROMPT_NAMES.len());
    for p in &prompts {
        assert!(
            p.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{}: description must be set and non-empty",
            p.name
        );
        assert!(
            p.arguments.as_ref().is_some_and(|a| !a.is_empty()),
            "{}: every prompt declares at least one argument",
            p.name
        );
    }
}

#[test]
fn analyze_character_prompt_renders_the_workflow_recipe() {
    let result = McpServer::get_prompt_for_test(
        "analyze-character",
        json!({"character": "Snowflake", "api_key": "abcd-efgh"}),
    )
    .unwrap();
    assert!(result.description.is_some());
    assert_eq!(result.messages.len(), 1);
    let body = match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    // Tool routing: must explicitly tell the LLM which tools to call.
    assert!(
        body.contains("get_character_build"),
        "must reference get_character_build: {body}"
    );
    assert!(
        body.contains("character=\"Snowflake\""),
        "must inline the character arg: {body}"
    );
    assert!(
        body.contains("api_key=\"abcd-efgh\""),
        "must inline the api_key arg: {body}"
    );
    assert!(
        body.contains("get_items"),
        "must mention get_items for unresolved equipment: {body}"
    );
}

#[test]
fn analyze_character_prompt_rejects_missing_required_arg() {
    let err =
        McpServer::get_prompt_for_test("analyze-character", json!({"character": "Snowflake"}))
            .unwrap_err();
    assert!(err.contains("api_key"), "got: {err}");
}

#[test]
fn compare_to_meta_with_gamemode_picks_correct_source_per_mode() {
    // fractals -> discretize
    let result = McpServer::get_prompt_for_test(
        "compare-to-meta",
        json!({"character": "X", "api_key": "k", "gamemode": "fractals"}),
    )
    .unwrap();
    let body = match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(
        body.contains("discretize"),
        "fractals -> discretize: {body}"
    );
    assert!(body.contains("list_catalog_builds"));
    assert!(body.contains("get_catalog_build"));

    // raids -> snowcrows
    let body = match &McpServer::get_prompt_for_test(
        "compare-to-meta",
        json!({"character": "X", "api_key": "k", "gamemode": "raids"}),
    )
    .unwrap()
    .messages[0]
        .content
    {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(body.contains("snowcrows"), "raids -> snowcrows: {body}");

    // open_world -> metabattle
    let body = match &McpServer::get_prompt_for_test(
        "compare-to-meta",
        json!({"character": "X", "api_key": "k", "gamemode": "open_world"}),
    )
    .unwrap()
    .messages[0]
        .content
    {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(
        body.contains("metabattle"),
        "open_world -> metabattle: {body}"
    );
}

#[test]
fn compare_to_meta_without_gamemode_asks_the_user() {
    let result = McpServer::get_prompt_for_test(
        "compare-to-meta",
        json!({"character": "Yarn", "api_key": "key"}),
    )
    .unwrap();
    let body = match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    let lower = body.to_lowercase();
    assert!(
        lower.contains("ask the user"),
        "must instruct the LLM to ask the user when gamemode is missing: {body}"
    );
    assert!(
        body.contains("fractals") && body.contains("raids") && body.contains("open_world"),
        "must list the valid gamemodes for the user to pick from: {body}"
    );
}

#[test]
fn decode_and_explain_prompt_chains_decode_then_resolution_calls() {
    let code = "[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]";
    let result =
        McpServer::get_prompt_for_test("decode-and-explain", json!({"code": code})).unwrap();
    let body = match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(
        body.contains("decode_build_code"),
        "must reference decode_build_code: {body}"
    );
    assert!(
        body.contains(&format!("code=\"{code}\"")),
        "must inline the code arg: {body}"
    );
    assert!(
        body.contains("get_skills") && body.contains("get_traits"),
        "must chain to id-resolution tools: {body}"
    );
}

#[test]
fn decode_and_explain_rejects_missing_code() {
    let err = McpServer::get_prompt_for_test("decode-and-explain", json!({})).unwrap_err();
    assert!(err.contains("code"), "got: {err}");
}

#[test]
fn recommend_build_picks_source_from_gamemode_and_tunes_for_experience() {
    let result = McpServer::get_prompt_for_test(
        "recommend-build",
        json!({"profession": "guardian", "gamemode": "fractals", "experience": "expert"}),
    )
    .unwrap();
    let body = match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(
        body.contains("source=\"discretize\""),
        "fractals -> discretize: {body}"
    );
    assert!(body.contains("profession=\"guardian\""));
    assert!(body.contains("gamemode=\"fractals\""));
    assert!(
        body.contains("expert"),
        "must thread experience into the tone: {body}"
    );
    assert!(body.contains("get_catalog_build"));
}

#[test]
fn recommend_build_defaults_experience_to_intermediate_when_omitted() {
    let body = match &McpServer::get_prompt_for_test(
        "recommend-build",
        json!({"profession": "warrior", "gamemode": "raids"}),
    )
    .unwrap()
    .messages[0]
        .content
    {
        PromptMessageContent::Text { text } => text.clone(),
        _ => unreachable!(),
    };
    assert!(
        body.contains("source=\"snowcrows\""),
        "raids -> snowcrows: {body}"
    );
    assert!(body.contains("intermediate"), "default experience: {body}");
}

#[test]
fn unknown_prompt_errors() {
    let err = McpServer::get_prompt_for_test("not-a-prompt", json!({})).unwrap_err();
    assert!(err.contains("unknown prompt"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Tier 6A — PvE coaching prompts.
//
// Each prompt's body must:
//   1. Mention every tool name it instructs the LLM to call.
//   2. Substitute the api_key (and other args) into the rendered text.
//   3. Tell the LLM to draw on its own conversation memory ("recall") rather
//      than implying server-side state — the server stores nothing.
// ---------------------------------------------------------------------------

const TEST_KEY: &str = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE-FFFFFFFF-GGGG-HHHH-IIII-JJJJJJJJJJJJ";

fn body_of(name: &'static str, args: serde_json::Value) -> String {
    let result = McpServer::get_prompt_for_test(name, args)
        .unwrap_or_else(|e| panic!("{name} render failed: {e}"));
    match &result.messages[0].content {
        PromptMessageContent::Text { text } => text.clone(),
        other => panic!("{name}: expected text content, got {other:?}"),
    }
}

#[test]
fn daily_routine_prompt_chains_dailies_achievements_raids_dungeons_and_recall() {
    let body = body_of(
        "daily-routine",
        json!({"api_key": TEST_KEY, "character": "Snowflake"}),
    );
    for tool in [
        "get_dailies",
        "get_account_achievements",
        "get_account_raids",
        "get_account_dungeons",
        "get_character_build",
    ] {
        assert!(body.contains(tool), "must reference {tool}: {body}");
    }
    assert!(
        body.contains(&format!("api_key=\"{TEST_KEY}\"")),
        "must inline api_key: {body}"
    );
    assert!(
        body.contains("character=\"Snowflake\""),
        "must inline character: {body}"
    );
    assert!(
        body.to_lowercase().contains("recall"),
        "must instruct the LLM to recall its own memory of the player: {body}"
    );
    // Server-side amnesia clause — must explicitly tell the AI the server
    // stores nothing about users.
    assert!(
        body.to_lowercase().contains("no memory") || body.to_lowercase().contains("stores nothing"),
        "must clarify the server has no per-user state: {body}"
    );
}

#[test]
fn daily_routine_skips_character_build_step_when_arg_omitted() {
    let body = body_of("daily-routine", json!({"api_key": TEST_KEY}));
    assert!(
        !body.contains("character=\""),
        "must not invent a character: {body}"
    );
    assert!(
        body.contains("get_dailies"),
        "still references the always-on tools: {body}"
    );
}

#[test]
fn daily_routine_requires_api_key() {
    let err = McpServer::get_prompt_for_test("daily-routine", json!({})).unwrap_err();
    assert!(err.contains("api_key"), "got: {err}");
}

#[test]
fn next_zone_prompt_chains_build_account_masteries_achievements_and_recall() {
    let body = body_of(
        "next-zone",
        json!({"api_key": TEST_KEY, "character": "Vesta"}),
    );
    for tool in [
        "get_character_build",
        "get_account",
        "get_account_masteries",
        "get_account_achievements",
    ] {
        assert!(body.contains(tool), "must reference {tool}: {body}");
    }
    assert!(body.contains("character=\"Vesta\""), "args inlined: {body}");
    assert!(body.contains(&format!("api_key=\"{TEST_KEY}\"")));
    assert!(
        body.to_lowercase().contains("recall"),
        "must lean on conversation memory: {body}"
    );
}

#[test]
fn next_zone_requires_both_api_key_and_character() {
    let err =
        McpServer::get_prompt_for_test("next-zone", json!({"api_key": TEST_KEY})).unwrap_err();
    assert!(err.contains("character"), "got: {err}");
}

#[test]
fn next_collection_prompt_passes_summary_false_and_recalls() {
    let body = body_of("next-collection", json!({"api_key": TEST_KEY}));
    assert!(body.contains("get_account_achievements"));
    assert!(
        body.contains("summary=false"),
        "must request raw list to see in-progress collections: {body}"
    );
    assert!(body.contains(&format!("api_key=\"{TEST_KEY}\"")));
    assert!(
        body.to_lowercase().contains("recall"),
        "must lean on conversation memory: {body}"
    );
}

#[test]
fn mount_progression_prompt_uses_account_and_masteries_and_recalls() {
    let body = body_of("mount-progression", json!({"api_key": TEST_KEY}));
    assert!(body.contains("get_account"));
    assert!(body.contains("get_account_masteries"));
    assert!(body.contains(&format!("api_key=\"{TEST_KEY}\"")));
    assert!(
        body.contains("PoF") || body.to_lowercase().contains("path of fire"),
        "must mention the relevant expansion gate: {body}"
    );
    assert!(
        body.to_lowercase().contains("recall"),
        "must lean on conversation memory: {body}"
    );
}

#[test]
fn legendary_progress_prompt_chains_wallet_achievements_raids_dungeons_account_and_wiki() {
    let body = body_of(
        "legendary-progress",
        json!({"api_key": TEST_KEY, "legendary": "Twilight"}),
    );
    for tool in [
        "get_wallet",
        "get_account_achievements",
        "get_account_raids",
        "get_account_dungeons",
        "get_account",
        "wiki_search",
    ] {
        assert!(body.contains(tool), "must reference {tool}: {body}");
    }
    assert!(body.contains("Twilight"), "must inline legendary: {body}");
    assert!(body.contains(&format!("api_key=\"{TEST_KEY}\"")));
}

#[test]
fn legendary_progress_without_legendary_falls_back_to_recall_or_offering_choices() {
    let body = body_of("legendary-progress", json!({"api_key": TEST_KEY}));
    assert!(
        body.to_lowercase().contains("recall") || body.to_lowercase().contains("which legendary"),
        "must either ask the AI to recall or offer choices when no legendary specified: {body}"
    );
    // Common starter-legendary names.
    assert!(
        body.contains("Twilight") || body.contains("Predator") || body.contains("Bolt"),
        "must offer at least one starter legendary as a fallback: {body}"
    );
}

#[test]
fn weekly_roundup_prompt_chains_raids_account_dungeons_and_flags_daily_caveat() {
    let body = body_of("weekly-roundup", json!({"api_key": TEST_KEY}));
    assert!(body.contains("get_account_raids"));
    assert!(body.contains("get_account"));
    assert!(body.contains("get_account_dungeons"));
    assert!(body.contains(&format!("api_key=\"{TEST_KEY}\"")));
    // The dungeons-reset-daily caveat is the load-bearing UX bit of this
    // prompt; if it disappears the LLM will mis-represent dungeon clears
    // as weekly progress.
    assert!(
        body.to_lowercase().contains("daily")
            && (body.to_lowercase().contains("not weekly") || body.contains("NOT weekly")),
        "must flag that dungeons reset daily, not weekly: {body}"
    );
    assert!(
        body.to_lowercase().contains("recall"),
        "must lean on conversation memory: {body}"
    );
}
