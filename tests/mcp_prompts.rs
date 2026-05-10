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
