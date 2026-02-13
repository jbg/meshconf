use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::broadcaster::Broadcaster;
use crate::pool::{BufPool, SharedBuf};
use crate::protocol;

/// Reads 960-sample f32 chunks from the capture channel, encodes them with Opus,
/// and broadcasts each encoded packet as a QUIC datagram to all connected peers.
///
/// Runs until cancelled. Does not return the rx — the encoder is app-lifetime.
pub async fn run_audio_encoder(
    mut rx: mpsc::Receiver<SharedBuf<f32>>,
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
    let datagram_pool: BufPool<u8> = BufPool::new();

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
                        broadcaster.send_audio_datagram_to_all(
                            protocol::now_us(),
                            &output[..len],
                            &datagram_pool,
                        );
                    }
                    None => break,
                }
            }
        }
    }
}
