use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::MediaObject;
use crate::room::PeerId;

/// Receives dispatched Opus payloads, decodes them, and sends tagged PCM samples
/// to the audio mixer for mixing and playback.
pub async fn run_audio_decoder(
    peer_id: PeerId,
    mut rx: mpsc::Receiver<MediaObject>,
    mixer_tx: mpsc::Sender<(PeerId, Vec<f32>)>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
    let mut output = vec![0f32; 960];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            packet = rx.recv() => {
                match packet {
                    Some(obj) => {
                        let len = decoder.decode_float(&obj.payload, &mut output, false)?;
                        let _ = mixer_tx.try_send((peer_id, output[..len].to_vec()));
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
