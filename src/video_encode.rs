use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::Connection;
use rav1e::prelude::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::broadcaster::Broadcaster;
use crate::protocol;
use crate::video_capture::{Frame, ENCODE_HEIGHT, ENCODE_WIDTH};

/// A persistent video encode pipeline.
///
/// The rav1e encode thread runs for the lifetime of the capture source.
/// Use `send_loop` to broadcast to all peers via a `Broadcaster`,
/// or `send_session` for single-connection backward compatibility.
pub struct VideoEncoder {
    /// Encoded AV1 packets produced by the encode thread.
    packet_rx: mpsc::Receiver<Vec<u8>>,
    /// Signal the encode thread to emit a keyframe on the next frame.
    force_keyframe: Arc<AtomicBool>,
    /// Keep the handle alive so the thread isn't detached.
    _encode_handle: tokio::task::JoinHandle<()>,
}

impl VideoEncoder {
    /// Spawn the encode thread. Consumes the capture receiver permanently.
    pub fn start(
        rx: mpsc::Receiver<Frame>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(4);
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let fk = force_keyframe.clone();

        let encode_handle = tokio::task::spawn_blocking(move || {
            encode_loop(rx, packet_tx, fk, cancel);
        });

        Ok(Self {
            packet_rx,
            force_keyframe,
            _encode_handle: encode_handle,
        })
    }

    /// Get a clone of the force_keyframe flag (for the Broadcaster to trigger keyframes).
    pub fn force_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.force_keyframe.clone()
    }

    /// Broadcast encoded packets to all peers via a Broadcaster. Runs until cancelled.
    pub async fn send_loop(
        mut self,
        broadcaster: Broadcaster,
        cancel: CancellationToken,
    ) {
        self.force_keyframe.store(true, Ordering::Relaxed);

        let mut group_id: u64 = 0;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                packet = self.packet_rx.recv() => {
                    match packet {
                        Some(data) => {
                            if broadcaster.peer_count() == 0 {
                                continue;
                            }
                            broadcaster.send_to_all(
                                protocol::TRACK_VIDEO,
                                group_id,
                                protocol::QUIC_PRIORITY_VIDEO,
                                protocol::PRIORITY_VIDEO,
                                &data,
                            ).await;
                            group_id = group_id.wrapping_add(1);
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Send encoded packets over a single connection until the session
    /// is cancelled or the connection fails. Returns self so it can be
    /// reused with a new connection. (Legacy single-peer API.)
    pub async fn send_session(
        mut self,
        conn: Connection,
        cancel: CancellationToken,
    ) -> Self {
        self.force_keyframe.store(true, Ordering::Relaxed);

        let mut group_id: u64 = 0;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                packet = self.packet_rx.recv() => {
                    match packet {
                        Some(data) => {
                            if let Err(e) = protocol::send_object(
                                &conn,
                                protocol::TRACK_VIDEO,
                                group_id,
                                protocol::QUIC_PRIORITY_VIDEO,
                                protocol::PRIORITY_VIDEO,
                                &data,
                            ).await {
                                tracing::warn!("Failed to send video object: {}", e);
                                break;
                            }
                            group_id = group_id.wrapping_add(1);
                        }
                        None => break,
                    }
                }
            }
        }
        self
    }
}

fn encode_loop(
    mut rx: mpsc::Receiver<Frame>,
    packet_tx: mpsc::Sender<Vec<u8>>,
    force_keyframe: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let enc_config = EncoderConfig {
        width: ENCODE_WIDTH as usize,
        height: ENCODE_HEIGHT as usize,
        speed_settings: SpeedSettings::from_preset(10),
        low_latency: true,
        min_key_frame_interval: 150,
        max_key_frame_interval: 150,
        bitrate: 800,
        time_base: Rational::new(1, 15),
        chroma_sampling: ChromaSampling::Cs420,
        pixel_range: PixelRange::Limited,
        ..Default::default()
    };

    let cfg = Config::new().with_encoder_config(enc_config);
    let mut ctx: Context<u8> = match cfg.new_context() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to create rav1e context: {}", e);
            return;
        }
    };

    let mut encode_latency_sum: f64 = 0.0;
    let mut encode_frame_count: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let frame_data = match rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        let capture_to_encode_ms = frame_data.captured_at.elapsed().as_secs_f64() * 1000.0;
        encode_latency_sum += capture_to_encode_ms;
        encode_frame_count += 1;
        if encode_frame_count.is_multiple_of(30) {
            tracing::info!(
                "Capture→encode latency (avg over 30): {:.1}ms",
                encode_latency_sum / 30.0,
            );
            encode_latency_sum = 0.0;
        }

        let mut frame = ctx.new_frame();
        {
            let planes = &mut frame.planes;
            let w = frame_data.width as usize;
            let h = frame_data.height as usize;

            let y_stride = planes[0].cfg.stride;
            let y_xo = planes[0].cfg.xorigin;
            let y_yo = planes[0].cfg.yorigin;
            for row in 0..h {
                let src = row * w;
                let dst = (row + y_yo) * y_stride + y_xo;
                planes[0].data[dst..dst + w].copy_from_slice(&frame_data.y[src..src + w]);
            }
            let u_w = w / 2;
            let u_h = h / 2;
            let u_stride = planes[1].cfg.stride;
            let u_xo = planes[1].cfg.xorigin;
            let u_yo = planes[1].cfg.yorigin;
            for row in 0..u_h {
                let src = row * u_w;
                let dst = (row + u_yo) * u_stride + u_xo;
                planes[1].data[dst..dst + u_w].copy_from_slice(&frame_data.u[src..src + u_w]);
            }
            let v_stride = planes[2].cfg.stride;
            let v_xo = planes[2].cfg.xorigin;
            let v_yo = planes[2].cfg.yorigin;
            for row in 0..u_h {
                let src = row * u_w;
                let dst = (row + v_yo) * v_stride + v_xo;
                planes[2].data[dst..dst + u_w].copy_from_slice(&frame_data.v[src..src + u_w]);
            }
        }

        let result = if force_keyframe.swap(false, Ordering::Relaxed) {
            let params = FrameParameters {
                frame_type_override: FrameTypeOverride::Key,
                ..Default::default()
            };
            ctx.send_frame((frame, params))
        } else {
            ctx.send_frame(frame)
        };

        if let Err(e) = result {
            tracing::warn!("rav1e send_frame error: {}", e);
            continue;
        }

        loop {
            match ctx.receive_packet() {
                Ok(packet) => {
                    match packet_tx.try_send(packet.data) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::debug!("Encode→network channel full, dropping packet");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return;
                        }
                    }
                }
                Err(EncoderStatus::Encoded | EncoderStatus::NeedMoreData) => break,
                Err(EncoderStatus::LimitReached) => break,
                Err(e) => {
                    tracing::warn!("rav1e receive_packet error: {:?}", e);
                    break;
                }
            }
        }
    }
}
