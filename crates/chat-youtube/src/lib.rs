//! YouTube live chat source.
//!
//! Two resolution paths (see `resolver`):
//! - `YouTubeTarget::Video`: public videos, API-key auth (testing path)
//! - `YouTubeTarget::OwnBroadcast`: the authenticated creator's own
//!   broadcast, including unlisted/members-only (OAuth, the product path)
//!
//! After resolution, a gRPC `streamList` connection runs in a reconnect
//! loop: the stream is known to drop (EOF after ~10s, see Google issue
//! tracker), so we resume via `page_token` with exponential backoff.

pub use crate::resolver::Auth;
use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use chat_core::{ChatMessage, ChatSource};
use tokio::sync::mpsc;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

mod convert;
mod pb;
mod resolver;

use convert::BoundedIdSet;
use pb::v3_data_live_chat_message_service_client::V3DataLiveChatMessageServiceClient;
use pb::LiveChatMessageListRequest;
use resolver::ResolvedStream;

const ENDPOINT: &str = "https://youtube.googleapis.com:443";

pub enum YouTubeTarget {
    /// Watch a specific video (public path, API key auth).
    Video(String),
    /// Watch the authenticated creator's own current/next broadcast
    /// (OAuth path — sees unlisted and members-only streams).
    OwnBroadcast,
}

/// Why a streamList connection ended.
enum StreamEnd {
    /// Server marked the broadcast offline (`offline_at` set): truly over.
    Offline,
    /// Connection ended without an offline marker (known ~10s EOF bug,
    /// network blips): resume with page_token.
    Eof,
    /// Downstream receiver is gone: the server is shutting down.
    ReceiverGone,
}

pub struct YouTubeSource {
    pub target: YouTubeTarget,
    pub auth: Auth,
}

impl ChatSource for YouTubeSource {
    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<ChatMessage>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        async move {
            // Before the resolution match, so we don't move self.target twice:
            let is_own_broadcast = matches!(self.target, YouTubeTarget::OwnBroadcast);

            // Resolution
            let stream = match self.target {
                YouTubeTarget::Video(video_id) => {
                    let live_chat_id =
                        resolver::resolve_live_chat_id(&self.auth, &video_id).await?;
                    ResolvedStream {
                        video_id,
                        live_chat_id: Some(live_chat_id),
                    }
                }
                YouTubeTarget::OwnBroadcast => resolver::resolve_own_broadcast(&self.auth).await?,
            };

            let live_chat_id = stream
                .live_chat_id
                .context("resolved stream has no active live chat yet")?;

            // --- Connection: one HTTP/2 channel, many streams over it ---
            let channel = Endpoint::from_shared(ENDPOINT.to_string())?
                .tls_config(ClientTlsConfig::new().with_webpki_roots())?
                .connect()
                .await
                .context("gRPC connect to YouTube failed")?;
            let mut client = V3DataLiveChatMessageServiceClient::new(channel);

            // --- Reconnect loop state ---
            // Resume token survives reconnects; dedupe drops replays;
            // backoff keeps us polite when the stream is down.
            let mut page_token: Option<String> = None;
            let mut dedupe = BoundedIdSet::new(2048);
            let mut backoff = Duration::from_secs(2);

            loop {
                let mut request = Request::new(LiveChatMessageListRequest {
                    live_chat_id: Some(live_chat_id.clone()),
                    hl: None,
                    profile_image_size: None,
                    max_results: Some(20),
                    page_token: page_token.clone(),
                    part: vec!["id".into(), "snippet".into(), "authorDetails".into()],
                });

                match &self.auth {
                    Auth::ApiKey(key) => {
                        let value: MetadataValue<_> = key.parse().context("invalid api key")?;
                        request.metadata_mut().insert("x-goog-api-key", value);
                    }
                    Auth::Bearer(token) => {
                        let value: MetadataValue<_> = format!("Bearer {token}")
                            .parse()
                            .context("invalid access token")?;
                        request.metadata_mut().insert("authorization", value);
                    }
                }

                match consume_stream(&mut client, request, &mut page_token, &mut dedupe, &tx).await
                {
                    Ok((n, StreamEnd::Eof)) => {
                        if n > 0 {
                            backoff = Duration::from_secs(2); // stream was healthy
                        }
                    }
                    Ok((_, StreamEnd::Offline)) => {
                        if is_own_broadcast {
                            // TODO(next): wait and re-resolve for the next broadcast.
                            eprintln!("[youtube] broadcast ended, stopping for now");
                        } else {
                            eprintln!("[youtube] stream offline, done");
                        }
                        // Normal completion: the ChatSource contract says "until the
                        // stream ends". Not an error.
                        return Ok(());
                    }
                    Ok((_, StreamEnd::ReceiverGone)) => {
                        return Ok(()); // server shutting down
                    }
                    Err(status) => {
                        if is_fatal(&status) {
                            return Err(status).context("YouTube gRPC stream failed");
                        }
                        eprintln!("[youtube] stream error, retrying: {status}");
                    }
                }

                // Stream ended (known ~10s EOF bug) or errored: reconnect.
                // Every line here = one reconnect = unknown quota cost.
                eprintln!(
                    "[youtube] reconnecting (page_token set: {})",
                    page_token.is_some()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Runs one streamList stream until it ends or errors. Returns the number
/// of messages forwarded so the caller can tell healthy streams from
/// immediately-dead ones. `page_token` is updated as responses arrive.
async fn consume_stream(
    client: &mut V3DataLiveChatMessageServiceClient<Channel>,
    request: Request<LiveChatMessageListRequest>,
    page_token: &mut Option<String>,
    dedupe: &mut BoundedIdSet,
    tx: &mpsc::Sender<ChatMessage>,
) -> Result<(usize, StreamEnd), Status> {
    let mut stream = client.stream_list(request).await?.into_inner();
    let mut count = 0;

    while let Some(response) = stream.message().await? {
        eprintln!(
            "[probe] items={} offline={:?}",
            response.items.len(),
            response.offline_at.is_some()
        );

        // The broadcast may be over, but this response can still carry
        // the last messages: record the flag, forward items, then stop.
        let offline = response.offline_at.is_some();

        if let Some(token) = response.next_page_token {
            *page_token = Some(token);
        }
        for item in response.items {
            let Some(msg) = convert::convert(item, dedupe) else {
                continue;
            };
            count += 1;
            if tx.send(msg).await.is_err() {
                return Ok((count, StreamEnd::ReceiverGone));
            }
        }

        if offline {
            return Ok((count, StreamEnd::Offline));
        }
    }
    Ok((count, StreamEnd::Eof))
}

/// Permanent failures: retrying cannot help. Transient failures (EOF bug,
/// network blips) fall through to the reconnect loop.
fn is_fatal(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::InvalidArgument
            | Code::NotFound
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::ResourceExhausted
            | Code::Unimplemented
            | Code::FailedPrecondition // live chat closed or invalid id!
    )
}
