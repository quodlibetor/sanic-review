//! Models to suggest at setup's model prompt. Suggestions only: any answer is
//! accepted, since Bedrock, Vertex and proxy ids can't be listed.

use std::path::{Path, PathBuf};

use sanic_core::config::AUTO_MODEL;
use serde_json::Value;

/// `claude` resolves these to the latest model of each family.
const ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// Environment variables `claude` reads model ids from, whether set in the
/// process or in the settings' `env` block.
const ENV_VARS: [&str; 4] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// Your user-level Claude settings, if they can be read.
pub fn read_claude_settings() -> Option<String> {
    let path = settings_path(
        |var| std::env::var(var).ok(),
        std::env::home_dir().as_deref(),
    )?;
    std::fs::read_to_string(path).ok()
}

/// `settings.json` in [`claude_dir`].
fn settings_path(env: impl Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<PathBuf> {
    Some(claude_dir(env, home)?.join("settings.json"))
}

/// Your user-level Claude directory: `$CLAUDE_CONFIG_DIR`, falling back to
/// `~/.claude`.
pub fn claude_dir(env: impl Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<PathBuf> {
    env("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".claude")))
}

/// [`AUTO_MODEL`], then `current`, the aliases, and the models named in the
/// `settings` file contents and by `env`, without duplicates, ignoring case;
/// the first spelling wins. Settings that don't parse are skipped.
pub fn known_models(
    current: Option<&str>,
    settings: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let settings = settings
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_default();
    let available = settings
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let settings_env = ENV_VARS
        .iter()
        .filter_map(|var| settings.get("env").and_then(|e| e.get(var)));
    let from_settings = settings
        .get("model")
        .into_iter()
        .chain(available)
        .chain(settings_env)
        .filter_map(|m| m.as_str().map(String::from));
    let candidates = [AUTO_MODEL.to_owned()]
        .into_iter()
        .chain(current.map(String::from))
        .chain(ALIASES.map(String::from))
        .chain(from_settings)
        .chain(ENV_VARS.iter().filter_map(|var| env(var)));
    let mut models: Vec<String> = Vec::new();
    for model in candidates {
        let model = model.trim();
        if !model.is_empty() && !models.iter().any(|m| m.eq_ignore_ascii_case(model)) {
            models.push(model.to_owned());
        }
    }
    models
}

/// The `models` containing `input`, ignoring case; all of them for no input.
pub fn matching(models: &[String], input: &str) -> Vec<String> {
    let input = input.trim().to_lowercase();
    models
        .iter()
        .filter(|m| m.to_lowercase().contains(&input))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_come_from_every_source_in_order_without_duplicates() {
        let settings = r#"{
            "model": "sonnet",
            "availableModels": ["claude-opus-5-5", 3, "  ", "us.anthropic.claude-sonnet-5-v1:0"],
            "env": { "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gateway-haiku", "OTHER": "x" }
        }"#;
        let env = |var: &str| match var {
            "ANTHROPIC_MODEL" => Some("my-proxy-model".into()),
            "ANTHROPIC_DEFAULT_OPUS_MODEL" => Some("claude-opus-5-5".into()),
            _ => None,
        };
        assert_eq!(
            known_models(Some("custom"), Some(settings), env),
            [
                "auto",
                "custom",
                "opus",
                "sonnet",
                "haiku",
                "fable",
                "claude-opus-5-5",
                "us.anthropic.claude-sonnet-5-v1:0",
                "gateway-haiku",
                "my-proxy-model",
            ]
        );
    }

    #[test]
    fn unparseable_settings_are_skipped() {
        let models = known_models(None, Some("not json"), |_| None);
        assert_eq!(models, ["auto", "opus", "sonnet", "haiku", "fable"]);
    }

    #[test]
    fn duplicates_ignore_case_and_keep_the_first_spelling() {
        let settings = r#"{"availableModels": ["Claude-Opus-5-5", "claude-opus-5-5", "OPUS"]}"#;
        let models = known_models(Some("Auto"), Some(settings), |_| None);
        assert_eq!(
            models,
            [
                "auto",
                "opus",
                "sonnet",
                "haiku",
                "fable",
                "Claude-Opus-5-5"
            ]
        );
    }

    #[test]
    fn settings_live_in_claude_config_dir_or_else_home() {
        let home = Path::new("/home/u");
        let set = |var: &str| (var == "CLAUDE_CONFIG_DIR").then(|| "/cfg".to_owned());
        assert_eq!(
            settings_path(set, Some(home)),
            Some(PathBuf::from("/cfg/settings.json"))
        );
        assert_eq!(
            settings_path(|_| Some(String::new()), Some(home)),
            Some(PathBuf::from("/home/u/.claude/settings.json"))
        );
        assert_eq!(settings_path(|_| None, None), None);
    }

    #[test]
    fn matching_ignores_case_and_shows_all_for_no_input() {
        let models = known_models(None, None, |_| None);
        assert_eq!(matching(&models, ""), models);
        assert_eq!(matching(&models, "OP"), ["opus"]);
        assert!(matching(&models, "bedrock-id").is_empty());
    }
}
