use std::collections::{HashMap, VecDeque};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::room::PeerId;

/// Combines N decoded audio streams into a single mixed output for playback.
pub struct AudioMixer {
    /// Receives decoded audio from all peer decoders.
    pub rx: mpsc::Receiver<(PeerId, Vec<f32>)>,
    /// Sends mixed audio to the playback engine.
    pub playback_tx: mpsc::Sender<Vec<f32>>,
}

impl AudioMixer {
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut peer_buffers: HashMap<PeerId, VecDeque<f32>> = HashMap::new();
        let mix_size: usize = 960; // 20ms @ 48kHz mono

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = self.rx.recv() => {
                    match msg {
                        Some((peer_id, samples)) => {
                            peer_buffers.entry(peer_id)
                                .or_insert_with(|| VecDeque::with_capacity(4800))
                                .extend(samples.iter());

                            // Mix whenever any peer has enough samples
                            self.try_mix(&mut peer_buffers, mix_size).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    async fn try_mix(
        &self,
        peer_buffers: &mut HashMap<PeerId, VecDeque<f32>>,
        mix_size: usize,
    ) {
        let any_ready = peer_buffers.values().any(|b| b.len() >= mix_size);
        if !any_ready {
            return;
        }

        let mut mixed = vec![0.0f32; mix_size];

        for buf in peer_buffers.values_mut() {
            let n = buf.len().min(mix_size);
            for (i, sample) in buf.drain(..n).enumerate() {
                mixed[i] += sample;
            }
        }

        // Soft clipping to prevent distortion with many speakers
        for s in &mut mixed {
            *s = soft_clip(*s);
        }

        let _ = self.playback_tx.try_send(mixed);
    }
}

/// Smooth soft clipper. Keeps signal in [-1, 1] without hard edges.
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 1.0 {
        x
    } else {
        x.signum() * (1.0 - (-x.abs()).exp())
    }
}
