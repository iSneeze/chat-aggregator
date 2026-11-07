//! The Warudo output: sends `{"action":"<name>"}` to Warudo's WebSocket,
//! which fires the blueprint's "On WebSocket Action" nodes of that name
//! (see WARUDO.md).
//!
//! One task owns the connection: it connects, reconnects when Warudo goes
//! away (restarted, not started yet), and meanwhile keeps or drops what the
//! reactor sends it (`queue_secs`). Nothing else touches the socket, so
//! there's nothing to lock.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info};

use crate::WarudoSettings;
use crate::output::{CONNECT_TIMEOUT, Outcome, Waiting, pause, taking_jobs};
use crate::reactor::OutputState;

/// A fired rule's Warudo action name.
pub(crate) type Job = crate::output::Job<String>;

/// The message Warudo expects. Built with serde_json, not by gluing strings
/// together, so a name with quotes can't break the JSON.
fn payload(job: &Job) -> String {
    serde_json::json!({ "action": job.action }).to_string()
}

/// Runs until `jobs` closes (the reactor no longer needs Warudo). `wake`
/// cuts a pause between connection attempts short.
pub(crate) async fn run(
    settings: WarudoSettings,
    mut jobs: mpsc::Receiver<Job>,
    state: watch::Sender<OutputState>,
    wake: Arc<Notify>,
) {
    let url = settings.url.as_str();
    let mut waiting = Waiting::new("Warudo", settings.queue_secs);
    // A job whose sending failed: the connection was dead without us
    // knowing. It gets one more try on a fresh connection, whatever the
    // queue setting: the output wasn't "down" when it fired.
    let mut retry: Option<Job> = None;
    let mut failures = 0;
    loop {
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url));
        let Some(connected) = taking_jobs(connect, &mut jobs, &mut waiting).await else {
            return;
        };
        let socket = match connected {
            Ok(Ok((socket, _response))) => socket,
            failed => {
                let error = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => "no answer".to_string(),
                };
                // Logged once per outage, not every few seconds while
                // Warudo isn't running.
                if failures == 0 {
                    info!("can't reach Warudo at {url}: {error}");
                }
                failures += 1;
                waiting.extend(retry.take());
                state.send_replace(OutputState::Retrying { error });
                let pause = pause(failures, &wake);
                if taking_jobs(pause, &mut jobs, &mut waiting).await.is_none() {
                    return;
                }
                continue;
            }
        };
        info!("connected to Warudo at {url}");
        failures = 0;
        state.send_replace(OutputState::Connected);
        match connected_loop(socket, &mut jobs, &mut waiting, retry.take()).await {
            Ended::Unneeded => return,
            Ended::Lost { unsent } => {
                // Straight back to connecting: Warudo might just close each
                // connection after a message (not known yet, see WARUDO.md).
                debug!("Warudo connection closed");
                retry = unsent;
                state.send_replace(OutputState::Connecting);
            }
        }
    }
}

enum Ended {
    /// The reactor dropped its sender: Warudo isn't needed any more.
    Unneeded,
    /// The connection is gone; `unsent` failed on its way out.
    Lost { unsent: Option<Job> },
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Sends while connected: first what waited for the connection, then
/// whatever comes in. Reads too, only to notice when Warudo closes the
/// connection, so the next reaction doesn't go into a dead socket.
async fn connected_loop(
    socket: Socket,
    jobs: &mut mpsc::Receiver<Job>,
    waiting: &mut Waiting<String>,
    retry: Option<Job>,
) -> Ended {
    // Split into a sending and a receiving half: the loop below waits on
    // the receiving one while sending on the other, and Rust allows only one
    // `&mut` to the whole socket at a time.
    let (mut send, mut receive) = socket.split();
    let backlog = retry.into_iter().chain(waiting.take_fresh(Instant::now()));
    for mut job in backlog {
        if let Err(e) = send.send(Message::text(payload(&job))).await {
            debug!("sending to Warudo failed: {e}");
            return Ended::Lost { unsent: Some(job) };
        }
        job.finish("Warudo", Outcome::Sent);
    }
    loop {
        tokio::select! {
            job = jobs.recv() => {
                let Some(mut job) = job else {
                    let _ = send.close().await;
                    return Ended::Unneeded;
                };
                if let Err(e) = send.send(Message::text(payload(&job))).await {
                    debug!("sending to Warudo failed: {e}");
                    return Ended::Lost { unsent: Some(job) };
                }
                job.finish("Warudo", Outcome::Sent);
            }
            frame = receive.next() => match frame {
                // Warudo isn't known to say anything; whatever it is, it's
                // ignored. (Pings are answered by tungstenite itself.)
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    return Ended::Lost { unsent: None };
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn job(action: &str, fired_at: Instant) -> Job {
        Job::new(action, action.to_string(), fired_at)
    }

    #[test]
    fn the_payload_is_warudos_format_and_always_valid_json() {
        let now = Instant::now();
        assert_eq!(
            payload(&job("throw_item", now)),
            r#"{"action":"throw_item"}"#
        );
        let odd = payload(&job(r#"a"b\c"#, now));
        let parsed: serde_json::Value = serde_json::from_str(&odd).unwrap();
        assert_eq!(parsed["action"], r#"a"b\c"#);
    }

    /// A stand-in Warudo: accepts connections and hands every text message
    /// it gets to the test. `close_after_each`: hangs up after every
    /// message (one way the real one might behave).
    pub(crate) fn fake_warudo(
        listener: TcpListener,
        close_after_each: bool,
    ) -> mpsc::UnboundedReceiver<String> {
        fake_warudo_counting(listener, close_after_each).0
    }

    /// `fake_warudo`, plus a receiver that gets `()` for every connection
    /// it accepts.
    fn fake_warudo_counting(
        listener: TcpListener,
        close_after_each: bool,
    ) -> (mpsc::UnboundedReceiver<String>, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (connected_tx, connected) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let _ = connected_tx.send(());
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(frame)) = ws.next().await {
                        if let Message::Text(text) = frame {
                            let _ = tx.send(text.to_string());
                            if close_after_each {
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                    }
                });
            }
        });
        (rx, connected)
    }

    pub(crate) async fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    struct Output {
        jobs: mpsc::Sender<Job>,
        state: watch::Receiver<OutputState>,
        _task: tokio::task::JoinHandle<()>,
    }

    fn start(url: &str, queue_secs: u32) -> Output {
        let (jobs, rx) = mpsc::channel(8);
        let (state_tx, state) = watch::channel(OutputState::Connecting);
        let settings = WarudoSettings {
            url: url.into(),
            queue_secs,
        };
        let task = tokio::spawn(run(settings, rx, state_tx, Arc::default()));
        Output {
            jobs,
            state,
            _task: task,
        }
    }

    async fn until(state: &mut watch::Receiver<OutputState>, f: impl Fn(&OutputState) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), state.wait_for(f))
            .await
            .expect("state not reached")
            .unwrap();
    }

    async fn received(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("nothing arrived")
            .unwrap()
    }

    #[tokio::test]
    async fn sends_actions_over_one_connection() {
        let (listener, url) = listener().await;
        let mut warudo = fake_warudo(listener, false);
        let mut out = start(&url, 0);
        until(&mut out.state, |s| *s == OutputState::Connected).await;
        for name in ["a", "b"] {
            out.jobs.send(job(name, Instant::now())).await.unwrap();
        }
        assert_eq!(received(&mut warudo).await, r#"{"action":"a"}"#);
        assert_eq!(received(&mut warudo).await, r#"{"action":"b"}"#);
    }

    #[tokio::test]
    async fn keeps_working_when_warudo_hangs_up_after_each_message() {
        let (listener, url) = listener().await;
        let (mut warudo, mut connections) = fake_warudo_counting(listener, true);
        let mut out = start(&url, 0);
        for name in ["a", "b", "c"] {
            // A new connection: the output noticed the last one's end (it
            // only reconnects after that), so `Connected` below is this
            // connection's, not the closed one's.
            connections.recv().await.unwrap();
            until(&mut out.state, |s| *s == OutputState::Connected).await;
            out.jobs.send(job(name, Instant::now())).await.unwrap();
            assert_eq!(
                received(&mut warudo).await,
                format!(r#"{{"action":"{name}"}}"#)
            );
        }
    }

    #[tokio::test]
    async fn queued_reactions_arrive_once_warudo_starts() {
        // A free port with nobody listening yet: Warudo isn't started.
        let (listener, url) = listener().await;
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut out = start(&url, 30);
        until(&mut out.state, |s| {
            matches!(s, OutputState::Retrying { .. })
        })
        .await;
        out.jobs.send(job("waited", Instant::now())).await.unwrap();

        let mut warudo = fake_warudo(TcpListener::bind(addr).await.unwrap(), false);
        assert_eq!(received(&mut warudo).await, r#"{"action":"waited"}"#);
    }

    #[tokio::test]
    async fn without_a_queue_reactions_while_down_are_dropped() {
        let (listener, url) = listener().await;
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut out = start(&url, 0);
        until(&mut out.state, |s| {
            matches!(s, OutputState::Retrying { .. })
        })
        .await;
        out.jobs.send(job("dropped", Instant::now())).await.unwrap();

        let mut warudo = fake_warudo(TcpListener::bind(addr).await.unwrap(), false);
        until(&mut out.state, |s| *s == OutputState::Connected).await;
        out.jobs.send(job("live", Instant::now())).await.unwrap();
        assert_eq!(received(&mut warudo).await, r#"{"action":"live"}"#);
    }
}
