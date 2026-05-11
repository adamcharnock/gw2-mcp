//! `gw2-mcp print-config` — emit a Claude Desktop config snippet for
//! this binary so users don't have to hand-write the JSON.
//!
//! Stdout gets pretty-printed JSON ready to paste; stderr gets a short
//! tip about where to put it. Pipe-friendly: `gw2-mcp print-config | pbcopy`.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

/// Args parsed from the subcommand.
#[derive(Debug, Default, Clone)]
pub struct Args {
    pub api_key: Option<String>,
    pub bottle: Option<String>,
    /// Override `current_exe()` for tests.
    pub binary_path: Option<PathBuf>,
}

pub fn run(args: Args) -> ExitCode {
    match build_config(&args) {
        Ok(snippet) => {
            println!("{snippet}");
            eprintln!();
            eprintln!(
                "Paste the above into ~/Library/Application Support/Claude/claude_desktop_config.json"
            );
            eprintln!("(merge with any existing \"mcpServers\" entries).");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn build_config(args: &Args) -> anyhow::Result<String> {
    let binary = match &args.binary_path {
        Some(p) => p.clone(),
        None => std::env::current_exe()?,
    };
    let mut env_map = serde_json::Map::new();
    if let Some(k) = args.api_key.as_deref() {
        env_map.insert("GW2_API_KEY".to_owned(), json!(k));
    }
    if let Some(b) = args.bottle.as_deref() {
        env_map.insert("GW2_BOTTLE".to_owned(), json!(b));
    }
    let snippet = json!({
        "mcpServers": {
            "gw2": {
                "command": binary.display().to_string(),
                "args": [],
                "env": env_map,
            }
        }
    });
    Ok(serde_json::to_string_pretty(&snippet)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid JSON")
    }

    #[test]
    fn minimal_config_has_command_args_empty_env() {
        let args = Args {
            binary_path: Some(PathBuf::from("/path/to/gw2-mcp")),
            ..Default::default()
        };
        let snippet = build_config(&args).unwrap();
        let v = parse(&snippet);
        assert_eq!(v["mcpServers"]["gw2"]["command"], "/path/to/gw2-mcp");
        assert_eq!(v["mcpServers"]["gw2"]["args"], json!([]));
        assert_eq!(v["mcpServers"]["gw2"]["env"], json!({}));
    }

    #[test]
    fn api_key_lands_in_env() {
        let args = Args {
            binary_path: Some(PathBuf::from("/p")),
            api_key: Some("ABC-DEF".to_owned()),
            ..Default::default()
        };
        let snippet = build_config(&args).unwrap();
        let v = parse(&snippet);
        assert_eq!(v["mcpServers"]["gw2"]["env"]["GW2_API_KEY"], "ABC-DEF");
    }

    #[test]
    fn bottle_lands_in_env() {
        let args = Args {
            binary_path: Some(PathBuf::from("/p")),
            bottle: Some("My Bottle".to_owned()),
            ..Default::default()
        };
        let snippet = build_config(&args).unwrap();
        let v = parse(&snippet);
        assert_eq!(v["mcpServers"]["gw2"]["env"]["GW2_BOTTLE"], "My Bottle");
    }

    #[test]
    fn both_api_key_and_bottle_can_be_set() {
        let args = Args {
            binary_path: Some(PathBuf::from("/p")),
            api_key: Some("AAA".to_owned()),
            bottle: Some("BBB".to_owned()),
        };
        let snippet = build_config(&args).unwrap();
        let v = parse(&snippet);
        let env = &v["mcpServers"]["gw2"]["env"];
        assert_eq!(env["GW2_API_KEY"], "AAA");
        assert_eq!(env["GW2_BOTTLE"], "BBB");
    }

    #[test]
    fn output_is_pretty_printed() {
        // Newlines + indentation present => not single-line.
        let args = Args {
            binary_path: Some(PathBuf::from("/p")),
            ..Default::default()
        };
        let snippet = build_config(&args).unwrap();
        assert!(snippet.contains('\n'));
        assert!(snippet.contains("  ")); // 2-space indent from to_string_pretty
    }
}
