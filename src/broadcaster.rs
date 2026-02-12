use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iroh::endpoint::Connection;

use crate::protocol;
use crate::room::PeerId;

/// Distributes encoded media packets to all currently connected peers.
///
/// `Clone`-able — the room adds/removes peers, the encoder calls `send_to_all`.
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
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            force_keyframe: None,
        }
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

    /// Send a packet to all registered peers sequentially.
    /// Individual failures are logged. Returns failed peer IDs.
    pub async fn send_to_all(
        &self,
        track_alias: u64,
        group_id: u64,
        quic_priority: i32,
        publisher_priority: u8,
        payload: &[u8],
    ) -> Vec<PeerId> {
        let peers: Vec<(PeerId, Connection)> = {
            let map = self.peers.lock().unwrap();
            map.iter().map(|(id, c)| (*id, c.clone())).collect()
        };

        let mut failed = Vec::new();
        for (id, conn) in peers {
            if let Err(e) = protocol::send_object(
                &conn,
                track_alias,
                group_id,
                quic_priority,
                publisher_priority,
                payload,
            )
            .await
            {
                tracing::warn!("Failed to send to peer {}: {}", id, e);
                failed.push(id);
            }
        }
        failed
    }

    /// Number of registered peers.
    pub fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }
}
