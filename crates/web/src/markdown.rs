//! Markdown as GitHub renders it, for the comments and descriptions the
//! dashboard shows read-only: GitHub's Markdown with its tables, task
//! lists, strikethrough and autolinks, sanitised to an allowlist, since
//! anyone who can comment on a PR writes some of it.
//!
//! Code blocks that name a language are highlighted, and a `suggestion`
//! block on lines shows as the change it suggests. Images are placeholders the
//! script loads on a click, so viewing a page fetches nothing from
//! anywhere else.

use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher, RandomState},
    sync::{LazyLock, Mutex},
};

use ammonia::{Builder, UrlRelative};
use comrak::{
    Arena, Options,
    nodes::{NodeHtmlBlock, NodeValue},
};
use maud::{Markup, PreEscaped, html};
use sanic_core::{pr::Thread, run::Side};
use sanic_runner::diff::DiffIndex;
use sanic_store::DraftRow;

use crate::{guard, highlight, threads};

/// What a text is shown with: for a comment on lines, the lines a
/// `suggestion` block in it would replace. Text that isn't on lines, the
/// default, shows one as the code block it is, as GitHub does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Context {
    /// The file the comment is on, to highlight a suggestion as its code;
    /// `None` for text that isn't on lines.
    path: Option<String>,
    /// The lines a suggestion replaces, from the diff; `None` when they
    /// aren't known.
    original: Option<Vec<String>>,
}

impl Context {
    /// For `draft`, on the lines of `diff` it's anchored to.
    pub fn draft(diff: Option<&DiffIndex>, draft: &DraftRow) -> Self {
        match threads::lines(draft) {
            Some((path, side, lines)) => Self::lines(diff, path, side, lines),
            None => Self::default(),
        }
    }

    /// For a comment in `thread`, on its lines on `head` as `diff` has
    /// them; a thread on another commit's lines may not be on the same
    /// lines, so its suggestions have no lines to show against. A thread on
    /// a whole file isn't on lines.
    pub fn thread(diff: Option<&DiffIndex>, thread: &Thread, head: &str) -> Self {
        let on_head = thread.place.lines_at(thread.line, head);
        let on_lines = thread.line.or(thread.place.original_line).is_some();
        match (thread.path.as_deref(), on_head) {
            (Some(path), Some((side, first, last))) => Self::lines(diff, path, side, (first, last)),
            (Some(path), None) if on_lines => Self {
                path: Some(path.to_owned()),
                original: None,
            },
            _ => Self::default(),
        }
    }

    /// For a comment on `path`'s lines `first` to `last` of `side`. A
    /// suggestion replaces new lines, so only those have originals, and
    /// only when the diff has every one.
    pub fn lines(
        diff: Option<&DiffIndex>,
        path: &str,
        side: Side,
        (first, last): (u32, u32),
    ) -> Self {
        let original = diff.filter(|_| side == Side::Right).and_then(|diff| {
            let lines: Vec<String> = diff
                .hunks(path)
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .filter(|l| l.new.is_some_and(|n| (first..=last).contains(&n)))
                .map(|l| l.text.clone())
                .collect();
            (lines.len() == (last - first + 1) as usize).then_some(lines)
        });
        Self {
            path: Some(path.to_owned()),
            original,
        }
    }
}

/// `text` as GitHub would show it, sanitised, in a `div.md`.
pub fn render(text: &str, cx: &Context) -> Markup {
    let key = KEYS.hash_one((text, cx));
    if let Some(html) = CACHE.lock().ok().and_then(|cache| cache.get(key, text, cx)) {
        return wrap(html);
    }
    let html = rendered(text, cx);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(key, text, cx, &html);
    }
    wrap(html)
}

fn wrap(html: String) -> Markup {
    html! { div.md { (PreEscaped(html)) } }
}

/// Roughly how much memory rendered texts and their contexts are kept in.
const CACHED_BYTES: usize = 16 << 20;

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));

/// Keyed per process, so texts can't be written to collide.
static KEYS: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// Texts already rendered, by a hash of the text and its context; a hit
/// is checked against both, so a collision renders afresh.
#[derive(Default)]
struct Cache {
    entries: HashMap<u64, Rendered>,
    bytes: usize,
}

struct Rendered {
    text: String,
    cx: Context,
    html: String,
}

impl Rendered {
    /// Roughly the memory it holds, its context's lines included.
    fn bytes(&self) -> usize {
        let original = self.cx.original.iter().flatten();
        size_of::<Self>()
            + self.text.len()
            + self.html.len()
            + self.cx.path.as_ref().map_or(0, String::len)
            + original
                .map(|line| size_of::<String>() + line.len())
                .sum::<usize>()
    }
}

impl Cache {
    fn get(&self, key: u64, text: &str, cx: &Context) -> Option<String> {
        let done = self.entries.get(&key)?;
        (done.text == text && done.cx == *cx).then(|| done.html.clone())
    }

    fn insert(&mut self, key: u64, text: &str, cx: &Context, html: &str) {
        let done = Rendered {
            text: text.to_owned(),
            cx: cx.clone(),
            html: html.to_owned(),
        };
        let size = done.bytes();
        // Keeping one that fills the cache alone would only empty it.
        if size > CACHED_BYTES {
            return;
        }
        // Pages refresh often, redrawing the same texts; a full cache
        // starts over rather than tracking what's used.
        if self.bytes + size > CACHED_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        if let Some(old) = self.entries.insert(key, done) {
            self.bytes -= old.bytes();
        }
        self.bytes += size;
    }
}

/// Marks where a code block goes back in once the rest is sanitised: its
/// highlighting is ours, so it isn't sanitised. Private-use characters,
/// which nobody types; if someone does, the block they name shows again,
/// in text, which is no worse than their own text.
const SLOT: (char, char) = ('\u{E000}', '\u{E001}');

fn rendered(text: &str, cx: &Context) -> String {
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, text, &OPTIONS);
    let mut slots = Vec::new();
    for node in root.descendants() {
        let mut ast = node.data_mut();
        let block = match &mut ast.value {
            NodeValue::HtmlBlock(NodeHtmlBlock { literal, .. })
            | NodeValue::HtmlInline(literal) => {
                *literal = filter_tags(literal);
                continue;
            }
            NodeValue::CodeBlock(block) => block,
            _ => continue,
        };
        let lang = block.info.split_whitespace().next().unwrap_or_default();
        let markup = if lang == "suggestion" && cx.path.is_some() {
            suggestion(&block.literal, cx)
        } else if let Some(h) = highlight::Highlighter::for_lang(lang) {
            code(&block.literal, h)
        } else {
            continue;
        };
        let at = format!("{}{}{}\n", SLOT.0, slots.len(), SLOT.1);
        slots.push(markup.into_string());
        ast.value = NodeValue::HtmlBlock(NodeHtmlBlock {
            block_type: 0,
            literal: at,
        });
    }
    let mut html = String::new();
    // Writing to a String doesn't fail.
    if comrak::html::format_document(root, &OPTIONS, &mut html).is_err() {
        return String::new();
    }
    let clean = SANITISER.clean(&html).to_string();
    finish(&clean, &slots)
}

static OPTIONS: LazyLock<Options<'static>> = LazyLock::new(|| {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    // GitHub's comments and descriptions break a line where the text does.
    options.render.hardbreaks = true;
    // Raw HTML goes through to the sanitiser, which decides what stays.
    options.render.r#unsafe = true;
    options
});

/// GitHub's safe tags, and only the attributes they need to render.
static SANITISER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let tags = [
        "a",
        "abbr",
        "b",
        "blockquote",
        "br",
        "caption",
        "cite",
        "code",
        "dd",
        "del",
        "details",
        "dfn",
        "div",
        "dl",
        "dt",
        "em",
        "figcaption",
        "figure",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "i",
        "img",
        "input",
        "ins",
        "kbd",
        "li",
        "mark",
        "ol",
        "p",
        "pre",
        "q",
        "rp",
        "rt",
        "ruby",
        "s",
        "samp",
        "small",
        "span",
        "strike",
        "strong",
        "sub",
        "summary",
        "sup",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "tr",
        "tt",
        "ul",
        "var",
        "wbr",
    ];
    let attributes: HashMap<&str, HashSet<&str>> = [
        ("a", &["href", "title"][..]),
        ("abbr", &["title"]),
        // All the placeholder `image` makes of it, whatever it's sized to.
        ("img", &["src", "alt"]),
        ("input", &["checked"]),
        ("ol", &["start"]),
        ("p", &["align"]),
        ("div", &["align"]),
        ("td", &["align", "colspan", "rowspan"]),
        ("th", &["align", "colspan", "rowspan"]),
        ("h1", &["align"]),
        ("h2", &["align"]),
        ("h3", &["align"]),
        ("h4", &["align"]),
        ("h5", &["align"]),
        ("h6", &["align"]),
        ("details", &["open"]),
    ]
    .into_iter()
    .map(|(tag, attrs)| (tag, attrs.iter().copied().collect()))
    .collect();
    let mut builder = Builder::empty();
    builder
        .tags(tags.into_iter().collect())
        .tag_attributes(attributes)
        // Not the `title` and `lang` it otherwise allows on every tag.
        .generic_attributes(HashSet::new())
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        // A link to the dashboard's own pages would be same-origin, which
        // its confirms trust; and GitHub's relative links mean nothing
        // here.
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer nofollow"))
        .set_tag_attribute_value("a", "target", "_blank")
        .attribute_filter(|tag, attr, value| {
            let url = matches!((tag, attr), ("a", "href") | ("img", "src"));
            // An absolute URL on this machine can be the dashboard's own
            // pages too, at its port.
            if url && is_local(value) {
                return None;
            }
            if tag == "img" && attr == "src" && !is_web(value) {
                return None;
            }
            Some(value.into())
        });
    builder
});

/// Tags whose content isn't HTML, which would take the rest of the text
/// with them: GFM's "disallowed raw HTML".
const DISALLOWED: [&str; 9] = [
    "title",
    "textarea",
    "style",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "script",
    "plaintext",
];

/// Raw HTML with GFM's disallowed tags escaped, so they show as text, as
/// GitHub shows them.
fn filter_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tag = &rest[at + 1..];
        let name = tag.strip_prefix('/').unwrap_or(tag);
        let disallowed = DISALLOWED.iter().any(|d| {
            name.get(..d.len())
                .is_some_and(|n| n.eq_ignore_ascii_case(d))
                && name[d.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_ascii_whitespace() || c == '>' || c == '/')
        });
        out.push_str(if disallowed { "&lt;" } else { "<" });
        rest = tag;
    }
    out.push_str(rest);
    out
}

fn is_web(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// Whether `url` is on a host the dashboard answers to.
fn is_local(url: &str) -> bool {
    ammonia::Url::parse(url).is_ok_and(|url| url.host_str().is_some_and(guard::is_loopback))
}

/// Highlighted, unless it's too long to be worth it.
fn code(text: &str, mut h: highlight::Highlighter) -> Markup {
    let lines: Vec<&str> = text.lines().collect();
    html! {
        pre.md-code {
            code {
                @if lines.len() > highlight::MAX_LINES {
                    (text)
                } @else {
                    @for (i, line) in lines.iter().enumerate() {
                        @if i > 0 { "\n" }
                        (h.line(line))
                    }
                }
            }
        }
    }
}

/// A `suggestion` block as GitHub shows it: the lines it replaces removed and
/// its own added; or, when the lines it replaces aren't known, its lines,
/// labelled.
fn suggestion(text: &str, cx: &Context) -> Markup {
    let new: Vec<&str> = text.lines().collect();
    let lines = |lines: &[&str]| -> Vec<Markup> {
        let mut h = cx
            .path
            .as_deref()
            .and_then(highlight::Highlighter::for_path)
            .filter(|_| lines.len() <= highlight::MAX_LINES);
        lines
            .iter()
            .map(|line| {
                if let Some(h) = &mut h {
                    h.line(line)
                } else {
                    html! { (line) }
                }
            })
            .collect()
    };
    html! {
        div.md-sugg {
            div.md-sugg-h {
                "Suggested change"
                @if cx.original.is_none() {
                    " " span.dim { "· the lines it replaces aren't in this run's diff" }
                }
            }
            @if let Some(original) = &cx.original {
                @let old: Vec<&str> = original.iter().map(String::as_str).collect();
                table.diff {
                    @for line in lines(&old) {
                        tr.del { td.code { span.sign { "-" } (line) } }
                    }
                    @for line in lines(&new) {
                        tr.add { td.code { span.sign { "+" } (line) } }
                    }
                }
            } @else {
                pre.md-code { code {
                    @for (i, line) in lines(&new).into_iter().enumerate() {
                        @if i > 0 { "\n" }
                        (line)
                    }
                } }
            }
        }
    }
}

/// The sanitised HTML with each image a placeholder and each code block
/// back in its slot.
///
/// The sanitiser's output is well-formed and regular: text never has a
/// raw `<`, and every attribute's value is quoted with `"`, which never
/// appears inside one. So a `<` starts a tag and a `>` outside quotes ends
/// it, and a slot is only put back where it's in text, never in an
/// attribute a hostile text left it in.
fn finish(html: &str, slots: &[String]) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            out.push_str(&unslot(rest, slots));
            break;
        };
        out.push_str(&unslot(&rest[..open], slots));
        rest = &rest[open..];
        let mut quoted = false;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c == '>' && !quoted
            })
            .map_or(rest.len(), |(i, _)| i + 1);
        let tag = &rest[..end];
        if let Some(placeholder) = image(tag) {
            out.push_str(&placeholder.into_string());
        } else if let Some(attrs) = tag.strip_prefix("<input") {
            // Only task lists' boxes, which can't be ticked here.
            let checked = attrs.contains("checked=");
            out.push_str(&html! { input type="checkbox" disabled checked[checked]; }.into_string());
        } else {
            out.push_str(tag);
        }
        rest = &rest[end..];
    }
    out
}

/// `text` with its slots filled.
fn unslot(text: &str, slots: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(SLOT.0) {
        out.push_str(&rest[..start]);
        let after = &rest[start + SLOT.0.len_utf8()..];
        let filled = after.find(SLOT.1).and_then(|end| {
            let slot = slots.get(after[..end].parse::<usize>().ok()?)?;
            Some((slot, end + SLOT.1.len_utf8()))
        });
        if let Some((slot, len)) = filled {
            out.push_str(slot);
            rest = &after[len..];
        } else {
            out.push(SLOT.0);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// An `<img>` tag as a placeholder naming the image and where it's from,
/// which the script swaps for the image on a click; `None` for any other
/// tag. It has no `src`, so nothing loads until then.
fn image(tag: &str) -> Option<Markup> {
    let attrs = tag.strip_prefix("<img")?;
    if !attrs.starts_with([' ', '>']) {
        return None;
    }
    let mut src = None;
    let mut alt = String::new();
    let mut rest = attrs;
    while let Some(eq) = rest.find("=\"") {
        let name = rest[..eq].trim();
        let value_len = rest[eq + 2..].find('"').unwrap_or(rest.len() - eq - 2);
        let value = unescape(&rest[eq + 2..eq + 2 + value_len]);
        match name {
            "src" => src = Some(value),
            "alt" => alt = value,
            _ => {}
        }
        rest = rest.get(eq + 3 + value_len..).unwrap_or_default();
    }
    let host = src
        .as_deref()
        .and_then(|src| ammonia::Url::parse(src).ok())
        .and_then(|url| url.host_str().map(str::to_owned));
    Some(html! {
        @if let (Some(src), Some(host)) = (&src, &host) {
            span.md-img role="button" tabindex="0" data-src=(src) data-alt=(alt)
                title={ "Load the image from " (host) } {
                "🖼 " span.md-img-alt { @if alt.is_empty() { "image" } @else { (alt) } }
                " " span.md-img-host { (host) }
            }
        } @else {
            // Its URL wasn't one to load.
            span.md-img.dead { "🖼 " @if alt.is_empty() { "image" } @else { (alt) } }
        }
    })
}

/// An attribute's value as the sanitiser escaped it, as it was.
fn unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", "\u{a0}")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use sanic_core::pr::Placement;

    use super::*;

    fn md(text: &str) -> String {
        render(text, &Context::default()).into_string()
    }

    #[test]
    fn hostile_html_is_stripped() {
        let cases = [
            "<script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "<a href=\"data:text/html,<script>alert(1)</script>\">x</a>",
            "![x](data:image/png;base64,AAAA)",
            "<iframe src=\"https://evil.example\"></iframe>",
            "<form action=\"/x\"><button>go</button></form>",
            "<p style=\"position:fixed\" onclick=\"alert(1)\">x</p>",
            "<div hx-post=\"/drafts/1/status\" hx-trigger=\"load\">x</div>",
            "<a data-dialog href=\"/prs/o/n/1/review-now\">x</a>",
            "<svg onload=alert(1)><circle/></svg>",
            "<style>body{display:none}</style>",
            "<input type=\"text\" name=\"csrf\" value=\"x\">",
        ];
        for case in cases {
            let out = md(case);
            for bad in [
                "<script",
                "onerror",
                "onclick",
                "onload",
                "javascript:",
                "data:",
                "<iframe",
                "<form",
                "<button",
                "style=",
                "hx-",
                "data-dialog",
                "<svg",
                "type=\"text\"",
                "name=",
                "/review-now",
                "<img",
            ] {
                assert!(!out.contains(bad), "{case:?} rendered {out:?}");
            }
        }
        // As on GitHub, a script shows as text.
        assert!(md("<script>alert(1)</script> after").contains("&lt;script&gt;alert(1)"));
    }

    #[test]
    fn disallowed_tags_show_as_text() {
        let out = md("Fix <SCRIPT>x</script> and <textarea/>, not <scripts> or <b>b</b>.");
        assert!(out.contains("&lt;SCRIPT&gt;x&lt;/script&gt;"), "{out}");
        assert!(out.contains("&lt;textarea/&gt;"), "{out}");
        assert!(
            out.contains("<b>b</b>") && !out.contains("scripts"),
            "{out}"
        );
    }

    #[test]
    fn unclosed_and_stray_tags_stay_inside_the_block() {
        for case in [
            "<details><summary>x",
            "</div></article><b>free",
            "<table><tr><td>cell",
            "<a href=\"https://example.com\">open",
            "```rust\nlet x = 1;",
            "<pre>",
            "<!-- open comment",
        ] {
            let out = md(case);
            assert!(out.ends_with("</div>"), "{case:?} rendered {out:?}");
            for tag in ["details", "table", "a", "pre", "code", "b"] {
                assert_eq!(
                    out.matches(&format!("<{tag}>")).count()
                        + out.matches(&format!("<{tag} ")).count(),
                    out.matches(&format!("</{tag}>")).count(),
                    "{case:?} rendered {out:?}"
                );
            }
            assert_eq!(out.matches("<div").count(), out.matches("</div>").count());
            assert!(!out.contains("</article>"), "{out}");
        }
    }

    #[test]
    fn html_nested_in_markdown_is_sanitised_too() {
        let out = md(
            "- item <img src=x onerror=alert(1)>\n\n> <a href=\"javascript:x\">q</a>\n\n| a |\n|---|\n| <script>x</script> |",
        );
        assert!(
            !out.contains("onerror") && !out.contains("javascript") && !out.contains("<script")
        );
        assert!(
            out.contains("<blockquote>") && out.contains("<table>"),
            "{out}"
        );
    }

    #[test]
    fn a_slot_in_an_attribute_stays_text() {
        // A hostile text leaves a code block's slot inside an attribute.
        let out = md("<details open='\n\n```rust\nlet x = 1;\n```\n\n'>x</details>");
        assert!(out.contains("<details open=\""), "{out}");
        assert!(!out.contains("open=\"<") && !out.contains("sy-"), "{out}");
    }

    #[test]
    fn links_open_apart_and_relative_ones_are_dropped() {
        let out = md("[gh](https://github.com/x) and <https://a.example> [rel](/prs/1)");
        assert!(
            out.contains(
                "<a href=\"https://github.com/x\" target=\"_blank\" rel=\"noopener noreferrer nofollow\">gh</a>"
            ),
            "{out}"
        );
        assert!(!out.contains("/prs/1"), "{out}");
    }

    #[test]
    fn links_and_images_on_this_machine_are_dropped() {
        for case in [
            "[x](http://127.0.0.1:7117/pr/o/n/1/runs/2/preview?event=COMMENT)",
            "<a href=\"http://LOCALHOST:7117/\">x</a>",
            "[x](http://[::1]:7117/)",
            "![x](http://127.0.0.1:7117/pr/o/n/1)",
        ] {
            let out = md(case);
            assert!(
                !out.contains("href=") && !out.contains("data-src") && !out.contains("7117"),
                "{case:?} rendered {out:?}"
            );
        }
        assert!(md("[x](https://localhost.example/)").contains("href="));
    }

    #[test]
    fn lines_break_where_the_text_does() {
        let out = md("one\ntwo");
        assert!(out.contains("one<br>"), "{out}");
    }

    #[test]
    fn tags_keep_only_their_own_attributes() {
        let out = md("<p title=\"t\" lang=\"en\">x</p>");
        assert!(out.contains("<p>x</p>"), "{out}");
    }

    #[test]
    fn gfm_basics() {
        let out = md(
            "| a | b |\n|:--|--:|\n| 1 | 2 |\n\n- [x] done\n- [ ] todo\n\n~~gone~~ see www.example.com\n\n```\nplain <b>\n```",
        );
        assert!(out.contains("<table>") && out.contains("<td"), "{out}");
        assert!(
            out.contains("<input type=\"checkbox\" disabled checked> done"),
            "{out}"
        );
        assert!(
            out.contains("<input type=\"checkbox\" disabled> todo"),
            "{out}"
        );
        assert!(out.contains("<del>gone</del>"), "{out}");
        assert!(out.contains("href=\"http://www.example.com\""), "{out}");
        assert!(
            out.contains("<pre><code>plain &lt;b&gt;\n</code></pre>"),
            "{out}"
        );
    }

    #[test]
    fn fences_that_name_a_language_are_highlighted() {
        let out = md("```rust\nlet s = \"<b>\";\n```");
        assert!(out.contains("<pre class=\"md-code\"><code>"), "{out}");
        assert!(out.contains("sy-keyword"), "{out}");
        assert!(out.contains("&lt;b&gt;") && !out.contains("<b>"), "{out}");
        // Unknown languages and very long blocks are plain.
        assert!(!md("```nosuchlang\nx\n```").contains("sy-"));
        let long = format!(
            "```rust\n{}```",
            "let x = 1;\n".repeat(highlight::MAX_LINES + 1)
        );
        assert!(!md(&long).contains("sy-"));
    }

    fn diff() -> DiffIndex {
        DiffIndex::parse(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -1,3 +1,3 @@\n fn a() {\n-    old();\n+    let x = 1;\n }\n",
        )
    }

    #[test]
    fn suggestions_show_the_change_they_suggest() {
        let text = "Try:\n\n```suggestion\n    let x = 2;\n```";
        let cx = Context::lines(Some(&diff()), "src/a.rs", Side::Right, (2, 2));
        let out = render(text, &cx).into_string();
        assert!(out.contains("Suggested change"), "{out}");
        assert!(out.contains("<tr class=\"del\">"), "{out}");
        assert!(out.contains("<tr class=\"add\">"), "{out}");
        assert!(out.contains("sy-"), "highlighted as the file's code: {out}");
        // A cap on one side's lines leaves the other's highlighted.
        let long = format!(
            "```suggestion\n{}```",
            "let y = 3;\n".repeat(highlight::MAX_LINES + 1)
        );
        let out_long = render(&long, &cx).into_string();
        let del = out_long
            .split("<tr class=\"add\">")
            .next()
            .unwrap_or_default();
        assert!(del.contains("sy-"), "{del}");
        // The line it replaces, then its own.
        let (old, new) = (out.find("1</span>"), out.find("2</span>"));
        assert!(old.zip(new).is_some_and(|(old, new)| old < new), "{out}");
    }

    #[test]
    fn suggestions_without_their_lines_are_labelled_code() {
        let text = "```suggestion\nlet x = 2;\n```";
        for cx in [
            Context::lines(None, "src/a.rs", Side::Right, (2, 2)),
            // Lines the diff doesn't have, and the old side.
            Context::lines(Some(&diff()), "src/a.rs", Side::Right, (2, 9)),
            Context::lines(Some(&diff()), "src/a.rs", Side::Left, (2, 2)),
        ] {
            let out = render(text, &cx).into_string();
            assert!(out.contains("Suggested change"), "{out}");
            assert!(out.contains("aren't in this run's diff"), "{out}");
            assert!(
                !out.contains("<tr") && out.contains("<pre class=\"md-code\">"),
                "{out}"
            );
        }
    }

    #[test]
    fn a_thread_s_suggestions_are_labelled_off_its_lines_and_plain_on_a_whole_file() {
        let thread = |line, place| Thread {
            id: "t".into(),
            path: Some("src/a.rs".into()),
            line,
            resolved: false,
            place,
            comments: Vec::new(),
        };
        let head = |head: &str| Placement {
            side: Some(Side::Right),
            head: Some(head.into()),
            ..Placement::default()
        };
        let shown = |thread: &Thread| {
            let cx = Context::thread(Some(&diff()), thread, "h1");
            render("```suggestion\nlet x = 2;\n```", &cx).into_string()
        };
        let on = shown(&thread(Some(2), head("h1")));
        assert!(on.contains("<tr class=\"add\">"), "{on}");
        // On another commit's lines, current or outdated.
        let outdated = Placement {
            outdated: true,
            original_line: Some(2),
            original_commit: Some("h0".into()),
            ..head("h1")
        };
        for elsewhere in [thread(Some(2), head("h0")), thread(None, outdated)] {
            let out = shown(&elsewhere);
            assert!(out.contains("aren't in this run's diff"), "{out}");
        }
        let file = shown(&thread(None, head("h1")));
        assert!(
            !file.contains("Suggested change") && file.contains("<pre><code>let x = 2;"),
            "{file}"
        );
    }

    #[test]
    fn the_cache_is_bounded_and_checks_what_it_hits() {
        let cx = Context::lines(Some(&diff()), "src/a.rs", Side::Right, (2, 2));
        let mut cache = Cache::default();
        cache.insert(1, "a", &cx, "<p>a</p>");
        assert_eq!(cache.get(1, "a", &cx).as_deref(), Some("<p>a</p>"));
        // A collision renders afresh.
        assert_eq!(cache.get(1, "b", &cx), None);
        assert_eq!(cache.get(1, "a", &Context::default()), None);
        // One replacing another under its key counts once, context and all.
        cache.insert(1, "b", &cx, "<p>b</p>");
        let counted = cache.entries[&1].bytes();
        assert_eq!(cache.bytes, counted);
        assert!(counted > size_of::<Rendered>() + "b<p>b</p>src/a.rs".len());
        // A full cache starts over; a text that fills it alone isn't kept.
        let half = "x".repeat(CACHED_BYTES / 2);
        cache.insert(2, &half, &cx, "");
        cache.insert(3, &half, &cx, "");
        assert_eq!(cache.entries.keys().collect::<Vec<_>>(), [&3]);
        cache.insert(4, &"x".repeat(CACHED_BYTES), &cx, "");
        assert_eq!(cache.entries.keys().collect::<Vec<_>>(), [&3]);
    }

    #[test]
    fn suggestions_in_text_not_on_lines_are_plain_code() {
        let out = md("```suggestion\nlet x = 2;\n```");
        assert!(
            !out.contains("Suggested change") && !out.contains("md-code"),
            "{out}"
        );
        assert!(
            out.contains("<pre><code>let x = 2;\n</code></pre>"),
            "{out}"
        );
    }

    #[test]
    fn github_s_other_safe_tags_keep_only_their_safe_attributes() {
        let out = md(
            "<div align=\"center\" class=\"btn go\" id=\"draft-1\" style=\"color:red\" onclick=\"x()\">\
             <span class=\"k\" data-dialog title=\"t\" onmouseover=\"x()\">s</span></div>\n\n\
             <dl><dt id=\"a\">t</dt><dd style=\"x\">d</dd></dl> <ins onclick=\"x()\">i</ins> \
             <mark>m</mark> <small id=\"s\">sm</small>\n\n\
             <table><caption class=\"c\">cap</caption><tfoot><tr><td align=\"right\" colspan=\"2\" onclick=\"x()\">f</td></tr></tfoot></table>\n\n\
             <p align=\"right\" hx-get=\"/\">p</p><h2 align=\"center\" id=\"x\">h</h2>",
        );
        for kept in [
            "<div align=\"center\"><span>s</span></div>",
            "<dl><dt>t</dt><dd>d</dd></dl>",
            "<ins>i</ins>",
            "<mark>m</mark> <small>sm</small>",
            "<caption>cap</caption>",
            "<tfoot><tr><td align=\"right\" colspan=\"2\">f</td></tr></tfoot>",
            "<p align=\"right\">p</p>",
            "<h2 align=\"center\">h</h2>",
        ] {
            assert!(out.contains(kept), "{kept}: {out}");
        }
        for bad in [
            "class=\"b",
            "class=\"k",
            "class=\"c",
            "id=",
            "style=",
            "onclick",
            "onmouseover",
            "data-",
            "hx-",
            "title=",
        ] {
            let inner = out.trim_start_matches("<div class=\"md\">");
            assert!(!inner.contains(bad), "{bad}: {out}");
        }
        let out = md("<abbr title=\"t\" id=\"a\">ab</abbr>");
        assert!(out.contains("<abbr title=\"t\">ab</abbr>"), "{out}");
        // The image is still only a placeholder, whatever it's sized to.
        let out = md(
            "<img align=\"right\" width=\"9999\" height=\"1\" src=\"https://a.example/i.png\" onload=\"x()\">",
        );
        assert!(
            !out.contains("<img") && !out.contains("onload") && !out.contains("width"),
            "{out}"
        );
    }

    #[test]
    fn images_are_placeholders_with_no_src() {
        let out = md(
            "![a \"diagram\"](https://img.example.com/a.png?x=1&y=2) and <img alt=\"shot\" src=\"https://github.com/user-attachments/1\">",
        );
        assert!(!out.contains("<img"), "{out}");
        assert!(!out.contains(" src="), "{out}");
        assert!(
            out.contains("data-src=\"https://img.example.com/a.png?x=1&amp;y=2\""),
            "{out}"
        );
        assert!(out.contains("data-alt=\"a &quot;diagram&quot;\""), "{out}");
        assert!(out.contains("img.example.com</span>"), "{out}");
        assert!(out.contains("github.com</span>"), "{out}");
        // Not a web URL: named, never loadable.
        let out = md("![x](ftp://files.example/a.png)");
        assert!(!out.contains("data-src"), "{out}");
    }
}
