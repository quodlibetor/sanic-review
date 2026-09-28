//! Tab completion of paths as you type them, as a shell completes them:
//! as far as every match agrees, and a `/` after a lone directory.

use std::path::Path;

use sanic_core::config::expand_path_in;

/// `typed` completed against the files under it. Relative paths resolve
/// against `base`, the config file's directory, and `~` is `home`, as the
/// loader resolves them; what's typed keeps its own form.
#[must_use]
pub fn complete_path(typed: &str, base: &Path, home: Option<&Path>) -> Option<String> {
    let (dir, prefix) = match typed.rfind('/') {
        Some(at) => (&typed[..=at], &typed[at + 1..]),
        None if typed == "~" => return Some("~/".into()),
        None => ("", typed),
    };
    let listed = if dir.is_empty() {
        base.to_owned()
    } else {
        expand_path_in(dir, base, home).ok()?
    };
    let mut names: Vec<(String, bool)> = std::fs::read_dir(listed)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            // Hidden files only once a `.` is typed.
            let shown =
                name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.'));
            let is_dir = entry.path().is_dir();
            shown.then_some((name, is_dir))
        })
        .collect();
    names.sort();
    let (first, rest) = names.split_first()?;
    let mut common = first.0.clone();
    for (name, _) in rest {
        let agreed = common
            .char_indices()
            .zip(name.chars())
            .find(|((_, a), b)| a != b)
            .map_or(common.len().min(name.len()), |((at, _), _)| at);
        common.truncate(agreed);
    }
    let lone_dir = rest.is_empty() && first.1;
    let completed = format!("{dir}{common}{}", if lone_dir { "/" } else { "" });
    (completed != typed).then_some(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_complete_as_far_as_the_matches_agree() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        for d in ["skills/review", "skills/rust", "src", ".hidden"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("notes.md"), "").unwrap();
        let complete = |typed: &str| complete_path(typed, base, Some(base));

        assert_eq!(complete("sk").as_deref(), Some("skills/"));
        assert_eq!(
            complete("skills/r"),
            None,
            "review and rust agree on nothing more"
        );
        assert_eq!(complete("skills/re").as_deref(), Some("skills/review/"));
        assert_eq!(complete("~/no").as_deref(), Some("~/notes.md"));
        assert_eq!(complete("~").as_deref(), Some("~/"));
        assert_eq!(complete("s"), None, "skills and src agree on nothing more");
        assert_eq!(complete(".h").as_deref(), Some(".hidden/"));
        assert_eq!(complete("zz"), None);
        assert_eq!(complete("nope/x"), None);
        let absolute = format!("{}/sr", base.display());
        assert_eq!(
            complete(&absolute),
            Some(format!("{}/src/", base.display()))
        );
    }
}
