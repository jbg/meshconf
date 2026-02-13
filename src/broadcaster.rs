use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iroh::endpoint::Connection;

use crate::pool::BufPool;
use crate::protocol;
use crate::room::PeerId;

/// Maximum send age for audio frames (microseconds). Frames older than this
/// at send time are dropped to save bandwidth.
const MAX_SEND_AGE_AUDIO_US: u64 = 60_000; // 60ms

/// Distributes encoded media packets to all currently connected peers.
///
/// `Clone`-able — the room adds/removes peers, the encoder sends data.
#[derive(Clone)]
pub struct Broadcaster {
    peers: Arc<Mutex<HashMap<PeerId, Connection>>>,
    /// Optional: when set, trigger a keyframe on the next peer addition (video only).
    force_keyframe: Option<Arc<AtomicBool>>,
}

impl Default for Broadcaster {
    fn default() -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            force_keyframe: None,
        }
    }
}

impl Broadcaster {
    /// Create a broadcaster (for audio — no keyframe flag).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a broadcaster that triggers keyframes when peers are added (for video).
    pub fn new_with_keyframe(force_keyframe: Arc<AtomicBool>) -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            force_keyframe: Some(force_keyframe),
        }
    }

    /// Register a peer to receive broadcast packets.
    pub fn add_peer(&self, id: PeerId, conn: Connection) {
        self.peers.lock().unwrap().insert(id, conn);
        if let Some(ref fk) = self.force_keyframe {
            fk.store(true, Ordering::Relaxed);
        }
    }

    /// Remove a peer.
    pub fn remove_peer(&self, id: &PeerId) {
        self.peers.lock().unwrap().remove(id);
    }

    /// Snapshot of current peers and their connections.
    pub fn peers(&self) -> Vec<(PeerId, Connection)> {
        let map = self.peers.lock().unwrap();
        map.iter().map(|(id, c)| (*id, c.clone())).collect()
    }

    /// Number of registered peers.
    pub fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    /// Send an audio frame as a QUIC datagram to all peers.
    /// Applies sender-side staleness check. Returns failed peer IDs.
    ///
    /// `pool` provides reusable buffers — the encoded datagram is written
    /// into a pooled buffer, converted to `Bytes` once, and cloned (refcount
    /// bump only) for each peer.
    pub fn send_audio_datagram_to_all(
        &self,
        capture_timestamp_us: u64,
        payload: &[u8],
        pool: &BufPool<u8>,
    ) -> Vec<PeerId> {
        // Sender-side staleness check
        if capture_timestamp_us > 0 {
            let now = protocol::now_us();
            let age_us = now.saturating_sub(capture_timestamp_us);
            if age_us > MAX_SEND_AGE_AUDIO_US {
                tracing::debug!("Dropping stale audio frame (age={}ms)", age_us / 1000);
                return vec![];
            }
        }

        let peers: Vec<(PeerId, Connection)> = {
            let map = self.peers.lock().unwrap();
            map.iter().map(|(id, c)| (*id, c.clone())).collect()
        };

        // Encode once, share across all peers.
        let datagram = protocol::encode_audio_datagram(capture_timestamp_us, payload, pool);

        let mut failed = Vec::new();
        for (id, conn) in peers {
            if let Err(e) = conn.send_datagram(datagram.clone()) {
                tracing::trace!("Failed to send audio datagram to {}: {}", id, e);
                failed.push(id);
            }
        }
        failed
    }
}
