//! Scripts and styles, embedded in the binary.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

const HTMX: &str = include_str!("../assets/htmx.min.js");
const APP: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/style.css");

pub async fn asset(Path(file): Path<String>) -> Response {
    let (body, kind) = match file.as_str() {
        "htmx.min.js" => (HTMX, "text/javascript"),
        "app.js" => (APP, "text/javascript"),
        "style.css" => (STYLE, "text/css"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, kind)], body).into_response()
}
