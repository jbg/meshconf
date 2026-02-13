use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use iroh::endpoint::Connection;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bandwidth::BandwidthEvent;
use crate::control::{self, ControlMessage, STREAM_TYPE_CONTROL};
use crate::pool::BufPool;
use crate::protocol::{self, MediaObject};
use crate::video_compositor::VideoDropCounter;

/// Central dispatcher: accepts incoming streams and datagrams, reads headers,
/// and routes payloads to the appropriate handler channel.
///
/// - QUIC datagrams: audio frames
/// - Unidirectional streams: video (one frame per stream)
/// - Bidirectional streams with tag 0x01: control messages
///
/// Each incoming uni stream is spawned as a separate task. The receiver
/// tracks the latest fully-received keyframe group_id and stops (aborts)
/// streams carrying frames from older GOPs — those frames are useless
/// because a newer keyframe has already been received.
pub async fn run_dispatcher(
    conn: Connection,
    audio_tx: mpsc::Sender<MediaObject>,
    video_tx: mpsc::Sender<MediaObject>,
    video_drops: VideoDropCounter,
    control_tx: Option<mpsc::Sender<ControlMessage>>,
    bw_tx: Option<mpsc::Sender<BandwidthEvent>>,
    cancel: CancellationToken,
) {
    // The group_id of the latest fully-received keyframe.  Streams carrying
    // frames with group_id < this value are from an old GOP and can be stopped.
    let latest_keyframe = Arc::new(AtomicU64::new(0));

    // Pools for receive-path buffers — shared across all streams for this
    // connection so buffers are reused globally.
    let video_payload_pool = BufPool::<u8>::new();
    let audio_payload_pool = BufPool::<u8>::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,

            // Audio: QUIC datagrams (unreliable, no HoL blocking).
            result = conn.read_datagram() => {
                match result {
                    Ok(data) => {
                        match protocol::decode_audio_datagram(&data, &audio_payload_pool) {
                            Ok(obj) => {
                                let _ = audio_tx.try_send(obj);
                            }
                            Err(e) => {
                                tracing::trace!("Failed to decode audio datagram: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        // Connection closed — stop the dispatcher.
                        if !cancel.is_cancelled() {
                            tracing::warn!("Dispatcher: connection closed (read_datagram: {})", e);
                        }
                        break;
                    }
                }
            }

            // Video: one uni stream per frame.
            result = conn.accept_uni() => {
                match result {
                    Ok(recv) => {
                        let vtx = video_tx.clone();
                        let vd = video_drops.clone();
                        let sc = cancel.clone();
                        let lk = latest_keyframe.clone();
                        let btx = bw_tx.clone();
                        let vpool = video_payload_pool.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_video_stream(recv, vtx, vd, lk, btx, vpool, sc).await {
                                tracing::trace!("Video stream read error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        // Connection closed — stop the dispatcher.
                        if !cancel.is_cancelled() {
                            tracing::warn!("Dispatcher: connection closed (accept_uni: {})", e);
                        }
                        break;
                    }
                }
            }

            // Control: bidi streams.  Errors here are non-fatal (the
            // connection may simply not have any more bidi streams).
            result = conn.accept_bi() => {
                match result {
                    Ok((send, mut recv)) => {
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
                        // Connection closed — stop the dispatcher.
                        if !cancel.is_cancelled() {
                            tracing::warn!("Dispatcher: connection closed (accept_bi: {})", e);
                        }
                        break;
                    }
                }
            }
        }
    }
}

/// QUIC error code used when stopping a stream we no longer need.
const STOP_OLD_FRAME: u32 = 0x10;



/// Read one video frame from a uni stream. Stops the stream early if a
/// newer keyframe has already been received (the data is useless).
async fn handle_video_stream(
    mut recv: iroh::endpoint::RecvStream,
    video_tx: mpsc::Sender<MediaObject>,
    video_drops: VideoDropCounter,
    latest_keyframe: Arc<AtomicU64>,
    _bw_tx: Option<mpsc::Sender<BandwidthEvent>>,
    payload_pool: BufPool<u8>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    if cancel.is_cancelled() {
        return Ok(());
    }

    let header = protocol::read_subgroup_header(&mut recv).await?;
    let group_id = header.group_id;
    let subgroup_id = header.subgroup_id;

    // Check if this frame is from an old GOP — if a newer keyframe has
    // already been fully received, this data is useless.  Tell the sender
    // to stop retransmitting by stopping the stream.
    let latest_kf = latest_keyframe.load(Ordering::Relaxed);
    if group_id < latest_kf {
        let _ = recv.stop(iroh::endpoint::VarInt::from_u32(STOP_OLD_FRAME));
        video_drops.increment();
        tracing::trace!(
            "Stopped old video stream: group={} (latest keyframe={})",
            group_id, latest_kf,
        );
        return Ok(());
    }

    // Read the single object on this stream.
    let object_data = match protocol::read_object(&mut recv, &payload_pool).await? {
        Some(od) => od,
        None => return Ok(()),
    };

    // If this is a keyframe, update the latest-received marker so future
    // old streams get stopped immediately.
    if subgroup_id == 0 {
        latest_keyframe.fetch_max(group_id, Ordering::Relaxed);
    }

    let obj = MediaObject {
        group_id,
        subgroup_id,
        object_id: object_data.object_id,
        capture_timestamp_us: object_data.capture_timestamp_us,
        codec: object_data.codec,
        payload: object_data.payload,
    };

    tracing::trace!(
        "Dispatched video: group={} subgroup={} payload={}B",
        group_id, subgroup_id, obj.payload.len(),
    );

    video_drops.increment_received();

    if video_tx.try_send(obj).is_err() {
        video_drops.increment();
    }

    Ok(())
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
