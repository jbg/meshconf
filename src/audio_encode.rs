use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::broadcaster::Broadcaster;
use crate::protocol;

/// Reads 960-sample f32 chunks from the capture channel, encodes them with Opus,
/// and broadcasts each encoded packet to all connected peers.
///
/// Runs until cancelled. Does not return the rx — the encoder is app-lifetime.
pub async fn run_audio_encoder(
    mut rx: mpsc::Receiver<Vec<f32>>,
    broadcaster: Broadcaster,
    cancel: CancellationToken,
) {
    let mut encoder = match opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to create Opus encoder: {}", e);
            return;
        }
    };
    let mut output = vec![0u8; 1500];
    let mut group_id: u64 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            chunk = rx.recv() => {
                match chunk {
                    Some(samples) => {
                        let len = match encoder.encode_float(&samples, &mut output) {
                            Ok(l) => l,
                            Err(e) => {
                                tracing::warn!("Opus encode error: {}", e);
                                continue;
                            }
                        };
                        // Skip sending if no peers are connected
                        if broadcaster.peer_count() == 0 {
                            continue;
                        }
                        broadcaster.send_to_all(
                            protocol::TRACK_AUDIO,
                            group_id,
                            protocol::QUIC_PRIORITY_AUDIO,
                            protocol::PRIORITY_AUDIO,
                            &output[..len],
                        ).await;
                        group_id = group_id.wrapping_add(1);
                    }
                    None => break,
                }
            }
        }
    }
}
