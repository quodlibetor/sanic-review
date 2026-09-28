//! Scripts, styles, the icon and the emoji font, embedded in the binary.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};

const HTMX: &str = include_str!("../assets/htmx.min.js");
const APP: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/style.css");
const FAVICON: &str = include_str!("../assets/favicon.svg");
const TWEMOJI: &[u8] = include_bytes!("../assets/twemoji.woff2");

pub async fn asset(Path(file): Path<String>) -> Response {
    let (body, kind): (&[u8], _) = match file.as_str() {
        "htmx.min.js" => (HTMX.as_bytes(), "text/javascript"),
        "app.js" => (APP.as_bytes(), "text/javascript"),
        "style.css" => (STYLE.as_bytes(), "text/css"),
        "syntax.css" => (crate::highlight::css().as_bytes(), "text/css"),
        "favicon.svg" => (FAVICON.as_bytes(), "image/svg+xml"),
        "twemoji.woff2" => (TWEMOJI, "font/woff2"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}

/// Where browsers look for the icon of a response that doesn't name one,
/// such as a plain-text error. Temporary, since a browser would keep a
/// permanent redirect for whatever serves on this port next.
pub async fn favicon_ico() -> Redirect {
    Redirect::temporary("/assets/favicon.svg")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use super::*;

    /// The code points in `emoji.txt`, which the font was subset to.
    fn covered() -> BTreeSet<char> {
        include_str!("../assets/emoji.txt")
            .lines()
            .map(|line| line.split('#').next().unwrap().trim())
            .filter(|line| !line.is_empty())
            .map(|point| {
                let hex = point.strip_prefix("U+").unwrap();
                char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap()
            })
            .collect()
    }

    #[test]
    fn the_font_covers_the_emoji_the_markup_uses() {
        // Symbols below these, such as ✓, are drawn by the text fonts.
        let emoji =
            |text: &str| -> BTreeSet<char> { text.chars().filter(|&c| c >= '\u{1F000}').collect() };
        // The markup, not the tests' fixtures.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut used = emoji(APP);
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("tests.rs") {
                used.extend(emoji(&fs::read_to_string(&path).unwrap()));
            }
        }
        assert_eq!(
            used,
            covered(),
            "emoji.txt should list the markup's emoji: update it and style.css's \
             unicode-range, then run `mise run emoji-font`"
        );
    }

    #[test]
    fn the_font_is_only_for_its_emoji() {
        let range = STYLE
            .lines()
            .find_map(|line| line.trim().strip_prefix("unicode-range: "))
            .unwrap();
        let points = covered()
            .iter()
            .map(|&c| format!("U+{:X}", u32::from(c)))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(range, format!("{points};"));
    }
}
