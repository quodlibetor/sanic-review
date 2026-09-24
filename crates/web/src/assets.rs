//! Scripts, styles and the icon, embedded in the binary.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};

const HTMX: &str = include_str!("../assets/htmx.min.js");
const APP: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/style.css");
const FAVICON: &str = include_str!("../assets/favicon.svg");

pub async fn asset(Path(file): Path<String>) -> Response {
    let (body, kind) = match file.as_str() {
        "htmx.min.js" => (HTMX, "text/javascript"),
        "app.js" => (APP, "text/javascript"),
        "style.css" => (STYLE, "text/css"),
        "syntax.css" => (crate::highlight::css(), "text/css"),
        "favicon.svg" => (FAVICON, "image/svg+xml"),
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
