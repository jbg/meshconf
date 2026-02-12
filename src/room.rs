use std::collections::HashMap;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, PublicKey};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::audio_decode;
use crate::audio_mixer::AudioMixer;
use crate::broadcaster::Broadcaster;
use crate::control::{self, ControlMessage, PeerInfo, STREAM_TYPE_CONTROL};
use crate::dispatch;
use crate::net;
use crate::video_compositor::{CompositorCommand, VideoCompositor};
use crate::video_compositor::GalleryFrame;
use crate::video_decode::{self, RgbFrame};
use crate::video_encode::VideoEncoder;

/// Peer identifier — an iroh Ed25519 public key.
pub type PeerId = PublicKey;

/// Events from per-peer tasks back to the room.
enum RoomEvent {
    PeerDisconnected(PeerId),
    /// An incoming connection from a non-host peer (for joiners accepting direct connections).
    IncomingConnection(Connection),
    /// A control message received from the host (joiner-side).
    ControlMsg(ControlMessage),
}

/// State for one remote peer.
struct PeerState {
    #[allow(dead_code)]
    conn: Connection,
    addr: EndpointAddr,
    session_cancel: CancellationToken,
    /// Send half of the control bidi stream (host→peer). Only set on host side.
    control_send: Option<iroh::endpoint::SendStream>,
    /// Per-peer task handles.
    dispatcher_handle: Option<JoinHandle<()>>,
    audio_decoder_handle: Option<JoinHandle<()>>,
    video_decoder_handle: Option<JoinHandle<()>>,
}

/// Deferred capture resources returned by `on_first_peer`.
pub struct CaptureResources {
    pub audio_capture_rx: Option<mpsc::Receiver<Vec<f32>>>,
    pub video_encoder: Option<VideoEncoder>,
}

/// Media resources passed to the Room at creation time.
pub struct MediaResources {
    /// Audio capture receiver (from mic). If None, no audio sending.
    /// Ignored if `on_first_peer` is set.
    pub audio_capture_rx: Option<mpsc::Receiver<Vec<f32>>>,
    /// Video encoder (owns capture). If None, no video sending.
    /// Ignored if `on_first_peer` is set.
    pub video_encoder: Option<VideoEncoder>,
    /// Playback channel for the audio engine. If None, no audio playback.
    pub playback_tx: Option<mpsc::Sender<Vec<f32>>>,
    /// Display channel for video. If None, no video display.
    pub display_tx: Option<mpsc::Sender<GalleryFrame>>,
    /// Called when the first peer joins to lazily start capture devices.
    /// If set, `audio_capture_rx` and `video_encoder` above are ignored.
    pub on_first_peer: Option<Box<dyn FnOnce() -> CaptureResources + Send>>,
}

/// Manages the set of peers and their connections in a call.
pub struct Room {
    endpoint: Endpoint,
    is_host: bool,
    peers: HashMap<PeerId, PeerState>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<RoomEvent>,
    event_rx: mpsc::Receiver<RoomEvent>,

    // Shared media resources
    audio_broadcaster: Broadcaster,
    video_broadcaster: Broadcaster,
    mixer_tx: mpsc::Sender<(PeerId, Vec<f32>)>,
    compositor_tx: mpsc::Sender<(PeerId, RgbFrame)>,
    compositor_cmd_tx: mpsc::Sender<CompositorCommand>,

    /// Deferred capture start — called on first peer join.
    deferred_capture: Option<Box<dyn FnOnce() -> CaptureResources + Send>>,
}

/// Determine if we should initiate the connection to the other peer.
fn should_initiate(our_id: &PublicKey, their_id: &PublicKey) -> bool {
    our_id.as_bytes() < their_id.as_bytes()
}

fn peer_id_to_string(id: &PublicKey) -> String {
    id.to_string()
}

fn addr_to_string(addr: &EndpointAddr) -> String {
    let json = serde_json::to_vec(addr).expect("Failed to serialize EndpointAddr");
    BASE64.encode(&json)
}

fn string_to_addr(s: &str) -> Result<EndpointAddr> {
    let json = BASE64.decode(s)?;
    Ok(serde_json::from_slice(&json)?)
}

impl Room {
    /// Create a room and start all long-lived media tasks.
    pub fn new(
        endpoint: Endpoint,
        is_host: bool,
        cancel: CancellationToken,
        mut media: MediaResources,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(64);

        // --- Audio mixer ---
        let (mixer_tx, mixer_rx) = mpsc::channel::<(PeerId, Vec<f32>)>(50);
        if let Some(playback_tx) = media.playback_tx.take() {
            let mixer = AudioMixer {
                rx: mixer_rx,
                playback_tx,
            };
            let mc = cancel.clone();
            tokio::spawn(async move { mixer.run(mc).await });
        } else {
            // Drop mixer_rx — decoders will fail to send, which is fine
            drop(mixer_rx);
        }

        // --- Video compositor ---
        let (compositor_tx, compositor_rx) = mpsc::channel::<(PeerId, RgbFrame)>(10);
        let (compositor_cmd_tx, compositor_cmd_rx) = mpsc::channel::<CompositorCommand>(10);
        if let Some(display_tx) = media.display_tx.take() {
            let compositor = VideoCompositor {
                rx: compositor_rx,
                cmd_rx: compositor_cmd_rx,
                display_tx,
            };
            let cc = cancel.clone();
            tokio::spawn(async move { compositor.run(cc).await });
        } else {
            drop(compositor_rx);
            drop(compositor_cmd_rx);
        }

        // --- Broadcasters (keyframe flag attached later when encoder starts) ---
        let audio_broadcaster = Broadcaster::new();
        let video_broadcaster = Broadcaster::new();

        // Start audio encoder immediately (audio engine is always running).
        if let Some(rx) = media.audio_capture_rx.take() {
            let bc = audio_broadcaster.clone();
            let ac = cancel.clone();
            tokio::spawn(async move {
                crate::audio_encode::run_audio_encoder(rx, bc, ac).await;
            });
        }

        // Start video capture immediately, or defer until first peer joins.
        let deferred_capture = media.on_first_peer.take();
        if deferred_capture.is_none()
            && let Some(encoder) = media.video_encoder.take() {
                let bc = video_broadcaster.clone();
                let vc = cancel.clone();
                tokio::spawn(async move {
                    encoder.send_loop(bc, vc).await;
                });
            }

        Self {
            endpoint,
            is_host,
            peers: HashMap::new(),
            cancel,
            event_tx,
            event_rx,
            audio_broadcaster,
            video_broadcaster,
            mixer_tx,
            compositor_tx,
            compositor_cmd_tx,
            deferred_capture,
        }
    }

    /// Start a video encoder broadcast loop.
    fn start_video_encoder(
        video_bc: &Broadcaster,
        cancel: &CancellationToken,
        encoder: VideoEncoder,
    ) {
        let bc = video_bc.clone();
        let vc = cancel.clone();
        tokio::spawn(async move {
            encoder.send_loop(bc, vc).await;
        });
    }

    /// Run the host-side room loop.
    pub async fn run_host(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,

                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        tracing::warn!("Endpoint closed");
                        break;
                    };
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("Failed to accept connection: {}", e);
                            continue;
                        }
                    };
                    if let Err(e) = self.handle_host_new_connection(conn).await {
                        tracing::warn!("Failed to handle new connection: {}", e);
                    }
                }

                event = self.event_rx.recv() => {
                    match event {
                        Some(RoomEvent::PeerDisconnected(peer_id)) => {
                            self.handle_peer_disconnected(peer_id).await;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
        }
        self.cleanup_all_peers();
        Ok(())
    }

    /// Run the joiner-side room loop.
    pub async fn run_joiner(&mut self, host_addr: EndpointAddr) -> Result<()> {
        // Connect to host
        let host_conn = net::connect_to_peer(&self.endpoint, host_addr.clone(), self.cancel.clone()).await?;
        let host_id = host_conn.remote_id();

        // Open control bidi stream to host
        let (mut send, mut recv) = host_conn.open_bi().await?;
        send.write_all(&[STREAM_TYPE_CONTROL]).await?;

        // Send Hello with our address
        let our_addr = self.endpoint.addr();
        let hello = ControlMessage::Hello {
            addr: addr_to_string(&our_addr),
        };
        control::write_control_msg(&mut send, &hello).await?;

        // Read PeerList from host
        let peer_list_msg = control::read_control_msg(&mut recv).await?;
        let initial_peers = match peer_list_msg {
            ControlMessage::PeerList { peers } => peers,
            other => {
                anyhow::bail!("Expected PeerList from host, got: {:?}", other);
            }
        };

        // Add host as a peer with media tasks
        let host_cancel = CancellationToken::new();
        self.add_peer_with_media(host_id, host_conn.clone(), host_addr, host_cancel.clone(), None);

        // Spawn task to read control messages from host
        let hc = host_cancel.clone();
        let ctrl_event_tx = self.event_tx.clone();
        let ctrl_cancel = self.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = ctrl_cancel.cancelled() => break,
                    _ = hc.cancelled() => break,
                    result = control::read_control_msg(&mut recv) => {
                        match result {
                            Ok(msg) => {
                                if ctrl_event_tx.send(RoomEvent::ControlMsg(msg)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("Host control stream ended: {}", e);
                                let _ = ctrl_event_tx.send(RoomEvent::PeerDisconnected(host_id)).await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Connect to initial peers, or expect them to connect to us.
        let mut expected_peers: HashMap<PeerId, EndpointAddr> = HashMap::new();
        let our_id = self.endpoint.id();
        for peer_info in &initial_peers {
            let peer_addr = match string_to_addr(&peer_info.addr) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("Failed to parse peer addr: {}", e);
                    continue;
                }
            };
            let peer_id = match peer_info.peer_id.parse::<PublicKey>() {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("Failed to parse peer id: {}", e);
                    continue;
                }
            };

            if should_initiate(&our_id, &peer_id) {
                self.connect_to_peer(peer_id, peer_addr).await;
            } else {
                // The other peer will connect to us — remember their addr so
                // we can match the incoming connection.
                expected_peers.insert(peer_id, peer_addr);
            }
        }

        // Spawn accept loop
        let accept_event_tx = self.event_tx.clone();
        let accept_cancel = self.cancel.clone();
        let accept_endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => break,
                    incoming = accept_endpoint.accept() => {
                        let Some(incoming) = incoming else { break };
                        match incoming.await {
                            Ok(conn) => {
                                let _ = accept_event_tx.send(RoomEvent::IncomingConnection(conn)).await;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to accept peer connection: {}", e);
                            }
                        }
                    }
                }
            }
        });

        // Main event loop
        let mut pending_connections: HashMap<PeerId, Connection> = HashMap::new();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,

                event = self.event_rx.recv() => {
                    match event {
                        Some(RoomEvent::PeerDisconnected(peer_id)) => {
                            if peer_id == host_id {
                                tracing::info!("Host disconnected");
                                break;
                            }
                            self.handle_peer_disconnected(peer_id).await;
                        }
                        Some(RoomEvent::IncomingConnection(conn)) => {
                            let remote_id = conn.remote_id();
                            if self.peers.contains_key(&remote_id) {
                                tracing::debug!("Duplicate connection from {}, closing", peer_id_to_string(&remote_id));
                                continue;
                            }
                            if let Some(addr) = expected_peers.remove(&remote_id) {
                                self.add_media_peer(remote_id, conn, addr);
                            } else {
                                pending_connections.insert(remote_id, conn);
                            }
                        }
                        Some(RoomEvent::ControlMsg(msg)) => {
                            match msg {
                                ControlMessage::PeerJoined { peer } => {
                                    let peer_id = match peer.peer_id.parse::<PublicKey>() {
                                        Ok(id) => id,
                                        Err(e) => {
                                            tracing::warn!("Failed to parse peer id: {}", e);
                                            continue;
                                        }
                                    };
                                    let peer_addr = match string_to_addr(&peer.addr) {
                                        Ok(a) => a,
                                        Err(e) => {
                                            tracing::warn!("Failed to parse peer addr: {}", e);
                                            continue;
                                        }
                                    };

                                    let our_id = self.endpoint.id();
                                    if should_initiate(&our_id, &peer_id) {
                                        self.connect_to_peer(peer_id, peer_addr).await;
                                    } else if let Some(conn) = pending_connections.remove(&peer_id) {
                                        self.add_media_peer(peer_id, conn, peer_addr);
                                    } else {
                                        expected_peers.insert(peer_id, peer_addr);
                                        let pid = peer_id;
                                        let cancel = self.cancel.clone();
                                        tokio::spawn(async move {
                                            tokio::select! {
                                                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                                                    tracing::warn!("Timed out waiting for connection from peer {}", peer_id_to_string(&pid));
                                                }
                                                _ = cancel.cancelled() => {}
                                            }
                                        });
                                    }
                                }
                                ControlMessage::PeerLeft { peer_id } => {
                                    if let Ok(id) = peer_id.parse::<PublicKey>() {
                                        self.handle_peer_disconnected(id).await;
                                        expected_peers.remove(&id);
                                        pending_connections.remove(&id);
                                    }
                                }
                                _ => {
                                    tracing::debug!("Unexpected control message: {:?}", msg);
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        self.cleanup_all_peers();
        Ok(())
    }

    /// Add a peer with full media pipeline (dispatcher, decoders) + broadcaster registration.
    fn add_peer_with_media(
        &mut self,
        peer_id: PeerId,
        conn: Connection,
        addr: EndpointAddr,
        session_cancel: CancellationToken,
        control_send: Option<iroh::endpoint::SendStream>,
    ) {
        if self.peers.contains_key(&peer_id) {
            tracing::debug!("Already connected to {}, ignoring", peer_id_to_string(&peer_id));
            return;
        }

        // Lazily start camera/encoder on first peer.
        if let Some(init) = self.deferred_capture.take() {
            let resources = init();
            if let Some(rx) = resources.audio_capture_rx {
                let bc = self.audio_broadcaster.clone();
                let ac = self.cancel.clone();
                tokio::spawn(async move {
                    crate::audio_encode::run_audio_encoder(rx, bc, ac).await;
                });
            }
            if let Some(encoder) = resources.video_encoder {
                Self::start_video_encoder(&self.video_broadcaster, &self.cancel, encoder);
            }
        }

        // Register for broadcast
        self.audio_broadcaster.add_peer(peer_id, conn.clone());
        self.video_broadcaster.add_peer(peer_id, conn.clone());

        // Per-peer receive channels
        let (audio_payload_tx, audio_payload_rx) = mpsc::channel(20);
        let (video_payload_tx, video_payload_rx) = mpsc::channel(4);

        // Dispatcher
        let dispatcher_handle = {
            let sc = session_cancel.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                dispatch::run_dispatcher(conn, audio_payload_tx, video_payload_tx, None, sc).await;
            })
        };

        // Audio decoder → mixer
        let audio_decoder_handle = {
            let sc = session_cancel.clone();
            let mtx = self.mixer_tx.clone();
            tokio::spawn(async move {
                let _ = audio_decode::run_audio_decoder(peer_id, audio_payload_rx, mtx, sc).await;
            })
        };

        // Video decoder → compositor
        let video_decoder_handle = {
            let sc = session_cancel.clone();
            let ctx = self.compositor_tx.clone();
            tokio::spawn(async move {
                let _ = video_decode::run_video_decoder(peer_id, video_payload_rx, ctx, sc).await;
            })
        };

        // Monitor connection for disconnection
        let event_tx = self.event_tx.clone();
        let sc = session_cancel.clone();
        let gc = self.cancel.clone();
        let conn_clone = conn.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = gc.cancelled() => {}
                _ = sc.cancelled() => {}
                _ = conn_clone.closed() => {
                    let _ = event_tx.send(RoomEvent::PeerDisconnected(peer_id)).await;
                }
            }
        });

        tracing::info!("Peer added with media: {}", peer_id_to_string(&peer_id));

        self.peers.insert(peer_id, PeerState {
            conn,
            addr,
            session_cancel,
            control_send,
            dispatcher_handle: Some(dispatcher_handle),
            audio_decoder_handle: Some(audio_decoder_handle),
            video_decoder_handle: Some(video_decoder_handle),
        });
    }

    /// Add a media peer (direct connection, no control channel).
    fn add_media_peer(&mut self, peer_id: PeerId, conn: Connection, addr: EndpointAddr) {
        let session_cancel = CancellationToken::new();
        self.add_peer_with_media(peer_id, conn, addr, session_cancel, None);
    }

    /// Host: handle a new incoming connection.
    async fn handle_host_new_connection(&mut self, conn: Connection) -> Result<()> {
        let peer_id = conn.remote_id();

        if self.peers.contains_key(&peer_id) {
            tracing::debug!("Duplicate connection from {}, ignoring", peer_id_to_string(&peer_id));
            return Ok(());
        }

        // Accept the bidi control stream from the joiner
        let (send, mut recv) = conn.accept_bi().await?;

        // Read stream type tag
        let mut tag = [0u8; 1];
        recv.read_exact(&mut tag).await?;
        anyhow::ensure!(
            tag[0] == STREAM_TYPE_CONTROL,
            "Expected control stream tag 0x01, got 0x{:02x}",
            tag[0]
        );

        // Read Hello
        let hello_msg = control::read_control_msg(&mut recv).await?;
        let peer_addr_str = match hello_msg {
            ControlMessage::Hello { addr } => addr,
            other => {
                anyhow::bail!("Expected Hello from peer, got: {:?}", other);
            }
        };
        let peer_addr = string_to_addr(&peer_addr_str)?;

        // Build PeerList of existing peers
        let existing_peers: Vec<PeerInfo> = self
            .peers
            .iter()
            .map(|(id, state)| PeerInfo {
                peer_id: peer_id_to_string(id),
                addr: addr_to_string(&state.addr),
            })
            .collect();

        // Send PeerList to new peer
        let mut control_send = send;
        let peer_list = ControlMessage::PeerList {
            peers: existing_peers,
        };
        control::write_control_msg(&mut control_send, &peer_list).await?;

        // Broadcast PeerJoined to all existing peers
        let joined_msg = ControlMessage::PeerJoined {
            peer: PeerInfo {
                peer_id: peer_id_to_string(&peer_id),
                addr: peer_addr_str,
            },
        };
        for (_, state) in self.peers.iter_mut() {
            if let Some(ref mut cs) = state.control_send
                && let Err(e) = control::write_control_msg(cs, &joined_msg).await {
                    tracing::warn!("Failed to send PeerJoined: {}", e);
                }
        }

        // Spawn a task to monitor control stream from peer
        let session_cancel = CancellationToken::new();
        let event_tx = self.event_tx.clone();
        let sc = session_cancel.clone();
        let gc = self.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = gc.cancelled() => break,
                    _ = sc.cancelled() => break,
                    result = control::read_control_msg(&mut recv) => {
                        match result {
                            Ok(msg) => {
                                tracing::debug!("Control msg from peer: {:?}", msg);
                            }
                            Err(_) => {
                                let _ = event_tx.send(RoomEvent::PeerDisconnected(peer_id)).await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        tracing::info!("Peer joined: {}", peer_id_to_string(&peer_id));

        // Add with full media pipeline
        self.add_peer_with_media(peer_id, conn, peer_addr, session_cancel, Some(control_send));

        Ok(())
    }

    /// Handle peer disconnection.
    async fn handle_peer_disconnected(&mut self, peer_id: PeerId) {
        if let Some(state) = self.peers.remove(&peer_id) {
            state.session_cancel.cancel();
            tracing::info!("Peer disconnected: {}", peer_id_to_string(&peer_id));

            // Remove from broadcasters
            self.audio_broadcaster.remove_peer(&peer_id);
            self.video_broadcaster.remove_peer(&peer_id);

            // Tell compositor to remove this peer's frame
            let _ = self.compositor_cmd_tx.try_send(CompositorCommand::RemovePeer(peer_id));

            // Clean up task handles in background
            tokio::spawn(async move {
                if let Some(h) = state.dispatcher_handle { let _ = h.await; }
                if let Some(h) = state.audio_decoder_handle { let _ = h.await; }
                if let Some(h) = state.video_decoder_handle { let _ = h.await; }
            });

            if self.is_host {
                let left_msg = ControlMessage::PeerLeft {
                    peer_id: peer_id_to_string(&peer_id),
                };
                for (_, state) in self.peers.iter_mut() {
                    if let Some(ref mut cs) = state.control_send
                        && let Err(e) = control::write_control_msg(cs, &left_msg).await {
                            tracing::warn!("Failed to send PeerLeft: {}", e);
                        }
                }
            }
        }
    }

    /// Joiner: connect to a peer and add them with media.
    async fn connect_to_peer(&mut self, peer_id: PeerId, peer_addr: EndpointAddr) {
        match net::connect_to_peer(&self.endpoint, peer_addr.clone(), self.cancel.clone()).await {
            Ok(conn) => {
                self.add_media_peer(peer_id, conn, peer_addr);
            }
            Err(e) => {
                tracing::warn!("Failed to connect to peer {}: {}", peer_id_to_string(&peer_id), e);
            }
        }
    }

    /// Clean up all peers on shutdown.
    fn cleanup_all_peers(&mut self) {
        for (peer_id, state) in self.peers.drain() {
            state.session_cancel.cancel();
            self.audio_broadcaster.remove_peer(&peer_id);
            self.video_broadcaster.remove_peer(&peer_id);
        }
    }

    /// Number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get a reference to the endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}
