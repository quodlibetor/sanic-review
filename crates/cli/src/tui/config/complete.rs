//! The paths a path being typed could go on to, for the dropdown under
//! it, as a shell lists them: a `/` after each directory.

use std::path::Path;

use sanic_core::config::expand_path_in;

/// The files under what's `typed` whose names start as it ends, each as
/// `typed` would read completed to it. Relative paths resolve against
/// `base`, the config file's directory, and `~` is `home`, as the loader
/// resolves them; what's typed keeps its own form.
#[must_use]
pub fn path_matches(typed: &str, base: &Path, home: Option<&Path>) -> Vec<String> {
    let (dir, prefix) = match typed.rfind('/') {
        Some(at) => (&typed[..=at], &typed[at + 1..]),
        None if typed == "~" => return vec!["~/".into()],
        None => ("", typed),
    };
    let listed = if dir.is_empty() {
        base.to_owned()
    } else {
        match expand_path_in(dir, base, home) {
            Ok(listed) => listed,
            Err(_) => return Vec::new(),
        }
    };
    let Ok(read) = std::fs::read_dir(listed) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            // Hidden files only once a `.` is typed.
            let shown =
                name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.'));
            let slash = if entry.path().is_dir() { "/" } else { "" };
            shown.then(|| format!("{dir}{name}{slash}"))
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_list_what_they_could_go_on_to() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        for d in ["skills/review", "skills/rust", "src", ".hidden"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("notes.md"), "").unwrap();
        let matches = |typed: &str| path_matches(typed, base, Some(base));

        assert_eq!(matches("sk"), ["skills/"]);
        assert_eq!(matches("skills/r"), ["skills/review/", "skills/rust/"]);
        assert_eq!(matches("~/no"), ["~/notes.md"]);
        assert_eq!(matches("~"), ["~/"]);
        assert_eq!(matches("s"), ["skills/", "src/"]);
        assert_eq!(
            matches(""),
            ["notes.md", "skills/", "src/"],
            "no hidden ones"
        );
        assert_eq!(matches(".h"), [".hidden/"]);
        assert!(matches("zz").is_empty());
        assert!(matches("nope/x").is_empty());
        let absolute = format!("{}/sr", base.display());
        assert_eq!(matches(&absolute), [format!("{}/src/", base.display())]);
    }
}
