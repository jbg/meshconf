use iroh::endpoint::Connection;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::control::{self, ControlMessage, STREAM_TYPE_CONTROL};
use crate::protocol::{self, MediaObject};

/// Central dispatcher: accepts all incoming streams, reads headers,
/// and routes payloads to the appropriate handler channel.
///
/// - Unidirectional streams: MoQ media (audio/video)
/// - Bidirectional streams with tag 0x01: control messages
pub async fn run_dispatcher(
    conn: Connection,
    audio_tx: mpsc::Sender<MediaObject>,
    video_tx: mpsc::Sender<MediaObject>,
    control_tx: Option<mpsc::Sender<ControlMessage>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            result = conn.accept_uni() => {
                match result {
                    Ok(mut recv) => {
                        // Read subgroup header
                        let header = match protocol::read_subgroup_header(&mut recv).await {
                            Ok(h) => h,
                            Err(e) => {
                                tracing::warn!("Failed to read subgroup header: {}", e);
                                continue;
                            }
                        };

                        // Read object payload
                        let payload = match protocol::read_object(&mut recv).await {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!("Failed to read object payload: {}", e);
                                continue;
                            }
                        };

                        let obj = MediaObject {
                            group_id: header.group_id,
                            payload,
                        };

                        // Route by track alias
                        match header.track_alias {
                            protocol::TRACK_AUDIO => {
                                let _ = audio_tx.try_send(obj);
                            }
                            protocol::TRACK_VIDEO => {
                                let _ = video_tx.try_send(obj);
                            }
                            other => {
                                tracing::warn!("Unknown track_alias: {}", other);
                            }
                        }
                    }
                    Err(e) => {
                        if !cancel.is_cancelled() {
                            tracing::warn!("accept_uni error (connection closed?): {}", e);
                        }
                        break;
                    }
                }
            }

            result = conn.accept_bi() => {
                match result {
                    Ok((send, mut recv)) => {
                        // Read stream type tag
                        let mut tag = [0u8; 1];
                        if recv.read_exact(&mut tag).await.is_err() {
                            continue;
                        }

                        if tag[0] == STREAM_TYPE_CONTROL {
                            if let Some(ref ctx) = control_tx {
                                let ctx = ctx.clone();
                                let sc = cancel.clone();
                                tokio::spawn(async move {
                                    handle_control_stream(recv, send, ctx, sc).await;
                                });
                            } else {
                                tracing::warn!("Received control stream but no handler configured");
                            }
                        } else {
                            tracing::warn!("Unknown bidi stream type: 0x{:02x}", tag[0]);
                        }
                    }
                    Err(e) => {
                        if !cancel.is_cancelled() {
                            tracing::warn!("accept_bi error: {}", e);
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// Read control messages from a bidirectional stream and forward them.
async fn handle_control_stream(
    mut recv: iroh::endpoint::RecvStream,
    _send: iroh::endpoint::SendStream,
    control_tx: mpsc::Sender<ControlMessage>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = control::read_control_msg(&mut recv) => {
                match result {
                    Ok(msg) => {
                        if control_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if !cancel.is_cancelled() {
                            tracing::debug!("Control stream ended: {}", e);
                        }
                        break;
                    }
                }
            }
        }
    }
}
