use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::jitter::PLC_SENTINEL_TIMESTAMP;
use crate::pool::{BufPool, SharedBuf};
use crate::protocol::MediaObject;
use crate::room::PeerId;

/// Receives dispatched Opus payloads, decodes them, and sends tagged PCM samples
/// to the audio mixer for mixing and playback.
///
/// Supports PLC (packet loss concealment): when a MediaObject with
/// `capture_timestamp_us == PLC_SENTINEL_TIMESTAMP` is received, the decoder
/// generates a concealment frame instead of decoding a real packet.
pub async fn run_audio_decoder(
    peer_id: PeerId,
    mut rx: mpsc::Receiver<MediaObject>,
    mixer_tx: mpsc::Sender<(PeerId, SharedBuf<f32>)>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
    let pcm_pool: BufPool<f32> = BufPool::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            packet = rx.recv() => {
                match packet {
                    Some(obj) => {
                        let mut buf = pcm_pool.checkout(960);
                        let len = if obj.capture_timestamp_us == PLC_SENTINEL_TIMESTAMP {
                            // PLC: pass empty slice → opus sends null ptr → concealment frame
                            decoder.decode_float(&[], &mut buf, false)?
                        } else {
                            decoder.decode_float(&obj.payload, &mut buf, false)?
                        };
                        let _ = mixer_tx.try_send((peer_id, buf.truncate_and_share(len)));
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
