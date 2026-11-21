//! Local live preview for overlay themes (`/preview`, `/preview/frame`,
//! `/preview/events`).
//!
//! Renders built-in sample messages using the theme's real MiniJinja template
//! and CSS inside a backdrop-switchable preview page, and streams live
//! `event: style` updates as options change in the Themes window.

use std::convert::Infallible;
use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use chat_core::ChatEvent;
use chat_core::demo::sample_messages;
use chat_render::options::lookup_theme_values;
use futures_util::{StreamExt, stream};
use tokio_stream::wrappers::WatchStream;

use crate::{
    NO_CACHE, NO_SNIFF, OVERLAY_CSP, OVERLAY_HTML, ServerState, ThemeChoice, script_hash, to_sse,
    url_segment,
};

const PREVIEW_HTML: &str = include_str!("preview.html");

static PREVIEW_CSP: LazyLock<String> = LazyLock::new(|| {
    let script = PREVIEW_HTML
        .split_once("<script>")
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(script, _)| script)
        .expect("preview.html has its script inline");
    let hash = script_hash(script);
    format!(
        "default-src 'none'; script-src 'sha256-{hash}'; \
         style-src 'self' 'unsafe-inline'; frame-src 'self'; \
         base-uri 'none'; form-action 'none'"
    )
});

#[derive(Default, serde::Deserialize)]
pub(crate) struct PreviewQuery {
    theme: Option<String>,
    demo: Option<String>,
}

/// Escapes a string for insertion inside a double-quoted HTML attribute.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `/preview`: outer shell with backdrop, width, direction, and live-drip
/// controls framing `/preview/frame`.
pub(crate) async fn page(
    State(state): State<ServerState>,
    Query(query): Query<PreviewQuery>,
) -> impl IntoResponse {
    let choice = state.choose_theme(query.theme.as_deref()).await;
    let theme_name = match choice {
        ThemeChoice::Pinned { name, .. } => name,
        ThemeChoice::App => String::new(),
    };
    let newest = state.overlay.borrow().newest.as_str();
    let html = PREVIEW_HTML
        .replacen(
            r#"data-theme="""#,
            &format!(r#"data-theme="{}""#, escape_attr(&theme_name)),
            1,
        )
        .replacen(
            r#"data-newest="bottom""#,
            &format!(r#"data-newest="{newest}""#),
            1,
        );
    (
        [
            NO_CACHE,
            NO_SNIFF,
            (header::CONTENT_SECURITY_POLICY, PREVIEW_CSP.as_str()),
        ],
        Html(html),
    )
}

/// `/preview/frame`: the real `overlay.html` served under `/preview/`, so its
/// relative `EventSource("events" + location.search)` connects to
/// `/preview/events` while keeping the exact same inline script & CSP hash.
pub(crate) async fn frame(
    State(state): State<ServerState>,
    Query(query): Query<PreviewQuery>,
) -> impl IntoResponse {
    let choice = state.choose_theme(query.theme.as_deref()).await;
    let source = state.source_for(&choice);
    let manifest = state.load_manifest(source.clone()).await;
    let (newest, resolved) = {
        let overlay = state.overlay.borrow();
        let values = lookup_theme_values(&overlay.theme_options, &source.key());
        (overlay.newest.as_str(), manifest.resolve(values))
    };

    let mut page = OVERLAY_HTML.replacen(
        r#"data-newest="bottom""#,
        &format!(r#"data-newest="{newest}""#),
        1,
    );
    if !resolved.classes.is_empty() {
        let joined = resolved.classes.join(" ");
        page = page.replacen(
            r#"<main class="chat""#,
            &format!(r#"<main class="chat {joined}""#),
            1,
        );
        page = page.replacen(
            r#"data-style-classes="""#,
            &format!(r#"data-style-classes="{joined}""#),
            1,
        );
    }
    let css_href = match choice {
        ThemeChoice::Pinned { name, .. } => {
            format!(r#"href="../themes/{}/overlay.css""#, url_segment(&name))
        }
        ThemeChoice::App => r#"href="../theme/overlay.css""#.to_string(),
    };
    page = page.replacen(r#"href="theme/overlay.css""#, &css_href, 1);

    (
        [
            NO_CACHE,
            NO_SNIFF,
            (header::CONTENT_SECURITY_POLICY, OVERLAY_CSP.as_str()),
        ],
        Html(page),
    )
}

/// `/preview/events`: SSE feed of built-in sample messages plus live
/// `event: style` and `event: reload` updates. Does not count as a connected
/// OBS overlay.
pub(crate) async fn events(
    State(state): State<ServerState>,
    Query(query): Query<PreviewQuery>,
) -> Response {
    let choice = state.choose_theme(query.theme.as_deref()).await;
    let source = state.source_for(&choice);
    let style_key = source.key();
    let theme = state.load_theme(source.clone()).await;
    let manifest = state.load_manifest(source).await;
    let avatars = state.avatars.clone();
    let demo_drip = query.demo.as_deref() == Some("1");

    // Pick a balanced showcase slice (chat, YouTube avatar, emote-only,
    // Super Chat, bits, 12-month sub, gifted subs, raid notice).
    let showcase = |round: usize| {
        sample_messages(round)
            .into_iter()
            .enumerate()
            .filter(|(i, _)| matches!(i, 0 | 1 | 2 | 3 | 4 | 6 | 8 | 10))
            .map(|(_, msg)| ChatEvent::Message(msg))
            .collect::<Vec<_>>()
    };

    let sample_stream = if demo_drip {
        let initial = stream::iter(showcase(0).into_iter().take(2));
        let dripping = stream::unfold((0usize, 2usize), move |(mut round, mut idx)| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let mut batch = showcase(round);
            if idx >= batch.len() {
                round += 1;
                idx = 0;
                batch = showcase(round);
            }
            let event = batch.swap_remove(idx);
            Some((event, (round, idx + 1)))
        });
        initial.chain(dripping).boxed()
    } else {
        stream::iter(showcase(0)).boxed()
    };

    let chat_events =
        sample_stream.filter_map(move |event| std::future::ready(to_sse(&theme, &avatars, &event)));

    let mut reload_rx = state.reload.clone();
    reload_rx.borrow_and_update();
    let reload = WatchStream::from_changes(reload_rx)
        .map(|_| Event::default().event("reload").data("theme changed"));

    let mut style_rx = state.overlay.clone();
    let initial_style = {
        let overlay = style_rx.borrow_and_update();
        let values = lookup_theme_values(&overlay.theme_options, &style_key);
        manifest.resolve(values)
    };
    let style_events = WatchStream::from_changes(style_rx).filter_map({
        let mut last = initial_style;
        move |overlay| {
            let values = lookup_theme_values(&overlay.theme_options, &style_key);
            let next = manifest.resolve(values);
            let event = if next != last {
                last = next.clone();
                serde_json::to_string(&next)
                    .ok()
                    .map(|json| Event::default().event("style").data(json))
            } else {
                None
            };
            std::future::ready(event)
        }
    });

    let stream = stream::select(stream::select(chat_events, reload), style_events)
        .map(Ok::<_, Infallible>)
        .take_until(state.shutdown.cancelled_owned());

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use chat_render::options::{ThemeValue, ThemeValues};
    use tokio::time::timeout;

    use crate::tests::{WAIT, start};

    async fn read_until(resp: &mut reqwest::Response, seen: &mut String, needle: &str) {
        timeout(WAIT, async {
            while !seen.contains(needle) {
                let chunk = resp.chunk().await.unwrap().expect("stream ended early");
                seen.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got:\n{seen}"));
    }

    #[tokio::test]
    async fn preview_shell_frame_and_live_style_work() {
        let server = start(None).await;
        let base = format!("http://{}", server.addr);

        // Outer preview shell carries matching CSP and requested theme.
        let resp = reqwest::get(format!("{base}/preview?theme=minimal"))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let csp = resp.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("frame-src 'self'"), "{csp}");
        let html = resp.text().await.unwrap();
        assert!(html.contains(r#"data-theme="minimal""#), "{html}");

        // Frame uses parent-relative theme stylesheet path and exact overlay script.
        let frame = reqwest::get(format!("{base}/preview/frame?theme=minimal"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            frame.contains(r#"href="../themes/minimal/overlay.css""#),
            "{frame}"
        );

        // Preview events stream sample messages and live style updates without
        // counting as a connected OBS overlay.
        let mut sse = reqwest::get(format!("{base}/preview/events?theme=minimal"))
            .await
            .unwrap();
        let mut seen = String::new();
        read_until(&mut sse, &mut seen, "event: chat").await;
        assert_eq!(
            server.connections.overlays(),
            0,
            "preview must not inflate OBS overlay connection count"
        );

        let mut opts = ThemeValues::new();
        opts.insert("font_size".into(), ThemeValue::Number(26.0));
        server.overlay.send_modify(|o| {
            o.theme_options.insert("minimal".into(), opts);
        });
        read_until(&mut sse, &mut seen, "event: style").await;
        assert!(seen.contains(r#""--chat-font-size":"26px""#), "{seen}");
    }
}
