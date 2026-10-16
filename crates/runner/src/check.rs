//! The procedure check: which skills a procedure requires, and whether a
//! run's transcript bears out the agent's report of running them.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use sanic_core::run::{InlineComment, Severity, SkillReport, SkillStatus, Verdict};
use serde_json::Value;

use crate::plugin::{self, Plugin};

/// Claude Code's own skills a procedure may name. A plugin skill of the
/// same name doesn't replace one: `/code-review` means the built-in.
pub const BUILTIN_SKILLS: &[&str] = &["code-review", "simplify"];

/// One thing a run's answer gets wrong, for the agent and the reviewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub text: String,
    /// Whether asking the agent again could fix it: not for a skill that
    /// didn't load, or one the agent reported as failed.
    pub retry: bool,
}

impl Miss {
    fn retry(text: String) -> Self {
        Self { text, retry: true }
    }

    fn stuck(text: String) -> Self {
        Self { text, retry: false }
    }
}

/// `skill` as the Skill tool may be given it, without a leading `/` or the
/// plugin's prefix.
fn bare(skill: &str) -> &str {
    let skill = skill.trim_start_matches('/');
    skill
        .strip_prefix(plugin::NAME)
        .and_then(|s| s.strip_prefix(':'))
        .unwrap_or(skill)
}

/// The skills `procedure`, each `(own name, SKILL.md text)`, requires, as
/// the Skill tool invokes them: each `/name` it writes as a word that names
/// a built-in or one of `plugin`'s skills, but a procedure's own. `finders`,
/// when the profile sets it, replaces what the text names.
#[must_use]
pub fn required_skills(
    procedure: &[(String, String)],
    plugin: &BTreeMap<String, PathBuf>,
    finders: Option<&[String]>,
) -> Vec<String> {
    let invocable = |name: &str| {
        if BUILTIN_SKILLS.contains(&name) {
            Some(name.to_owned())
        } else if plugin.contains_key(name) {
            Some(Plugin::invocable(name))
        } else {
            None
        }
    };
    let names: Vec<&str> = match finders {
        Some(finders) => finders.iter().map(|f| bare(f)).collect(),
        None => procedure
            .iter()
            .flat_map(|(_, text)| slash_mentions(text))
            .filter(|name| !procedure.iter().any(|(own, _)| own == name))
            .collect(),
    };
    let mut required: Vec<String> = Vec::new();
    for skill in names.into_iter().filter_map(invocable) {
        if !required.contains(&skill) {
            required.push(skill);
        }
    }
    required
}

/// Each `/name` in `text` that's a word of its own: after the start of a
/// line, whitespace or `` `([*"' ``, and before anything but a name's
/// characters, `/` or `_`, so not a URL's or a path's segments. A
/// `sanic:` prefix is dropped.
fn slash_mentions(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    text.match_indices('/').filter_map(move |(i, _)| {
        let before = i.checked_sub(1).map(|b| bytes[b]);
        if !matches!(
            before,
            None | Some(b' ' | b'\n' | b'\t' | b'`' | b'(' | b'[' | b'*' | b'"' | b'\'')
        ) {
            return None;
        }
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == ':'))
            .unwrap_or(rest.len());
        let after = rest[end..].chars().next();
        if after.is_some_and(|c| c == '/' || c == '_' || c.is_ascii_alphanumeric()) {
            return None;
        }
        let name = bare(rest[..end].trim_end_matches([':', '-']));
        (!name.is_empty()).then_some(name)
    })
}

/// A Skill call in a transcript, and what it answered.
#[derive(Debug, Default)]
struct SkillCall {
    skill: String,
    args: String,
    answer: Option<String>,
    is_error: bool,
    /// Something it started called `StructuredOutput`, whose answer is lost.
    swallowed: bool,
}

impl SkillCall {
    /// Whether it ran on `diff` and gave its findings back.
    fn ran_on(&self, diff: &str) -> bool {
        let answered = self.answer.as_deref().is_some_and(|a| {
            let result = a.split_once("Result:").map_or(a, |(_, r)| r).trim();
            !result.is_empty() && result != "Skill execution completed"
        });
        self.args.contains(diff) && answered && !self.is_error && !self.swallowed
    }
}

/// What the transcripts, in order, say happened: the Skill calls, and the
/// skills the first session listed as loaded, if it said.
fn scan(transcripts: &[PathBuf]) -> (Vec<SkillCall>, Option<Vec<String>>) {
    let mut calls: Vec<SkillCall> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut loaded = None;
    for path in transcripts {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for event in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            if event["type"] == "system" && event["subtype"] == "init" {
                if loaded.is_none() {
                    loaded = event["skills"].as_array().map(|s| {
                        s.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    });
                }
                continue;
            }
            let parent = event["parent_tool_use_id"].as_str();
            let Some(blocks) = event["message"]["content"].as_array() else {
                continue;
            };
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") if block["name"] == "Skill" => {
                        let input = &block["input"];
                        if let Some(id) = block["id"].as_str() {
                            by_id.insert(id.to_owned(), calls.len());
                        }
                        calls.push(SkillCall {
                            skill: bare(input["skill"].as_str().unwrap_or_default()).to_owned(),
                            args: input["args"].as_str().unwrap_or_default().to_owned(),
                            ..SkillCall::default()
                        });
                    }
                    Some("tool_use") if block["name"] == "StructuredOutput" => {
                        if let Some(&i) = parent.and_then(|p| by_id.get(p)) {
                            calls[i].swallowed = true;
                        }
                    }
                    Some("tool_result") => {
                        let Some(&i) = block["tool_use_id"].as_str().and_then(|id| by_id.get(id))
                        else {
                            continue;
                        };
                        calls[i].is_error = block["is_error"].as_bool().unwrap_or(false);
                        calls[i].answer = Some(text_of(&block["content"]));
                    }
                    _ => {}
                }
            }
        }
    }
    (calls, loaded)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What a run's answer needs checking for.
pub struct Answer<'a> {
    pub report: &'a [SkillReport],
    pub verdict: Verdict,
    pub comments: &'a [InlineComment],
}

/// What `transcripts` and `answer` get wrong about `required`, run on the
/// diff at `diff`. Empty when nothing does.
#[must_use]
pub fn misses(
    transcripts: &[PathBuf],
    answer: &Answer<'_>,
    required: &[String],
    diff: &Path,
) -> Vec<Miss> {
    let (calls, loaded) = scan(transcripts);
    let diff = diff.display().to_string();
    let mut misses = Vec::new();
    for skill in required {
        let report = answer.report.iter().find(|r| bare(&r.skill) == bare(skill));
        let called: Vec<&SkillCall> = calls.iter().filter(|c| c.skill == bare(skill)).collect();
        let didnt_load = loaded.as_ref().is_some_and(|l| !l.contains(skill));
        if didnt_load {
            misses.push(Miss::stuck(format!(
                "`{skill}` didn't load in this session, so it couldn't run."
            )));
            continue;
        }
        match report.map(|r| r.status) {
            None => misses.push(Miss::retry(format!(
                "`{skill}` is missing from `run_report`."
            ))),
            Some(SkillStatus::Ran) if !called.iter().any(|c| c.ran_on(&diff)) => {
                misses.push(Miss::retry(match called.last() {
                    None => format!("`{skill}` is reported as run, but was never called."),
                    Some(c) if !c.args.contains(&diff) => format!(
                        "`{skill}` was called without the prescribed args, so it couldn't read \
                         this run's diff."
                    ),
                    Some(_) => format!(
                        "`{skill}` was called but gave no findings back: it errored, or answered \
                         through `StructuredOutput`, which never reaches you."
                    ),
                }));
            }
            Some(SkillStatus::Failed) => misses.push(Miss::stuck(format!(
                "`{skill}` failed: {}",
                report
                    .and_then(|r| r.reason.as_deref())
                    .unwrap_or("no reason given")
            ))),
            Some(SkillStatus::Ran) => {}
        }
    }
    let any = |severities: &[Severity]| {
        answer
            .comments
            .iter()
            .any(|c| severities.contains(&c.severity))
    };
    let verdict = match answer.verdict {
        Verdict::RequestChanges if !any(&[Severity::Blocker, Severity::Major]) => {
            Some("The verdict is `request_changes`, but no comment is a `blocker` or `major`.")
        }
        Verdict::Approve if any(&[Severity::Blocker, Severity::Major]) => {
            Some("The verdict is `approve`, but a comment is a `blocker` or `major`.")
        }
        Verdict::None if any(&[Severity::Blocker]) => {
            Some("The verdict is `none`, but a comment is a `blocker`.")
        }
        _ => None,
    };
    misses.extend(verdict.map(|v| Miss::retry(v.to_owned())));
    misses
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plugin(names: &[&str]) -> BTreeMap<String, PathBuf> {
        names
            .iter()
            .map(|n| ((*n).to_owned(), PathBuf::from("/s")))
            .collect()
    }

    #[test]
    fn a_procedure_requires_the_skills_it_names_as_slash_words() {
        let text = "\
---
description: Runs `/code-review`, plus `/lakehouse-review` on lakehouse PRs. \
NOT for your own branch (see `review-yourself`).
---
| Finding | `/code-review`, `spark-reviewer` |
Fetch https://api.github.com/repos/x/pulls and read path/to/simplify.
Run /review-comments again, or /other-procedure, or /util-unknown.
Not /lakehouse_review, /simplify/x or /code-reviewX. But **/sanic:lint**, \"/simplify\".";
        let plugin = plugin(&[
            "lakehouse-review",
            "review-yourself",
            "review-comments",
            "other-procedure",
            "spark-reviewer",
            "lint",
        ]);
        let procedure = [
            ("review-comments".to_owned(), text.to_owned()),
            ("other-procedure".to_owned(), String::new()),
        ];
        assert_eq!(
            required_skills(&procedure, &plugin, None),
            [
                "code-review",
                "sanic:lakehouse-review",
                "sanic:lint",
                "simplify"
            ]
        );
        let finders = [
            "/spark-reviewer".to_owned(),
            "nope".to_owned(),
            "sanic:lint".to_owned(),
        ];
        assert_eq!(
            required_skills(&procedure, &plugin, Some(&finders)),
            ["sanic:spark-reviewer", "sanic:lint"]
        );
    }

    fn line(v: &Value) -> String {
        format!("{v}\n")
    }

    fn skill_use(id: &str, skill: &str, args: &str, parent: Option<&str>) -> String {
        line(
            &json!({"type": "assistant", "parent_tool_use_id": parent, "message": {"content": [
                {"type": "tool_use", "id": id, "name": "Skill", "input": {"skill": skill, "args": args}}
            ]}}),
        )
    }

    fn skill_result(id: &str, text: &str, is_error: bool) -> String {
        line(&json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error,
             "content": [{"type": "text", "text": text}]}
        ]}}))
    }

    fn init(skills: &[&str]) -> String {
        line(&json!({"type": "system", "subtype": "init", "skills": skills}))
    }

    fn found(skill: &str) -> String {
        format!("Skill \"{skill}\" completed (forked execution).\n\nResult:\nI found 2 issues.")
    }

    const STUB: &str =
        "Skill \"code-review\" completed (forked execution).\n\nResult:\nSkill execution completed";

    const BOTH: &[&str] = &["code-review", "sanic:lakehouse-review"];

    fn said(skill: &str, status: SkillStatus) -> SkillReport {
        SkillReport {
            skill: skill.into(),
            status,
            reason: Some("why".into()),
            findings_count: None,
        }
    }

    fn check(transcripts: &[&str], report: &[SkillReport], verdict: Verdict) -> Vec<Miss> {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = transcripts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let p = tmp.path().join(format!("t{i}.jsonl"));
                std::fs::write(&p, t).unwrap();
                p
            })
            .collect();
        let answer = Answer {
            report,
            verdict,
            comments: &[],
        };
        let required: Vec<String> = BOTH.iter().map(|s| (*s).to_owned()).collect();
        misses(&paths, &answer, &required, Path::new("/runs/9/pr.diff"))
    }

    const ARGS: &str =
        "medium The PR's diff is in /runs/9/pr.diff and its code is in the working directory.";

    #[test]
    fn calls_that_ran_on_the_diff_and_answered_pass() {
        let transcript = [
            init(BOTH),
            skill_use("a", "code-review", ARGS, None),
            skill_result("a", &found("code-review"), false),
            skill_use("b", "/lakehouse-review", ARGS, None),
            skill_result("b", "Launching skill: sanic:lakehouse-review", false),
        ]
        .concat();
        let report = [
            said("code-review", SkillStatus::Ran),
            said("sanic:lakehouse-review", SkillStatus::Ran),
        ];
        assert_eq!(check(&[&transcript], &report, Verdict::Comment), []);
    }

    #[test]
    fn a_skipped_misreported_or_swallowed_skill_is_a_miss_to_retry() {
        let skipped = init(BOTH);
        let wrong_args = [
            init(BOTH),
            skill_use("a", "code-review", "medium 36096ab..HEAD", None),
            skill_result("a", &found("code-review"), false),
        ]
        .concat();
        let swallowed = [
            init(BOTH),
            skill_use("a", "code-review", ARGS, None),
            line(
                &json!({"type": "assistant", "parent_tool_use_id": "a", "message": {"content": [
                    {"type": "tool_use", "id": "c", "name": "StructuredOutput", "input": {}}
                ]}}),
            ),
            skill_result("a", STUB, false),
        ]
        .concat();
        let errored = [
            init(BOTH),
            skill_use("a", "code-review", ARGS, None),
            skill_result("a", "boom", true),
        ]
        .concat();
        let ran = [said("code-review", SkillStatus::Ran)];
        let has = |t: &str, text: &str| {
            let found = check(&[t], &ran, Verdict::Comment);
            let miss = found.iter().find(|m| m.text.contains(text));
            assert!(miss.is_some_and(|m| m.retry), "{text}: {found:?}");
        };
        has(&skipped, "never called");
        has(&wrong_args, "prescribed args");
        has(&swallowed, "no findings back");
        has(&errored, "no findings back");
        has(&skipped, "`sanic:lakehouse-review` is missing");
    }

    #[test]
    fn a_skill_that_didnt_load_or_failed_is_a_miss_not_to_retry() {
        let report = [
            said("code-review", SkillStatus::Failed),
            said("sanic:lakehouse-review", SkillStatus::Ran),
        ];
        let found = check(&[&init(&["code-review"])], &report, Verdict::Comment);
        assert_eq!(
            found,
            [
                Miss::stuck("`code-review` failed: why".into()),
                Miss::stuck(
                    "`sanic:lakehouse-review` didn't load in this session, so it couldn't run."
                        .into()
                ),
            ]
        );
    }

    #[test]
    fn a_retry_counts_calls_from_both_sessions() {
        let first = [init(BOTH), skill_use("a", "code-review", "x", None)].concat();
        let retry = [
            skill_use("b", "code-review", ARGS, None),
            skill_result("b", &found("code-review"), false),
            skill_use("c", "sanic:lakehouse-review", ARGS, None),
            skill_result("c", "Nothing in scope.", false),
        ]
        .concat();
        let report = [
            said("code-review", SkillStatus::Ran),
            said("lakehouse-review", SkillStatus::Ran),
        ];
        assert_eq!(check(&[&first, &retry], &report, Verdict::Comment), []);
    }

    #[test]
    fn the_verdict_must_fit_the_comments() {
        let comment = |severity| InlineComment {
            path: "a".into(),
            line: 1,
            start_line: None,
            side: sanic_core::run::Side::Right,
            body: "b".into(),
            severity,
            confidence: sanic_core::run::Confidence::High,
            note: None,
        };
        let cases = [
            (Verdict::RequestChanges, vec![comment(Severity::Minor)], 1),
            (Verdict::RequestChanges, vec![comment(Severity::Major)], 0),
            (Verdict::Approve, vec![comment(Severity::Major)], 1),
            (Verdict::Approve, vec![comment(Severity::Nit)], 0),
            (Verdict::None, vec![comment(Severity::Blocker)], 1),
            (Verdict::None, vec![comment(Severity::Minor)], 0),
            (Verdict::Comment, vec![comment(Severity::Blocker)], 0),
        ];
        for (verdict, comments, expected) in cases {
            let answer = Answer {
                report: &[],
                verdict,
                comments: &comments,
            };
            let found = misses(&[], &answer, &[], Path::new("/d"));
            assert_eq!(found.len(), expected, "{verdict:?}: {found:?}");
            assert!(found.iter().all(|m| m.retry));
        }
    }
}
