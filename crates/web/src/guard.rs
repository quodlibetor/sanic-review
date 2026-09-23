//! What keeps other sites out of the dashboard.
//!
//! Binding to `127.0.0.1` keeps other machines out, but any page open in
//! your browser can still send requests to it. So:
//!
//! - Every request's `Host` must name the loopback interface, which stops
//!   a DNS-rebinding page from reading the dashboard under its own name.
//! - A state-changing request (anything but `GET` and `HEAD`) must carry
//!   the per-process CSRF token, in the `x-csrf-token` header (htmx) or a
//!   `csrf` form field (plain forms). A cross-site page can't read it.
//!   `Sec-Fetch-Site`, when sent, must be `same-origin`, and `Origin`, when
//!   sent, the dashboard's own, or `null` alongside that `same-origin`.
//! - A page another site navigates you to (`Sec-Fetch-Site` other than
//!   `same-origin` or `none`, or without it, a `Referer` from elsewhere)
//!   is replaced by a link to itself, so the other site can't open a
//!   confirm page and catch a keystroke on it.
//! - Every response forbids framing, so a page can't overlay the Confirm
//!   button and trick you into clicking it, and its CSP allows only the
//!   dashboard's own scripts and styles.

use std::fmt::Write as _;

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use color_eyre::eyre::{Result, WrapErr};

use crate::{Shared, page};

/// The header htmx sends the token in.
pub const TOKEN_HEADER: &str = "x-csrf-token";
/// The form field plain forms send it in.
pub const TOKEN_FIELD: &str = "csrf";
/// Forms are small; a draft body is the largest thing sent.
const MAX_FORM: usize = 1 << 20;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                   img-src 'self'; connect-src 'self'; form-action 'self'; \
                   frame-ancestors 'none'; base-uri 'none'";

/// A random token, fixed for the life of the process.
pub struct Csrf(String);

impl Csrf {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|err| color_eyre::eyre::eyre!("{err}"))
            .wrap_err("generating the dashboard's CSRF token")?;
        let mut hex = String::with_capacity(64);
        for b in bytes {
            let _ = write!(hex, "{b:02x}");
        }
        Ok(Self(hex))
    }

    pub fn token(&self) -> &str {
        &self.0
    }

    /// Compares in constant time, so response timing can't reveal how much
    /// of a guess was right.
    pub fn matches(&self, candidate: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), candidate.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

pub async fn guard(State(app): State<Shared>, req: Request, next: Next) -> Response {
    let req = match check(&app.csrf, req).await {
        Ok(req) => req,
        Err(why) => return (StatusCode::FORBIDDEN, why).into_response(),
    };
    let mut resp = if navigated_from_elsewhere(&req) {
        let target = req
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned();
        page::elsewhere(&target).into_response()
    } else {
        next.run(req).await
    };
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        // Links out carry no referrer; the dashboard's own requests keep
        // theirs, so Firefox sends a real `Origin` with its form posts.
        HeaderValue::from_static("same-origin"),
    );
    resp
}

/// Passes `req` on, with its body intact, or says why it's refused.
async fn check(csrf: &Csrf, req: Request) -> Result<Request, &'static str> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .ok_or("a Host header is required")?
        .to_owned();
    if !is_loopback(&host) {
        return Err("the dashboard only answers to 127.0.0.1 and localhost");
    }
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return Ok(req);
    }
    let headers = req.headers();
    if headers
        .get_all("sec-fetch-site")
        .iter()
        .any(|site| site != "same-origin")
    {
        return Err("cross-site requests are refused");
    }
    // Only the browser sets `Sec-Fetch-Site`, and it's `same-origin` if it
    // got this far, so a page can't be claiming to be the dashboard. Firefox
    // sends `Origin: null` for the dashboard's own form posts in some cases,
    // so `null` passes when the browser vouches for the origin like this.
    let vouched = headers.contains_key("sec-fetch-site");
    let own = own_origin(&host);
    if headers.get_all(header::ORIGIN).iter().any(|origin| {
        let origin = origin.to_str().ok().map(str::to_ascii_lowercase);
        let origin = origin.as_deref();
        origin != Some(own.as_str()) && !(vouched && origin == Some("null"))
    }) {
        return Err("requests from other origins are refused");
    }
    if header_token(headers).is_some_and(|t| csrf.matches(t)) {
        return Ok(req);
    }
    // No header: the token must be a form field, so read the body and put
    // it back for the handler.
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, MAX_FORM)
        .await
        .map_err(|_| "the request body is too large")?;
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    let valid = fields
        .iter()
        .any(|(name, value)| name == TOKEN_FIELD && csrf.matches(value));
    if !valid {
        return Err("missing or wrong CSRF token; reload the dashboard and try again");
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

/// Whether another site sent you to this page, by a link, a redirect or
/// `window.open`. Such a page is shown only as a link to itself: keys
/// you're typing on the other site mustn't land on a confirm page, and
/// merely opening a PR page marks it seen. Scripts and styles are exempt;
/// they have no effect.
///
/// Browsers that send `Sec-Fetch-Site` say where a navigation came from
/// outright. Without it, a `Referer` from anywhere but the dashboard at
/// the request's `Host` counts as elsewhere. No `Referer` doesn't: a typed
/// URL or a bookmark sends none, and neither does a site that hides where
/// it links from, which is what the keyboard script's delay after a page
/// opens is for.
fn navigated_from_elsewhere(req: &Request) -> bool {
    if !matches!(*req.method(), Method::GET | Method::HEAD)
        || req.uri().path().starts_with("/assets/")
    {
        return false;
    }
    let headers = req.headers();
    let mut sites = headers.get_all("sec-fetch-site").iter().peekable();
    if sites.peek().is_some() {
        return sites.any(|site| site != "same-origin" && site != "none");
    }
    // `check` has made sure there's a loopback `Host`.
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let own = format!("{}/", own_origin(host));
    headers.get_all(header::REFERER).iter().any(|referer| {
        !referer
            .to_str()
            .is_ok_and(|r| r.to_ascii_lowercase().starts_with(&own))
    })
}

/// The dashboard's own origin when it's reached at `host`, lowercased as
/// browsers write origins; `Host` itself may not be.
fn own_origin(host: &str) -> String {
    format!("http://{}", host.to_ascii_lowercase())
}

fn header_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(TOKEN_HEADER)?.to_str().ok()
}

/// Whether `host`, a `Host` header value, names this machine's loopback
/// interface, on any port: SSH port forwarding may change the port.
fn is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(addr, _)| addr),
        None => host.split_once(':').map_or(host, |(name, _)| name),
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_hosts_pass() {
        for host in [
            "127.0.0.1:7117",
            "localhost:9000",
            "LOCALHOST",
            "[::1]:7117",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "evil.example:7117",
            "127.0.0.1.evil.example",
            "localhost.evil.example:7117",
            "[::2]:7117",
            "",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn tokens_are_random_and_compared_whole() {
        let (a, b) = (Csrf::generate().unwrap(), Csrf::generate().unwrap());
        assert_eq!(a.token().len(), 64);
        assert_ne!(a.token(), b.token());
        assert!(a.matches(a.token()));
        assert!(!a.matches(b.token()));
        assert!(!a.matches(&a.token()[..63]));
        assert!(!a.matches(""));
    }
}
