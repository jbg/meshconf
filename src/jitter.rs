use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::pool::SharedBuf;
use crate::protocol::{self, MediaObject};
use crate::video_compositor::VideoDropCounter;

/// Configuration for a jitter buffer instance.
#[derive(Clone, Debug)]
pub struct JitterBufferConfig {
    /// Target playout delay added to each packet's timestamp (microseconds).
    /// This is the amount of jitter the buffer can absorb.
    pub target_delay_us: u64,

    /// Maximum age of a packet (relative to playout clock) before it is
    /// discarded on arrival. Packets older than this are too late to play.
    pub max_late_us: u64,

    /// How quickly the clock offset adapts to drift (0.0–1.0).
    /// Lower = more stable, higher = faster adaptation.
    pub drift_alpha: f64,

    /// Label for logging (e.g. "audio" or "video").
    pub label: &'static str,
}

/// Default audio jitter buffer configuration.
pub fn audio_config() -> JitterBufferConfig {
    JitterBufferConfig {
        target_delay_us: 60_000,  // 60ms — 3 Opus frames
        max_late_us: 40_000,      // 40ms
        drift_alpha: 0.005,
        label: "audio",
    }
}

/// Default video jitter buffer configuration.
pub fn video_config() -> JitterBufferConfig {
    JitterBufferConfig {
        target_delay_us: 40_000,  // 40ms
        max_late_us: 80_000,      // 80ms
        drift_alpha: 0.01,
        label: "video",
    }
}

/// Shared clock offset per peer, driven by audio, read by video.
/// Ensures A/V sync by using the same time mapping for both media types.
pub struct PeerClock {
    /// Smoothed offset: local_us - remote_us (signed).
    offset_us: AtomicI64,
    /// Whether the offset has been initialised.
    initialised: std::sync::atomic::AtomicBool,
}

impl PeerClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            offset_us: AtomicI64::new(0),
            initialised: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Update the clock offset (called by the audio jitter buffer).
    pub fn update(&self, offset_us: i64) {
        self.offset_us.store(offset_us, Ordering::Relaxed);
        self.initialised.store(true, Ordering::Relaxed);
    }

    /// Read the current clock offset.
    pub fn offset_us(&self) -> i64 {
        self.offset_us.load(Ordering::Relaxed)
    }

    /// Whether the clock has been initialised with at least one sample.
    pub fn is_initialised(&self) -> bool {
        self.initialised.load(Ordering::Relaxed)
    }
}

/// Jitter buffer statistics.
#[derive(Debug, Default, Clone)]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    pub dropped_late: u64,
    pub dropped_duplicate: u64,
}

/// Sentinel value for a PLC (packet loss concealment) frame — audio only.
/// When the jitter buffer emits this, the decoder should generate a PLC frame.
pub const PLC_SENTINEL_TIMESTAMP: u64 = u64::MAX;

/// Create a PLC sentinel MediaObject (empty payload, special timestamp).
pub fn plc_sentinel() -> MediaObject {
    MediaObject {
        group_id: 0,
        subgroup_id: 0, object_id: 0,
        capture_timestamp_us: PLC_SENTINEL_TIMESTAMP,
        codec: None,
        payload: SharedBuf::from_vec(Vec::new()),
    }
}

/// Run the jitter buffer as an async task.
///
/// For audio: `is_clock_master` should be true — this buffer drives the PeerClock.
/// For video: `is_clock_master` should be false — it reads from the PeerClock.
///
/// When `emit_plc` is true (audio), emits PLC sentinel frames when playout time
/// arrives but no packet is available.
pub async fn run_jitter_buffer(
    config: JitterBufferConfig,
    peer_clock: Arc<PeerClock>,
    is_clock_master: bool,
    emit_plc: bool,
    mut rx: mpsc::Receiver<MediaObject>,
    tx: mpsc::Sender<MediaObject>,
    drop_counter: Option<VideoDropCounter>,
    cancel: CancellationToken,
) {
    let mut queue: BTreeMap<u64, MediaObject> = BTreeMap::new();
    let mut clock_offset_us: Option<i64> = None;
    let mut last_emitted_ts: Option<u64> = None;
    let mut stats = JitterStats::default();
    let mut last_stats_log = Instant::now();

    // For non-master (video), try to bootstrap from shared clock
    if !is_clock_master && peer_clock.is_initialised() {
        clock_offset_us = Some(peer_clock.offset_us());
    }

    loop {
        // Determine next playout time
        let next_playout = queue.keys().next().and_then(|&ts| {
            let offset = clock_offset_us?;
            // playout_local_us = capture_ts + offset
            let playout_us = (ts as i64 + offset) as u64;
            let now = protocol::now_us();
            if playout_us > now {
                Some(Duration::from_micros(playout_us - now))
            } else {
                Some(Duration::ZERO)
            }
        });

        let sleep_dur = next_playout.unwrap_or(Duration::from_millis(5));

        tokio::select! {
            _ = cancel.cancelled() => break,

            obj = rx.recv() => {
                match obj {
                    Some(obj) => {
                        stats.received += 1;
                        let ts = obj.capture_timestamp_us;

                        if ts == 0 {
                            // No timestamp — pass through immediately
                            let _ = tx.try_send(obj);
                            stats.played += 1;
                            continue;
                        }

                        let now = protocol::now_us();

                        // Update clock offset
                        let instantaneous_offset =
                            now as i64 - ts as i64 + config.target_delay_us as i64;

                        match clock_offset_us {
                            None => {
                                clock_offset_us = Some(instantaneous_offset);
                                if is_clock_master {
                                    peer_clock.update(instantaneous_offset);
                                }
                            }
                            Some(ref mut offset) => {
                                if is_clock_master {
                                    *offset = ((*offset as f64) * (1.0 - config.drift_alpha)
                                        + (instantaneous_offset as f64) * config.drift_alpha)
                                        as i64;
                                    peer_clock.update(*offset);
                                } else {
                                    // Video: read from shared clock, but add our own target_delay difference
                                    if peer_clock.is_initialised() {
                                        *offset = peer_clock.offset_us()
                                            - audio_config().target_delay_us as i64
                                            + config.target_delay_us as i64;
                                    }
                                }
                            }
                        }

                        let offset = clock_offset_us.unwrap();

                        // Check if too late
                        let playout_us = (ts as i64 + offset) as u64;
                        if now > playout_us + config.max_late_us {
                            stats.dropped_late += 1;
                            if let Some(ref dc) = drop_counter { dc.increment(); }
                            tracing::trace!(
                                "{}: dropped late packet ({}ms late)",
                                config.label,
                                (now - playout_us) / 1000
                            );
                            continue;
                        }

                        // Check for duplicate / reorder
                        if let Some(last_ts) = last_emitted_ts {
                            if ts <= last_ts {
                                stats.dropped_duplicate += 1;
                                if let Some(ref dc) = drop_counter { dc.increment(); }
                                continue;
                            }
                        }
                        if queue.contains_key(&ts) {
                            stats.dropped_duplicate += 1;
                            if let Some(ref dc) = drop_counter { dc.increment(); }
                            continue;
                        }

                        queue.insert(ts, obj);
                    }
                    None => break,
                }
            }

            _ = tokio::time::sleep(sleep_dur) => {
                // Check if front of queue is ready
                let now = protocol::now_us();
                let mut emitted = false;

                while let Some((&ts, _)) = queue.first_key_value() {
                    if let Some(offset) = clock_offset_us {
                        let playout_us = (ts as i64 + offset) as u64;

                        if now >= playout_us {
                            // Check if too late even for playout
                            if now > playout_us + config.max_late_us {
                                queue.pop_first();
                                stats.dropped_late += 1;
                                if let Some(ref dc) = drop_counter { dc.increment(); }
                                continue;
                            }

                            let (_, obj) = queue.pop_first().unwrap();
                            last_emitted_ts = Some(ts);
                            stats.played += 1;
                            emitted = true;
                            if tx.try_send(obj).is_err() {
                                if let Some(ref dc) = drop_counter { dc.increment(); }
                                tracing::trace!("{}: downstream full, dropping", config.label);
                            }
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                // If playout time passed and queue was empty, emit PLC for audio
                if !emitted && emit_plc && clock_offset_us.is_some() {
                    if let Some(last_ts) = last_emitted_ts {
                        // Expected next audio frame is 20ms after last
                        let expected_next = last_ts + 20_000;
                        let offset = clock_offset_us.unwrap();
                        let expected_playout = (expected_next as i64 + offset) as u64;
                        if now >= expected_playout && now < expected_playout + config.max_late_us {
                            last_emitted_ts = Some(expected_next);
                            let _ = tx.try_send(plc_sentinel());
                        }
                    }
                }
            }
        }

        // Periodic stats logging
        if last_stats_log.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                "Jitter[{}]: rx={} played={} late={} dup={} queue={} offset={}ms",
                config.label,
                stats.received,
                stats.played,
                stats.dropped_late,
                stats.dropped_duplicate,
                queue.len(),
                clock_offset_us.unwrap_or(0) / 1000,
            );
            last_stats_log = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time;

    #[tokio::test]
    async fn test_packets_are_buffered_and_released_in_order() {
        let config = JitterBufferConfig {
            target_delay_us: 50_000, // 50ms
            max_late_us: 30_000,
            drift_alpha: 1.0, // instant adaptation for test
            label: "test",
        };

        let peer_clock = PeerClock::new();
        let (in_tx, in_rx) = mpsc::channel(32);
        let (out_tx, mut out_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let jb_cancel = cancel.clone();
        tokio::spawn(async move {
            run_jitter_buffer(config, peer_clock, true, false, in_rx, out_tx, None, jb_cancel).await;
        });

        let base_ts = protocol::now_us();

        // Send 3 packets with sequential timestamps
        for i in 0..3u64 {
            let ts = base_ts + i * 20_000;
            in_tx.send(MediaObject {
                group_id: i,
                subgroup_id: 0, object_id: 0,
                capture_timestamp_us: ts, codec: None,
                payload: SharedBuf::from_vec(vec![i as u8]),
            }).await.unwrap();
        }

        // Wait for target delay + some margin
        time::sleep(Duration::from_millis(120)).await;

        // Collect what came out
        let mut received = Vec::new();
        while let Ok(obj) = out_rx.try_recv() {
            received.push(obj.payload[0]);
        }

        assert!(!received.is_empty(), "Should have received some packets");
        // Packets should be in order
        for i in 1..received.len() {
            assert!(received[i] > received[i - 1], "Packets should be in order");
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_late_packets_are_dropped() {
        let config = JitterBufferConfig {
            target_delay_us: 10_000, // 10ms
            max_late_us: 5_000,      // 5ms
            drift_alpha: 1.0,
            label: "test-late",
        };

        let peer_clock = PeerClock::new();
        let (in_tx, in_rx) = mpsc::channel(32);
        let (out_tx, mut out_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();

        let jb_cancel = cancel.clone();
        tokio::spawn(async move {
            run_jitter_buffer(config, peer_clock, true, false, in_rx, out_tx, None, jb_cancel).await;
        });

        // First, send a "current" packet to establish the clock offset
        let now_ts = protocol::now_us();
        in_tx.send(MediaObject {
            group_id: 0,
            subgroup_id: 0, object_id: 0,
            capture_timestamp_us: now_ts, codec: None,
            payload: SharedBuf::from_vec(vec![1]),
        }).await.unwrap();

        // Wait for it to be processed and played out
        time::sleep(Duration::from_millis(30)).await;
        // Drain it
        let _ = out_rx.try_recv();

        // Now send a packet with a very old timestamp (500ms before the first)
        // This should be detected as late since the clock is already established
        let old_ts = now_ts - 500_000;
        in_tx.send(MediaObject {
            group_id: 1,
            subgroup_id: 0, object_id: 0,
            capture_timestamp_us: old_ts, codec: None,
            payload: SharedBuf::from_vec(vec![42]),
        }).await.unwrap();

        time::sleep(Duration::from_millis(50)).await;

        // Should not have received the late packet
        assert!(out_rx.try_recv().is_err(), "Late packet should have been dropped");

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_peer_clock_shared_between_audio_and_video() {
        let clock = PeerClock::new();
        assert!(!clock.is_initialised());

        clock.update(12345);
        assert!(clock.is_initialised());
        assert_eq!(clock.offset_us(), 12345);

        clock.update(-5000);
        assert_eq!(clock.offset_us(), -5000);
    }
}
