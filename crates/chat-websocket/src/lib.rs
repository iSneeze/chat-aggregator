//! What the WebSocket sources (Picarto, StreamElements, Streamlabs) have in
//! common: opening a `wss://` connection with the right TLS setup
//! ([`connect`]), and staying connected ([`keep_connected`]): connecting
//! again after a pause whenever a connection ends, saying so in the
//! source's status, and giving up only on problems the user has to fix.

use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chat_core::{Activity, Reporter, SetupError};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

/// The WebSocket library (its `Message`, its errors, building requests with
/// headers), re-exported: the sources use it through this crate, so they
/// all get the same version without listing it themselves.
pub use tokio_tungstenite::tungstenite;

/// An open connection. Read it with `StreamExt::next` and write to it with
/// `SinkExt::send` (both from futures-util).
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// A connection that lasted this long worked: the wait starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// Opens a WebSocket connection. `request` is the address (`wss://…`), or a
/// request made from one when it needs headers of its own. The error is
/// tungstenite's own, so a caller can tell a refused handshake
/// (`tungstenite::Error::Http`, with the server's answer) from a network
/// problem.
pub async fn connect(
    request: impl IntoClientRequest + Unpin,
) -> Result<Socket, tungstenite::Error> {
    let connector = Connector::Rustls(Arc::clone(&TLS));
    // `None`: tungstenite's default limits (message sizes and such).
    // `false`: Nagle's algorithm stays on (it bundles small writes), which
    // is fine for a few pings and a subscription.
    let (socket, _response) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await?;
    Ok(socket)
}

/// TLS for every connection: built when the first one is opened, then
/// shared (its cache of TLS sessions too, which lets a reconnect skip part
/// of the handshake).
///
/// The crypto backend is chosen explicitly. Otherwise tungstenite asks
/// rustls for the process-wide default, which panics when rustls was built
/// with both of its backends: `ring` (from chat-youtube's gRPC) and
/// `aws-lc-rs` (from reqwest), as in the app. We take aws-lc-rs, which
/// reqwest uses anyway. The safe defaults include TLS 1.2, which
/// sockets.streamlabs.com needs, thanks to rustls' `tls12` feature
/// (Cargo.toml).
static TLS: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        // Fails only if the backend speaks neither TLS 1.2 nor 1.3: a broken
        // build, which `the_tls_setup_builds_and_speaks_tls_1_2` catches, not
        // something that can go wrong while the app runs.
        .expect("aws-lc-rs speaks TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
});

/// Why a connection ended without an error.
#[derive(Debug)]
pub enum Ended {
    /// The server closed it: connect again after a pause.
    Closed,
    /// The server announced a restart (StreamElements does): connect again
    /// right away, to another one.
    Restarting,
    /// Nobody listens to the source's events anymore: stop.
    ReceiverGone,
}

/// Keeps a source connected: runs `connection` (one connection, from
/// connecting until it ends) again and again, until nobody listens anymore
/// (`Ok`) or it fails with a [`SetupError`]. That one is returned as it is:
/// connecting again won't help, and the engine shows it to the user.
///
/// Between connections it waits, and the source's status says why and for
/// how long: 3 s, then twice as long after each quick failure in a row (up
/// to 2 minutes), so a server that keeps refusing isn't hammered. A
/// connection that lasted a minute worked, so the wait starts over.
/// `service` names the other side, in the status ("connection closed by
/// Picarto") and in the log.
///
/// A source reconnects by itself rather than leaving it to the engine: a
/// network hiccup isn't worth a restart, which builds the source anew.
///
/// `connection` is a closure that returns a future, rather than an async
/// closure (`async || …`): with an async closure, the compiler can't yet
/// tell that the caller's future is `Send`, which `ChatSource::run`
/// promises. `FnMut`: it's called once per connection, and may change what
/// it captured.
pub async fn keep_connected<F>(
    service: &str,
    activity: &Reporter,
    mut connection: impl FnMut() -> F,
) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<Ended>>,
{
    let mut delay = RETRY_FIRST;
    loop {
        let started = Instant::now();
        let ended = connection().await;
        // However it ended: a connection that lasted means the server works.
        if started.elapsed() >= STABLE_AFTER {
            delay = RETRY_FIRST;
        }
        let problem = match ended {
            Ok(Ended::ReceiverGone) => return Ok(()),
            Ok(Ended::Restarting) => {
                info!("{service} is restarting its server; reconnecting");
                activity.set(Activity::Connecting);
                continue;
            }
            Ok(Ended::Closed) => {
                info!("{service} closed the connection");
                format!("connection closed by {service}")
            }
            // A wrong token or name stays wrong.
            Err(e) if e.is::<SetupError>() => return Err(e),
            Err(e) => {
                warn!("{service}: {e:#}");
                format!("connection problem: {e:#}")
            }
        };
        activity.set(Activity::Degraded(format!(
            "{problem}; reconnecting in {} s",
            delay.as_secs()
        )));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
        activity.set(Activity::Connecting);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use anyhow::anyhow;

    use super::*;

    /// One connection, scripted: how long it lasts, how it ends.
    type Script = (Duration, anyhow::Result<Ended>);

    fn failed() -> Script {
        (Duration::ZERO, Err(anyhow!("connection refused")))
    }

    fn closed() -> Script {
        (Duration::ZERO, Ok(Ended::Closed))
    }

    /// Runs `keep_connected` through `scripts`, then ends it
    /// (`ReceiverGone`). Returns its result and the waits between the
    /// connections, in seconds (a connection's own time included). With
    /// tokio's clock paused, the waits take no real time and come out
    /// exact.
    async fn run(scripts: Vec<Script>) -> (anyhow::Result<()>, Vec<u64>) {
        let mut scripts = VecDeque::from(scripts);
        let mut starts = Vec::new();
        let result = keep_connected("Example", &Reporter::detached(), || {
            starts.push(Instant::now());
            let (lasts, ended) = scripts
                .pop_front()
                .unwrap_or((Duration::ZERO, Ok(Ended::ReceiverGone)));
            async move {
                tokio::time::sleep(lasts).await;
                ended
            }
        })
        .await;
        let waits = starts
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs())
            .collect();
        (result, waits)
    }

    #[tokio::test(start_paused = true)]
    async fn waits_longer_after_each_quick_failure() {
        // Errors and closed connections count the same.
        let scripts = vec![
            failed(),
            closed(),
            failed(),
            closed(),
            failed(),
            closed(),
            failed(),
            closed(),
            failed(),
        ];
        let (result, waits) = run(scripts).await;
        assert!(result.is_ok(), "ends when nobody listens: {result:?}");
        assert_eq!(waits, [3, 6, 12, 24, 48, 96, 120, 120, 120]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_lasted_starts_the_wait_over() {
        let almost = (STABLE_AFTER - Duration::from_secs(1), Ok(Ended::Closed));
        let lasted = (STABLE_AFTER, Ok(Ended::Closed));
        let (_, waits) = run(vec![failed(), failed(), almost, failed(), lasted, failed()]).await;
        // A connection's own time is part of the wait after it.
        assert_eq!(waits, [3, 6, 59 + 12, 24, 60 + 3, 6]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_reconnects_right_away() {
        let restart = || (Duration::ZERO, Ok(Ended::Restarting));
        let lasted_restart = (STABLE_AFTER, Ok(Ended::Restarting));
        let scripts = vec![
            failed(),
            failed(),
            restart(),
            failed(),
            lasted_restart,
            failed(),
        ];
        let (_, waits) = run(scripts).await;
        // A quick restart keeps the wait as it was; one after a connection
        // that lasted starts it over, like any other end.
        assert_eq!(waits, [3, 6, 0, 12, 60, 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_error_ends_it() {
        let refused = (
            Duration::ZERO,
            Err(SetupError("the token was refused".into()).into()),
        );
        let (result, waits) = run(vec![failed(), refused, failed()]).await;
        let error = result.unwrap_err();
        assert!(error.is::<SetupError>(), "returned as it is: {error:#}");
        assert_eq!(waits, [3], "no connection after it");
    }

    #[tokio::test(start_paused = true)]
    async fn the_status_says_why_and_for_how_long() {
        let (activity, mut status) = Reporter::new();
        let mut ends = VecDeque::from([
            Ok(Ended::Closed),
            Err(anyhow!("no answer for 75 s")),
            Ok(Ended::ReceiverGone),
        ]);
        // In a task of its own, so this test can watch the status while it
        // runs.
        let looping = tokio::spawn(async move {
            keep_connected("Example", &activity, || {
                let ended = ends.pop_front().unwrap();
                async move { ended }
            })
            .await
        });
        for expected in [
            "connection closed by Example; reconnecting in 3 s",
            "connection problem: no answer for 75 s; reconnecting in 6 s",
        ] {
            let expected = Activity::Degraded(expected.into());
            status.wait_for(|now| *now == expected).await.unwrap();
        }
        assert!(looping.await.unwrap().is_ok());
    }

    #[test]
    fn the_tls_setup_builds_and_speaks_tls_1_2() {
        // Using `TLS` builds it: this panics if it can't be (see `TLS`).
        // sockets.streamlabs.com speaks nothing newer than TLS 1.2.
        let tls_1_2 = TLS
            .crypto_provider()
            .cipher_suites
            .iter()
            .any(|suite| suite.version().version == rustls::ProtocolVersion::TLSv1_2);
        assert!(tls_1_2, "rustls' `tls12` feature is off");
    }
}
