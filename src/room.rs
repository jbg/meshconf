use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, PublicKey};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use crate::audio_decode;
use crate::pool;
use crate::audio_mixer::AudioMixer;
use crate::bandwidth::{BandwidthEstimator, BandwidthEvent};
use crate::broadcaster::Broadcaster;
use crate::control::{self, ControlMessage, PeerInfo, STREAM_TYPE_CONTROL};
use crate::dispatch;
use crate::jitter::{self, PeerClock};
use crate::net;
use crate::video_compositor::{CompositorCommand, VideoCompositor, VideoDropCounter};
use crate::video_compositor::GalleryFrame;
use crate::video_decode::{self, RgbFrame};
use crate::video_encode::VideoEncoder;

/// How often each peer broadcasts its full peer list to all control channels.
const PEER_LIST_INTERVAL: Duration = Duration::from_secs(5);

/// Peer identifier — an iroh Ed25519 public key.
pub type PeerId = PublicKey;

/// Events from per-peer tasks back to the room.
enum RoomEvent {
    PeerDisconnected(PeerId),
    /// An incoming connection from another peer.
    IncomingConnection(Connection),
    /// A control message received from any peer, tagged with sender.
    ControlMsg(PeerId, ControlMessage),
}

/// Interval at which each peer sends congestion feedback to its remote.
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(1);

/// State for one remote peer.
struct PeerState {
    #[allow(dead_code)]
    conn: Connection,
    addr: EndpointAddr,
    /// Display name received from this peer (via Hello or PeerList).
    name: String,
    session_cancel: CancellationToken,
    /// Send half of the control bidi stream to this peer.
    control_send: Option<iroh::endpoint::SendStream>,
    /// Per-peer task handles.
    dispatcher_handle: Option<JoinHandle<()>>,
    audio_decoder_handle: Option<JoinHandle<()>>,
    video_decoder_handle: Option<JoinHandle<()>>,
    feedback_handle: Option<JoinHandle<()>>,
    /// Video stats for this peer (for congestion feedback reporting).
    #[allow(dead_code)]
    video_drops: VideoDropCounter,
}

/// Media resources passed to the Room at creation time.
pub struct MediaResources {
    /// Audio capture receiver (from mic). If None, no audio sending.
    pub audio_capture_rx: Option<mpsc::Receiver<pool::SharedBuf<f32>>>,
    /// Video encoder (owns capture). If None, no video sending.
    pub video_encoder: Option<VideoEncoder>,
    /// Playback channel for the audio engine. If None, no audio playback.
    pub playback_tx: Option<mpsc::Sender<pool::SharedBuf<f32>>>,
    /// Display channel for video. If None, no video display.
    pub display_tx: Option<mpsc::Sender<GalleryFrame>>,
    /// Pre-created compositor input channel.  When provided the Room uses
    /// this pair instead of creating its own, which allows the caller to
    /// hold additional Sender clones (e.g. for a local camera preview).
    pub compositor_channel: Option<(
        mpsc::Sender<(PeerId, RgbFrame)>,
        mpsc::Receiver<(PeerId, RgbFrame)>,
    )>,
    /// Shared encoder config for adaptive bitrate.  Created by the caller
    /// (so it can also be passed to `VideoEncoder::start`) and read/written
    /// by the bandwidth estimator.
    pub encoder_config: Option<std::sync::Arc<crate::bandwidth::EncoderConfig>>,
    /// Local user's display name (typically the OS username).
    pub our_name: String,
}

/// Manages the set of peers and their connections in a call.
///
/// There is no host/joiner distinction. Every participant runs the same
/// event loop: accepting incoming connections (anyone can share their
/// ticket) and optionally connecting to an initial peer via a ticket
/// provided on the command line.
///
/// Membership is propagated via periodic full PeerList broadcasts on
/// every control channel, so all participants converge on the same view
/// even if a peer that introduced two others disconnects before the
/// introduction is complete.
pub struct Room {
    endpoint: Endpoint,
    peers: HashMap<PeerId, PeerState>,
    cancel: CancellationToken,
    event_tx: mpsc::Sender<RoomEvent>,
    event_rx: mpsc::Receiver<RoomEvent>,

    // Shared media resources
    audio_broadcaster: Broadcaster,
    video_broadcaster: Broadcaster,
    mixer_tx: mpsc::Sender<(PeerId, pool::SharedBuf<f32>)>,
    compositor_tx: mpsc::Sender<(PeerId, RgbFrame)>,
    compositor_cmd_tx: mpsc::Sender<CompositorCommand>,

    /// Bandwidth estimator event channel (if ABR is active).
    bw_tx: Option<mpsc::Sender<BandwidthEvent>>,

    /// Our display name, sent to peers in Hello and included in PeerList.
    our_name: String,
}

/// Determine which side initiates the connection when two peers discover
/// each other. The peer with the lexicographically smaller public key
/// connects; the other waits.
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
        cancel: CancellationToken,
        mut media: MediaResources,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(64);

        // --- Audio mixer ---
        let (mixer_tx, mixer_rx) = mpsc::channel::<(PeerId, pool::SharedBuf<f32>)>(50);
        if let Some(playback_tx) = media.playback_tx.take() {
            let mixer = AudioMixer {
                rx: mixer_rx,
                playback_tx,
            };
            let mc = cancel.clone();
            tokio::spawn(async move { mixer.run(mc).await });
        } else {
            drop(mixer_rx);
        }

        // --- Video compositor ---
        let (compositor_tx, compositor_rx) = media
            .compositor_channel
            .take()
            .unwrap_or_else(|| mpsc::channel::<(PeerId, RgbFrame)>(10));
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

        // --- Bandwidth estimator ---
        let bw_tx = if let Some(ref enc_config) = media.encoder_config {
            let (bw_tx, _bw_handle, bw_estimator) = BandwidthEstimator::new(enc_config.clone());
            let bc = cancel.clone();
            tokio::spawn(async move { bw_estimator.run(bc).await });
            Some(bw_tx)
        } else {
            None
        };

        // --- Broadcasters ---
        let audio_broadcaster = Broadcaster::new();

        // Create video broadcaster with the encoder's keyframe flag so that
        // whenever a new peer is added, the encoder is told to produce a
        // keyframe. Without this, late-joining peers only receive P-frames
        // and cannot decode the video until the next natural keyframe.
        let video_broadcaster = match media.video_encoder {
            Some(ref encoder) => Broadcaster::new_with_keyframe(encoder.force_keyframe_flag()),
            None => Broadcaster::new(),
        };

        // Start audio encoder immediately if provided.
        if let Some(rx) = media.audio_capture_rx.take() {
            let bc = audio_broadcaster.clone();
            let ac = cancel.clone();
            tokio::spawn(async move {
                crate::audio_encode::run_audio_encoder(rx, bc, ac).await;
            });
        }

        // Start video encoder broadcast loop immediately if available.
        if let Some(encoder) = media.video_encoder.take() {
            let bc = video_broadcaster.clone();
            let vc = cancel.clone();
            let btx = bw_tx.clone();
            tokio::spawn(async move {
                encoder.send_loop(bc, btx, vc).await;
            });
        }

        // Tell the compositor our own name and that the local tile is "Local".
        let our_id = endpoint.id();
        let our_name = media.our_name;
        let _ = compositor_cmd_tx.try_send(CompositorCommand::SetPeerName(our_id, Arc::from(our_name.as_str())));
        let _ = compositor_cmd_tx.try_send(CompositorCommand::SetConnectionKind(
            our_id,
            crate::video_compositor::ConnectionKind::Local,
        ));

        Self {
            endpoint,
            peers: HashMap::new(),
            cancel,
            event_tx,
            event_rx,
            audio_broadcaster,
            video_broadcaster,
            mixer_tx,
            compositor_tx,
            compositor_cmd_tx,
            bw_tx,
            our_name,
        }
    }

    /// Spawn a task that reads control messages from a peer's recv stream
    /// and forwards them as RoomEvents.
    fn spawn_control_reader(
        peer_id: PeerId,
        mut recv: iroh::endpoint::RecvStream,
        event_tx: mpsc::Sender<RoomEvent>,
        session_cancel: CancellationToken,
        global_cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = global_cancel.cancelled() => break,
                    _ = session_cancel.cancelled() => break,
                    result = control::read_control_msg(&mut recv) => {
                        match result {
                            Ok(msg) => {
                                if event_tx.send(RoomEvent::ControlMsg(peer_id, msg)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("Control stream from {} ended: {}", peer_id_to_string(&peer_id), e);
                                let _ = event_tx.send(RoomEvent::PeerDisconnected(peer_id)).await;
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Build a PeerList message containing ourselves and all connected peers.
    fn build_peer_list(&self) -> ControlMessage {
        let our_addr = self.endpoint.addr();
        let mut peers: Vec<PeerInfo> = vec![PeerInfo {
            peer_id: peer_id_to_string(&self.endpoint.id()),
            addr: addr_to_string(&our_addr),
            name: self.our_name.clone(),
        }];
        peers.extend(self.peers.iter().map(|(id, state)| PeerInfo {
            peer_id: peer_id_to_string(id),
            addr: addr_to_string(&state.addr),
            name: state.name.clone(),
        }));
        ControlMessage::PeerList { peers }
    }

    /// Send our current peer list to all peers that have a control channel.
    async fn broadcast_peer_list(&mut self) {
        let msg = self.build_peer_list();
        for (pid, state) in self.peers.iter_mut() {
            if let Some(ref mut cs) = state.control_send {
                if let Err(e) = control::write_control_msg(cs, &msg).await {
                    tracing::debug!(
                        "Failed to send PeerList to {}: {}",
                        peer_id_to_string(pid),
                        e
                    );
                }
            }
        }
    }

    /// Process a PeerList received from another peer. For each peer in the
    /// list that we don't already know about, initiate a connection (if we
    /// should, per the tie-breaker) or wait for them to connect to us.
    /// Also updates names for peers we already know.
    async fn handle_received_peer_list(&mut self, peers: Vec<PeerInfo>) {
        let our_id = self.endpoint.id();
        for info in peers {
            let peer_id = match info.peer_id.parse::<PublicKey>() {
                Ok(id) => id,
                Err(_) => continue,
            };
            if peer_id == our_id {
                continue;
            }
            // Update name for existing peers if it changed.
            if let Some(state) = self.peers.get_mut(&peer_id) {
                if !info.name.is_empty() && state.name != info.name {
                    state.name = info.name.clone();
                    let _ = self.compositor_cmd_tx.try_send(
                        CompositorCommand::SetPeerName(peer_id, Arc::from(info.name.as_str())),
                    );
                }
                continue;
            }
            let peer_addr = match string_to_addr(&info.addr) {
                Ok(a) => a,
                Err(_) => continue,
            };
            if should_initiate(&our_id, &peer_id) {
                self.connect_to_peer(peer_id, peer_addr).await;
            }
            // Otherwise: the other peer will connect to us via our accept loop.
        }
    }

    /// Run the room event loop.
    ///
    /// If `initial_peer_addr` is provided, we first connect to that peer,
    /// exchange Hello/PeerList, and connect to any peers it tells us about.
    /// Regardless, we accept incoming connections from anyone who has our
    /// ticket.
    pub async fn run(&mut self, initial_peer_addr: Option<EndpointAddr>) -> Result<()> {
        // If we have an initial peer (we joined via their ticket), connect
        // and get the existing peer list.
        if let Some(addr) = initial_peer_addr {
            self.join_via_peer(addr).await?;
        }

        // Spawn accept loop — runs for the lifetime of the room.
        let accept_event_tx = self.event_tx.clone();
        let accept_cancel = self.cancel.clone();
        let accept_endpoint = self.endpoint.clone();
        tracing::info!("Starting accept loop");
        tokio::spawn(async move {
            tracing::info!("Accept loop task running");
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => {
                        tracing::info!("Accept loop cancelled");
                        break;
                    }
                    incoming = accept_endpoint.accept() => {
                        tracing::info!("Accept loop got incoming connection attempt");
                        let Some(incoming) = incoming else {
                            tracing::info!("Accept loop: no more incoming connections");
                            break;
                        };
                        match incoming.await {
                            Ok(conn) => {
                                tracing::info!(remote = %conn.remote_id().fmt_short(), "Accepted connection");
                                let _ = accept_event_tx.send(RoomEvent::IncomingConnection(conn)).await;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to accept connection: {}", e);
                            }
                        }
                    }
                }
            }
        });

        // Periodic peer-list broadcast timer.
        let mut peer_list_interval = tokio::time::interval(PEER_LIST_INTERVAL);
        peer_list_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the first immediate tick — we just sent peer lists during
        // the handshake, and we also broadcast right after adding a peer.
        peer_list_interval.tick().await;

        // Main event loop
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,

                _ = peer_list_interval.tick() => {
                    self.broadcast_peer_list().await;
                }

                event = self.event_rx.recv() => {
                    match event {
                        Some(RoomEvent::PeerDisconnected(peer_id)) => {
                            self.handle_peer_disconnected(peer_id).await;
                            // Peer set changed — broadcast immediately so
                            // others learn about the departure quickly.
                            self.broadcast_peer_list().await;
                        }

                        Some(RoomEvent::IncomingConnection(conn)) => {
                            if let Err(e) = self.handle_incoming_connection(conn).await {
                                tracing::warn!("Failed to handle incoming connection: {}", e);
                            }
                        }

                        Some(RoomEvent::ControlMsg(_from, msg)) => {
                            match msg {
                                ControlMessage::PeerList { peers } => {
                                    self.handle_received_peer_list(peers).await;
                                }
                                ControlMessage::CongestionFeedback {
                                    fraction_lost, jitter_us, ..
                                } => {
                                    if let Some(ref tx) = self.bw_tx {
                                        let _ = tx.try_send(BandwidthEvent::ReceiverFeedback {
                                            fraction_lost,
                                            jitter_us,
                                        });
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

    /// Connect to an initial peer (the one whose ticket we were given),
    /// exchange Hello/PeerList, and initiate connections to any peers it
    /// tells us about.
    async fn join_via_peer(&mut self, addr: EndpointAddr) -> Result<()> {
        let conn = net::connect_to_peer(&self.endpoint, addr.clone(), self.cancel.clone()).await?;
        let peer_id = conn.remote_id();

        let (send, recv, initial_peers) =
            self.open_control_and_add_peer(peer_id, conn, addr).await?;

        // Store the control send half.
        if let Some(state) = self.peers.get_mut(&peer_id) {
            state.control_send = Some(send);
        }

        // Spawn control reader.
        let session_cancel = self
            .peers
            .get(&peer_id)
            .map(|s| s.session_cancel.clone())
            .unwrap_or_else(CancellationToken::new);
        Self::spawn_control_reader(
            peer_id,
            recv,
            self.event_tx.clone(),
            session_cancel,
            self.cancel.clone(),
        );

        // Connect to peers from the list.
        self.handle_received_peer_list(initial_peers).await;

        Ok(())
    }

    /// Open a control bidi, send Hello, read PeerList, add the peer with
    /// media. Returns the send and recv halves of the bidi stream plus the
    /// peer list so the caller can set up the control reader and act on the
    /// peer list.
    async fn open_control_and_add_peer(
        &mut self,
        peer_id: PeerId,
        conn: Connection,
        addr: EndpointAddr,
    ) -> Result<(
        iroh::endpoint::SendStream,
        iroh::endpoint::RecvStream,
        Vec<PeerInfo>,
    )> {
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&[STREAM_TYPE_CONTROL]).await?;

        let our_addr = self.endpoint.addr();
        let hello = ControlMessage::Hello {
            addr: addr_to_string(&our_addr),
            name: self.our_name.clone(),
        };
        control::write_control_msg(&mut send, &hello).await?;

        let peer_list_msg = control::read_control_msg(&mut recv).await?;
        let peers = match peer_list_msg {
            ControlMessage::PeerList { peers } => peers,
            other => {
                anyhow::bail!("Expected PeerList, got: {:?}", other);
            }
        };

        let session_cancel = CancellationToken::new();
        // We don't know their name yet from the PeerList response — it
        // will arrive later via their PeerList broadcasts. Use empty for now.
        self.add_peer_with_media(peer_id, conn, addr, session_cancel, None, String::new());

        Ok((send, recv, peers))
    }

    /// Handle a connection initiated by another peer (either via our ticket
    /// or via a mesh `connect_to_peer`). All incoming connections carry a
    /// control bidi with a Hello/PeerList exchange.
    async fn handle_incoming_connection(&mut self, conn: Connection) -> Result<()> {
        let remote_id = conn.remote_id();

        if self.peers.contains_key(&remote_id) {
            tracing::debug!(
                "Duplicate connection from {}, ignoring",
                peer_id_to_string(&remote_id)
            );
            return Ok(());
        }

        // Accept the control bidi stream.
        let (send, mut recv) = conn.accept_bi().await?;

        // Read stream type tag.
        let mut tag = [0u8; 1];
        recv.read_exact(&mut tag).await?;
        anyhow::ensure!(
            tag[0] == STREAM_TYPE_CONTROL,
            "Expected control stream tag 0x01, got 0x{:02x}",
            tag[0]
        );

        // Read Hello.
        let hello_msg = control::read_control_msg(&mut recv).await?;
        let (peer_addr_str, peer_name) = match hello_msg {
            ControlMessage::Hello { addr, name } => (addr, name),
            other => {
                anyhow::bail!("Expected Hello, got: {:?}", other);
            }
        };
        let peer_addr = string_to_addr(&peer_addr_str)?;

        // Send our current peer list.
        let mut control_send = send;
        let peer_list = self.build_peer_list();
        control::write_control_msg(&mut control_send, &peer_list).await?;

        // Add peer with media + control.
        let session_cancel = CancellationToken::new();
        Self::spawn_control_reader(
            remote_id,
            recv,
            self.event_tx.clone(),
            session_cancel.clone(),
            self.cancel.clone(),
        );

        tracing::info!("Peer connected: {}", peer_id_to_string(&remote_id));
        self.add_peer_with_media(
            remote_id,
            conn,
            peer_addr,
            session_cancel,
            Some(control_send),
            peer_name,
        );

        // Peer set changed — broadcast immediately so existing peers learn
        // about the newcomer without waiting for the next periodic tick.
        self.broadcast_peer_list().await;

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
        name: String,
    ) {
        if self.peers.contains_key(&peer_id) {
            tracing::debug!(
                "Already connected to {}, ignoring",
                peer_id_to_string(&peer_id)
            );
            return;
        }

        // Register for broadcast.
        self.audio_broadcaster.add_peer(peer_id, conn.clone());
        self.video_broadcaster.add_peer(peer_id, conn.clone());
        tracing::info!(
            "add_peer_with_media: peer {} registered with broadcasters",
            peer_id_to_string(&peer_id),
        );

        // Per-peer receive channels.
        let (audio_payload_tx, audio_payload_rx) = mpsc::channel(20);
        // Video goes directly from dispatcher → decoder (no jitter buffer).
        let (video_payload_tx, video_payload_rx) = mpsc::channel(8);

        // Audio jitter buffer output.
        let (audio_jitter_tx, audio_jitter_rx) = mpsc::channel(20);

        // Shared peer clock (audio drives).
        let peer_clock = PeerClock::new();

        // Shared video drop counter — incremented by dispatcher and decoder
        // on any video frame drop or decode failure.
        let video_drops = VideoDropCounter::new();

        // Register the drop counter with the compositor so it can include
        // the count in gallery frames for the display to show.
        let _ = self.compositor_cmd_tx.try_send(
            CompositorCommand::SetVideoDropCounter(peer_id, video_drops.clone()),
        );

        // Dispatcher.
        let dispatcher_handle = {
            let sc = session_cancel.clone();
            let conn = conn.clone();
            let vd = video_drops.clone();
            let btx = self.bw_tx.clone();
            tokio::spawn(async move {
                dispatch::run_dispatcher(conn, audio_payload_tx, video_payload_tx, vd, None, btx, sc).await;
            })
        };

        // Audio jitter buffer (clock master, emits PLC sentinels).
        {
            let pc = peer_clock.clone();
            let sc = session_cancel.clone();
            tokio::spawn(async move {
                jitter::run_jitter_buffer(
                    jitter::audio_config(),
                    pc,
                    true,  // is_clock_master
                    true,  // emit_plc
                    audio_payload_rx,
                    audio_jitter_tx,
                    None,  // no drop counter for audio
                    sc,
                )
                .await;
            });
        }

        // Audio decoder → mixer.
        let audio_decoder_handle = {
            let sc = session_cancel.clone();
            let mtx = self.mixer_tx.clone();
            tokio::spawn(async move {
                let _ = audio_decode::run_audio_decoder(peer_id, audio_jitter_rx, mtx, sc).await;
            })
        };

        // Video decoder → compositor (no jitter buffer — the decoder's
        // keyframe gate and gap detection handle ordering, and the
        // compositor renders at its own rate).
        let video_decoder_handle = {
            let sc = session_cancel.clone();
            let ctx = self.compositor_tx.clone();
            let vd = video_drops.clone();
            tokio::spawn(async move {
                let _ = video_decode::run_video_decoder(peer_id, video_payload_rx, ctx, vd, sc).await;
            })
        };

        // Congestion feedback sender — periodically reports receive-side
        // video stats back to the remote peer so they can adapt their
        // encoder bitrate.
        let feedback_handle = {
            let sc = session_cancel.clone();
            let gc = self.cancel.clone();
            let vd = video_drops.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                run_feedback_sender(conn, vd, sc, gc).await;
            })
        };

        // Monitor connection for disconnection.
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

        // Inform compositor of peer name.
        if !name.is_empty() {
            let _ = self.compositor_cmd_tx.try_send(
                CompositorCommand::SetPeerName(peer_id, Arc::from(name.as_str())),
            );
        }

        // Spawn path watcher — monitors whether the connection is direct
        // or relayed and sends updates to the compositor.
        {
            let cmd_tx = self.compositor_cmd_tx.clone();
            let sc = session_cancel.clone();
            let gc = self.cancel.clone();
            let mut watcher = conn.paths();
            tokio::spawn(async move {
                use iroh::Watcher;
                use iroh::endpoint::PathInfoList;
                use crate::video_compositor::ConnectionKind;

                fn classify(paths: &PathInfoList) -> ConnectionKind {
                    for path in paths.iter() {
                        if path.is_selected() {
                            return if path.is_relay() {
                                ConnectionKind::Relay
                            } else {
                                ConnectionKind::Direct
                            };
                        }
                    }
                    ConnectionKind::Unknown
                }

                let mut last_kind = ConnectionKind::Unknown;
                loop {
                    let kind = classify(&watcher.get());
                    if kind != last_kind {
                        last_kind = kind;
                        let _ = cmd_tx.try_send(
                            CompositorCommand::SetConnectionKind(peer_id, kind),
                        );
                    }
                    tokio::select! {
                        _ = gc.cancelled() => break,
                        _ = sc.cancelled() => break,
                        result = watcher.updated() => {
                            if result.is_err() { break; }
                        }
                    }
                }
            });
        }

        tracing::info!("Peer added with media: {}", peer_id_to_string(&peer_id));

        self.peers.insert(
            peer_id,
            PeerState {
                conn,
                addr,
                name,
                session_cancel,
                control_send,
                dispatcher_handle: Some(dispatcher_handle),
                audio_decoder_handle: Some(audio_decoder_handle),
                video_decoder_handle: Some(video_decoder_handle),
                feedback_handle: Some(feedback_handle),
                video_drops,
            },
        );
    }

    /// Handle peer disconnection — detected locally via connection close.
    async fn handle_peer_disconnected(&mut self, peer_id: PeerId) {
        if let Some(state) = self.peers.remove(&peer_id) {
            state.session_cancel.cancel();
            tracing::info!("Peer disconnected: {}", peer_id_to_string(&peer_id));

            // Remove from broadcasters.
            self.audio_broadcaster.remove_peer(&peer_id);
            self.video_broadcaster.remove_peer(&peer_id);

            // Tell compositor to remove this peer's frame.
            let _ = self
                .compositor_cmd_tx
                .try_send(CompositorCommand::RemovePeer(peer_id));

            // Clean up task handles in background.
            tokio::spawn(async move {
                if let Some(h) = state.dispatcher_handle {
                    let _ = h.await;
                }
                if let Some(h) = state.audio_decoder_handle {
                    let _ = h.await;
                }
                if let Some(h) = state.video_decoder_handle {
                    let _ = h.await;
                }
                if let Some(h) = state.feedback_handle {
                    let _ = h.await;
                }
            });
        }
    }

    /// Connect to a peer (mesh connection triggered by a received PeerList).
    /// Opens a control bidi and performs the Hello/PeerList handshake so
    /// the receiving side can handle it uniformly via
    /// `handle_incoming_connection`.
    async fn connect_to_peer(&mut self, peer_id: PeerId, peer_addr: EndpointAddr) {
        if self.peers.contains_key(&peer_id) {
            return;
        }
        let conn = match net::connect_to_peer(
            &self.endpoint,
            peer_addr.clone(),
            self.cancel.clone(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "Failed to connect to peer {}: {}",
                    peer_id_to_string(&peer_id),
                    e
                );
                return;
            }
        };

        // Perform control handshake (open bidi, send Hello, read PeerList).
        match self
            .open_control_and_add_peer(peer_id, conn, peer_addr)
            .await
        {
            Ok((send, recv, _peer_list)) => {
                // Store the control send half.
                if let Some(state) = self.peers.get_mut(&peer_id) {
                    state.control_send = Some(send);
                }
                // Spawn control reader.
                let session_cancel = self
                    .peers
                    .get(&peer_id)
                    .map(|s| s.session_cancel.clone())
                    .unwrap_or_else(CancellationToken::new);
                Self::spawn_control_reader(
                    peer_id,
                    recv,
                    self.event_tx.clone(),
                    session_cancel,
                    self.cancel.clone(),
                );
                // Broadcast updated peer list immediately.
                self.broadcast_peer_list().await;
            }
            Err(e) => {
                tracing::warn!(
                    "Control handshake failed with {}: {}",
                    peer_id_to_string(&peer_id),
                    e
                );
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

/// Periodically send congestion feedback to a remote peer.
///
/// Opens a bidi stream to the remote and sends `CongestionFeedback`
/// messages every [`FEEDBACK_INTERVAL`].  The stats come from the
/// `VideoDropCounter` which tracks both received and dropped frames.
async fn run_feedback_sender(
    conn: Connection,
    video_drops: VideoDropCounter,
    session_cancel: CancellationToken,
    global_cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(FEEDBACK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the first tick (no data yet).
    interval.tick().await;

    let mut prev_received: u64 = 0;
    let mut prev_dropped: u64 = 0;

    loop {
        tokio::select! {
            _ = global_cancel.cancelled() => break,
            _ = session_cancel.cancelled() => break,
            _ = interval.tick() => {
                let cur_received = video_drops.get_received();
                let cur_dropped = video_drops.get();

                let delta_received = cur_received.saturating_sub(prev_received);
                let delta_dropped = cur_dropped.saturating_sub(prev_dropped);
                prev_received = cur_received;
                prev_dropped = cur_dropped;

                let total = delta_received + delta_dropped;
                let fraction_lost = if total > 0 {
                    delta_dropped as f64 / total as f64
                } else {
                    0.0
                };

                let msg = ControlMessage::CongestionFeedback {
                    received: delta_received,
                    dropped: delta_dropped,
                    fraction_lost,
                    jitter_us: 0, // TODO: plumb jitter from jitter buffer
                };

                // Send on a fresh bidi stream.  If the peer doesn't handle
                // it (older version), the stream is simply reset — no harm.
                match conn.open_bi().await {
                    Ok((mut send, _recv)) => {
                        if send.write_all(&[control::STREAM_TYPE_CONTROL]).await.is_err() {
                            continue;
                        }
                        if let Err(e) = control::write_control_msg(&mut send, &msg).await {
                            tracing::trace!("Failed to send congestion feedback: {e}");
                        }
                        let _ = send.finish();
                    }
                    Err(_) => break, // connection closed
                }
            }
        }
    }
}
