/// Status information shown at the bottom of the terminal display.
/// `copyable` is an optional raw string rendered at column 0 so that
/// terminal text selection copies it cleanly without embedded spaces.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StatusInfo {
    pub message: String,
    /// Optional text (e.g. a ticket) printed left-aligned for easy copying.
    pub copyable: Option<String>,
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::pool::{BufPool, SharedBuf};
use crate::room::PeerId;
use crate::video_decode::RgbFrame;

/// Shared atomic counters for video frame drops and receives in the
/// receive path.  Incremented by the dispatcher, jitter buffer, and
/// decoder.  Also used by the congestion feedback reporter to compute
/// per-interval loss fractions.
#[derive(Clone)]
pub struct VideoDropCounter {
    drops: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl VideoDropCounter {
    pub fn new() -> Self {
        Self {
            drops: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Increment the drop counter by 1.
    pub fn increment(&self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the received counter by 1.
    pub fn increment_received(&self) {
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the current drop total.
    pub fn get(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Read the current received total.
    pub fn get_received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

/// Gap in terminal cells between tiles in the gallery grid.
pub const CELL_GAP_COLS: u16 = 1;
pub const CELL_GAP_ROWS: u16 = 1;

/// Whether a peer is connected directly or via the iroh relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    /// Not yet determined.
    Unknown,
    /// Direct UDP hole-punch.
    Direct,
    /// Routed through an iroh relay server.
    Relay,
    /// This is the local participant (no network path).
    Local,
}

/// Commands to the compositor from the room.
pub enum CompositorCommand {
    RemovePeer(PeerId),
    /// Set (or update) the display name for a peer.
    SetPeerName(PeerId, Arc<str>),
    /// Set (or update) the connection kind for a peer.
    SetConnectionKind(PeerId, ConnectionKind),
    /// Register a video drop counter for a peer.
    SetVideoDropCounter(PeerId, VideoDropCounter),
}

/// A gallery frame: one or more peer frames with layout info.
pub struct GalleryFrame {
    /// Per-peer frames in display order, with grid position (col, row index).
    pub tiles: SharedBuf<TileFrame>,
    /// Grid dimensions.
    pub cols: u32,
    pub rows: u32,
}

pub struct TileFrame {
    pub peer_id: PeerId,
    pub frame: RgbFrame,
    pub grid_col: u32,
    pub grid_row: u32,
    /// Display name of this peer (empty if unknown).
    pub name: Arc<str>,
    /// How we are connected to this peer.
    pub conn_kind: ConnectionKind,
    /// Cumulative count of dropped/failed video frames for this peer.
    pub video_drops: u64,
}

/// Arranges N decoded video streams in a grid layout for display.
pub struct VideoCompositor {
    /// Receives decoded frames tagged with peer ID.
    pub rx: mpsc::Receiver<(PeerId, RgbFrame)>,
    /// Receives commands (e.g., remove peer).
    pub cmd_rx: mpsc::Receiver<CompositorCommand>,
    /// Sends gallery frames to the display.
    pub display_tx: mpsc::Sender<GalleryFrame>,
}

impl VideoCompositor {
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut latest_frames: HashMap<PeerId, RgbFrame> = HashMap::new();
        let mut peer_names: HashMap<PeerId, Arc<str>> = HashMap::new();
        let mut peer_conn_kinds: HashMap<PeerId, ConnectionKind> = HashMap::new();
        let mut peer_drop_counters: HashMap<PeerId, VideoDropCounter> = HashMap::new();
        let tile_pool: BufPool<TileFrame> = BufPool::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,

                msg = self.rx.recv() => {
                    match msg {
                        Some((peer_id, frame)) => {
                            latest_frames.insert(peer_id, frame);
                            if let Some(gallery) = build_gallery(&latest_frames, &peer_names, &peer_conn_kinds, &peer_drop_counters, &tile_pool) {
                                let _ = self.display_tx.try_send(gallery);
                            }
                        }
                        None => break,
                    }
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(CompositorCommand::RemovePeer(peer_id)) => {
                            latest_frames.remove(&peer_id);
                            peer_names.remove(&peer_id);
                            peer_conn_kinds.remove(&peer_id);
                            peer_drop_counters.remove(&peer_id);
                            let gallery = build_gallery(&latest_frames, &peer_names, &peer_conn_kinds, &peer_drop_counters, &tile_pool).unwrap_or(GalleryFrame {
                                tiles: SharedBuf::from_vec(Vec::new()),
                                cols: 0,
                                rows: 0,
                            });
                            let _ = self.display_tx.try_send(gallery);
                        }
                        Some(CompositorCommand::SetPeerName(peer_id, name)) => {
                            peer_names.insert(peer_id, name);
                            if let Some(gallery) = build_gallery(&latest_frames, &peer_names, &peer_conn_kinds, &peer_drop_counters, &tile_pool) {
                                let _ = self.display_tx.try_send(gallery);
                            }
                        }
                        Some(CompositorCommand::SetConnectionKind(peer_id, kind)) => {
                            peer_conn_kinds.insert(peer_id, kind);
                            if let Some(gallery) = build_gallery(&latest_frames, &peer_names, &peer_conn_kinds, &peer_drop_counters, &tile_pool) {
                                let _ = self.display_tx.try_send(gallery);
                            }
                        }
                        Some(CompositorCommand::SetVideoDropCounter(peer_id, counter)) => {
                            peer_drop_counters.insert(peer_id, counter);
                        }
                        None => break,
                    }
                }
            }
        }
    }
}

fn build_gallery(
    frames: &HashMap<PeerId, RgbFrame>,
    names: &HashMap<PeerId, Arc<str>>,
    conn_kinds: &HashMap<PeerId, ConnectionKind>,
    drop_counters: &HashMap<PeerId, VideoDropCounter>,
    tile_pool: &BufPool<TileFrame>,
) -> Option<GalleryFrame> {
    if frames.is_empty() {
        return None;
    }

    let n = frames.len();
    let cols = (n as f64).sqrt().ceil() as u32;
    let rows = ((n as f64) / cols as f64).ceil() as u32;

    let mut peers: Vec<_> = frames.iter().collect();
    peers.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));

    let mut tiles = tile_pool.checkout_empty();
    for (i, (peer_id, frame)) in peers.into_iter().enumerate() {
        tiles.push(TileFrame {
            peer_id: *peer_id,
            frame: RgbFrame {
                group_id: frame.group_id,
                capture_timestamp_us: frame.capture_timestamp_us,
                width: frame.width,
                height: frame.height,
                data: frame.data.clone(), // SharedBuf: refcount bump, no data copy
            },
            grid_col: (i as u32) % cols,
            grid_row: (i as u32) / cols,
            name: names.get(peer_id).cloned().unwrap_or_else(|| Arc::from("")),
            conn_kind: conn_kinds.get(peer_id).copied().unwrap_or(ConnectionKind::Unknown),
            video_drops: drop_counters.get(peer_id).map(|c| c.get()).unwrap_or(0),
        });
    }

    Some(GalleryFrame { tiles: tiles.share(), cols, rows })
}
