//! Keeps web pages out: the server only answers requests meant for this
//! computer, and the JSON API only talks to local programs and pages.
//!
//! Listening on `127.0.0.1` keeps other *computers* out, but not web pages
//! open in the streamer's own browser, which may send requests to localhost
//! too:
//!
//! - **DNS rebinding**: a page on `evil.example` points its own domain at
//!   `127.0.0.1`. The browser then counts our server as part of that site
//!   and lets the page read every answer. The `Host` header gives it away:
//!   it still says `evil.example`. [`only_local_hosts`] refuses those.
//! - **WebSockets** aren't covered by the browser's same-origin rules at
//!   all: any page can open one to `ws://127.0.0.1`. But the browser always
//!   says which page is asking (the `Origin` header), and a page can't fake
//!   it. [`allowed_origin`] decides who may read the JSON API.
//!
//! Most chat is public, but not all of it (members-only and unlisted YouTube
//! broadcasts). And a page that can read *your* chat from *your* computer
//! learns who you are, which matters most to streamers who keep their real
//! identity private.

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::debug;

/// The origin of an OBS browser source set to "Local file": OBS serves
/// those files from this made-up address.
const OBS_LOCAL_FILE: &str = "http://absolute";

/// Middleware for every route: refuses requests whose `Host` isn't this
/// computer (see the module docs). A request without one can't come from a
/// browser (browsers always send it), so simple tools still get through.
pub(crate) async fn only_local_hosts(request: Request, next: Next) -> Response {
    let local = match request.headers().get(header::HOST) {
        // A header that isn't even text doesn't name this computer.
        Some(host) => host.to_str().is_ok_and(is_loopback_host),
        // HTTP/2 puts the host into the URL instead of a header.
        None => request
            .uri()
            .authority()
            .is_none_or(|authority| is_loopback_host(authority.as_str())),
    };
    if !local {
        // Only `debug`: any web page can cause this, and a `warn` per
        // request would let it fill the log file.
        debug!(host = ?request.headers().get(header::HOST), "refused a request for another host");
        return (
            StatusCode::FORBIDDEN,
            "chat-aggregator only answers requests for 127.0.0.1 or localhost.",
        )
            .into_response();
    }
    next.run(request).await
}

/// Whether `host` (`name` or `name:port`) is this computer: `localhost`,
/// `127.0.0.1` or `[::1]`, on any port.
fn is_loopback_host(host: &str) -> bool {
    // Cut off the port. IPv6 addresses have colons of their own, which is
    // why they're in brackets (`[::1]:7878`): there, what follows the last
    // colon isn't all digits unless it's the port.
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]"
}

/// Whether a client whose `Origin` header is `origin` may use the JSON API:
///
/// - no `Origin`: not a browser (a bot, a game engine, a script);
/// - a page served from this computer (`http://localhost:8080`, any port);
/// - an OBS browser source showing a local file.
///
/// Everything else is a page from the web. That includes `null`, which a
/// page opened straight from disk sends, but which any website can send too
/// (from a sandboxed frame), so it proves nothing.
pub(crate) fn allowed_origin(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    origin == OBS_LOCAL_FILE
        || origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .is_some_and(is_loopback_host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for host in [
            "127.0.0.1:7878",
            "127.0.0.1",
            "localhost:7878",
            "LocalHost",
            "[::1]:7878",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example:7878",
            "localhost.evil.example",
            "127.0.0.1.evil.example:7878",
            "evil.example:127.0.0.1",
            "192.168.1.10:7878",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn origins() {
        for origin in [
            None,
            Some("http://localhost:8080"),
            Some("http://127.0.0.1:7878"),
            Some("https://localhost"),
            Some("http://absolute"),
        ] {
            assert!(allowed_origin(origin), "{origin:?}");
        }
        for origin in [
            "https://evil.example",
            "null",
            "file://",
            "http://localhost.evil.example",
            "http://absolute.evil.example",
            "",
        ] {
            assert!(!allowed_origin(Some(origin)), "{origin}");
        }
    }
}
