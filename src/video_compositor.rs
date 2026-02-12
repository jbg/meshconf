use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::room::PeerId;
use crate::video_decode::RgbFrame;

/// Gap in terminal cells between tiles in the gallery grid.
pub const CELL_GAP_COLS: u16 = 1;
pub const CELL_GAP_ROWS: u16 = 1;

/// Commands to the compositor from the room.
pub enum CompositorCommand {
    RemovePeer(PeerId),
}

/// A gallery frame: one or more peer frames with layout info.
pub struct GalleryFrame {
    /// Per-peer frames in display order, with grid position (col, row index).
    pub tiles: Vec<TileFrame>,
    /// Grid dimensions.
    pub cols: u32,
    pub rows: u32,
}

pub struct TileFrame {
    pub peer_id: PeerId,
    pub frame: RgbFrame,
    pub grid_col: u32,
    pub grid_row: u32,
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

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,

                msg = self.rx.recv() => {
                    match msg {
                        Some((peer_id, frame)) => {
                            latest_frames.insert(peer_id, frame);
                            if let Some(gallery) = build_gallery(&latest_frames) {
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
                            let gallery = build_gallery(&latest_frames).unwrap_or(GalleryFrame {
                                tiles: vec![],
                                cols: 0,
                                rows: 0,
                            });
                            let _ = self.display_tx.try_send(gallery);
                        }
                        None => break,
                    }
                }
            }
        }
    }
}

fn build_gallery(frames: &HashMap<PeerId, RgbFrame>) -> Option<GalleryFrame> {
    if frames.is_empty() {
        return None;
    }

    let n = frames.len();
    let cols = (n as f64).sqrt().ceil() as u32;
    let rows = ((n as f64) / cols as f64).ceil() as u32;

    let mut peers: Vec<_> = frames.iter().collect();
    peers.sort_by_key(|(id, _)| id.as_bytes().to_vec());

    let tiles = peers
        .into_iter()
        .enumerate()
        .map(|(i, (peer_id, frame))| {
            let grid_col = (i as u32) % cols;
            let grid_row = (i as u32) / cols;
            TileFrame {
                peer_id: *peer_id,
                frame: RgbFrame {
                    group_id: frame.group_id,
                    width: frame.width,
                    height: frame.height,
                    data: frame.data.clone(),
                },
                grid_col,
                grid_row,
            }
        })
        .collect();

    Some(GalleryFrame { tiles, cols, rows })
}
