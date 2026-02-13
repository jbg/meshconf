use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Quality ladder
// ---------------------------------------------------------------------------

/// A quality preset defining encode resolution, frame rate, and target bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityPreset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// Available quality levels, ordered low → high.
pub const QUALITY_LADDER: &[QualityPreset] = &[
    QualityPreset { width: 320, height: 240, fps: 10, bitrate_bps: 100_000 },
    QualityPreset { width: 480, height: 360, fps: 15, bitrate_bps: 250_000 },
    QualityPreset { width: 640, height: 480, fps: 15, bitrate_bps: 500_000 },
];

const DEFAULT_QUALITY_IDX: usize = 2; // start at highest

// ---------------------------------------------------------------------------
// Bitrate bounds
// ---------------------------------------------------------------------------

const MIN_BITRATE_BPS: u32 = 50_000;
const MAX_BITRATE_BPS: u32 = 2_000_000;

// ---------------------------------------------------------------------------
// AIMD parameters
// ---------------------------------------------------------------------------

/// Additive increase: 5 % every probe interval.
const INCREASE_RATE: f64 = 0.05;
/// Probe (try to increase) every 2 s when no congestion detected.
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// After a decrease, hold the lower rate for this long before probing.
const HOLD_AFTER_DECREASE: Duration = Duration::from_secs(2);
/// RTT increase (over smoothed baseline) that triggers a mild reduction.
const RTT_INCREASE_THRESHOLD: f64 = 0.20;

// ---------------------------------------------------------------------------
// Events fed into the estimator
// ---------------------------------------------------------------------------

/// Events reported to the bandwidth estimator from various parts of the
/// send and receive pipeline.
pub enum BandwidthEvent {
    /// A video frame was fully written to a QUIC stream for one peer.
    SendComplete {
        duration: Duration,
        payload_bytes: usize,
    },
    /// Smoothed RTT sample from a peer connection.
    RttSample { rtt_us: u64 },
    /// The receiver stopped a stream (old GOP or congestion).  The
    /// dispatcher already stops old-GOP streams; this event is for
    /// *non-old* stops, which indicate real congestion.
    StreamStopped,
    /// Receiver-side feedback received from a remote peer describing how
    /// well our video is being received.  Modelled on RTCP Receiver
    /// Reports (RFC 3550 §6.4.1).
    ReceiverFeedback {
        /// Fraction of video frames lost in the reporting interval (0.0–1.0).
        fraction_lost: f64,
        /// Observed inter-arrival jitter in microseconds.
        jitter_us: u64,
    },
}

// ---------------------------------------------------------------------------
// Shared encoder configuration (lock-free, read by the blocking encode thread)
// ---------------------------------------------------------------------------

/// Shared, lock-free configuration written by the estimator and read by the
/// encode loop running on a blocking thread.
pub struct EncoderConfig {
    /// Target average bitrate in bits per second.
    pub target_bitrate: AtomicU32,
    /// Target encode width.
    pub target_width: AtomicU32,
    /// Target encode height.
    pub target_height: AtomicU32,
    /// Target encode frame rate.
    pub target_fps: AtomicU32,
    /// Set by the estimator when resolution or FPS changes; cleared by the
    /// encode loop after it has recreated the compression session.
    pub quality_changed: AtomicBool,
}

impl EncoderConfig {
    /// Create a new encoder config initialised to the highest quality preset.
    pub fn new() -> Arc<Self> {
        let preset = QUALITY_LADDER.last().unwrap();
        Arc::new(Self {
            target_bitrate: AtomicU32::new(preset.bitrate_bps),
            target_width: AtomicU32::new(preset.width),
            target_height: AtomicU32::new(preset.height),
            target_fps: AtomicU32::new(preset.fps),
            quality_changed: AtomicBool::new(false),
        })
    }
}

// ---------------------------------------------------------------------------
// Handle returned to callers (read-only view of estimator output)
// ---------------------------------------------------------------------------

/// Read-only handle to the estimator's output, given to the encoder.
pub struct BandwidthHandle {
    pub config: Arc<EncoderConfig>,
}

// ---------------------------------------------------------------------------
// Congestion severity (internal)
// ---------------------------------------------------------------------------

enum CongestionLevel {
    None,
    /// 15 % reduction.
    Mild,
    /// 20 % reduction.
    Moderate,
    /// 50 % reduction.
    Severe,
}

// ---------------------------------------------------------------------------
// Bandwidth estimator
// ---------------------------------------------------------------------------

/// Adaptive bitrate estimator.
///
/// Runs as an async task, consuming [`BandwidthEvent`]s and updating the
/// shared [`EncoderConfig`] atomics that the encode loop reads.
pub struct BandwidthEstimator {
    event_rx: mpsc::Receiver<BandwidthEvent>,
    config: Arc<EncoderConfig>,

    // Internal state
    current_bitrate: u32,
    quality_idx: usize,
    baseline_rtt_us: Option<f64>,
    smoothed_rtt_us: Option<f64>,
    last_probe: Instant,
    last_decrease: Instant,
    frame_interval_ms: f64,
}

impl BandwidthEstimator {
    /// Create a new estimator.  Returns:
    /// - `mpsc::Sender<BandwidthEvent>` — clone and hand to anything that
    ///   produces congestion signals.
    /// - `BandwidthHandle` — give to the encoder so it can read the config.
    /// - `BandwidthEstimator` — spawn with [`run`].
    /// Create a new estimator bound to the given shared encoder config.
    ///
    /// The same `Arc<EncoderConfig>` should be given to
    /// [`VideoEncoder::start`] so the encode loop can read the values
    /// the estimator writes.
    pub fn new(config: Arc<EncoderConfig>) -> (mpsc::Sender<BandwidthEvent>, BandwidthHandle, Self) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let initial = QUALITY_LADDER[DEFAULT_QUALITY_IDX];

        let handle = BandwidthHandle {
            config: config.clone(),
        };

        let estimator = Self {
            event_rx,
            config,
            current_bitrate: initial.bitrate_bps,
            quality_idx: DEFAULT_QUALITY_IDX,
            baseline_rtt_us: None,
            smoothed_rtt_us: None,
            last_probe: Instant::now(),
            last_decrease: Instant::now(),
            frame_interval_ms: 1000.0 / initial.fps as f64,
        };

        (event_tx, handle, estimator)
    }

    /// Run the estimator until cancelled.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = self.event_rx.recv() => {
                    match event {
                        Some(e) => self.handle_event(e),
                        None => break,
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Event handling
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: BandwidthEvent) {
        let congestion = match event {
            BandwidthEvent::SendComplete { duration, payload_bytes: _ } => {
                let send_ms = duration.as_secs_f64() * 1000.0;
                if send_ms > self.frame_interval_ms {
                    CongestionLevel::Moderate
                } else {
                    CongestionLevel::None
                }
            }

            BandwidthEvent::RttSample { rtt_us } => {
                let rtt = rtt_us as f64;
                match self.smoothed_rtt_us {
                    None => {
                        self.smoothed_rtt_us = Some(rtt);
                        self.baseline_rtt_us = Some(rtt);
                        CongestionLevel::None
                    }
                    Some(ref mut smoothed) => {
                        *smoothed = *smoothed * 0.9 + rtt * 0.1;
                        if let Some(ref mut baseline) = self.baseline_rtt_us {
                            if *smoothed > *baseline * (1.0 + RTT_INCREASE_THRESHOLD) {
                                CongestionLevel::Mild
                            } else {
                                // Slowly adapt baseline if RTT improves.
                                *baseline = *baseline * 0.999 + *smoothed * 0.001;
                                CongestionLevel::None
                            }
                        } else {
                            CongestionLevel::None
                        }
                    }
                }
            }

            BandwidthEvent::StreamStopped => CongestionLevel::Severe,

            BandwidthEvent::ReceiverFeedback { fraction_lost, jitter_us: _ } => {
                if fraction_lost > 0.10 {
                    CongestionLevel::Severe
                } else if fraction_lost > 0.05 {
                    CongestionLevel::Moderate
                } else if fraction_lost > 0.02 {
                    CongestionLevel::Mild
                } else {
                    CongestionLevel::None
                }
            }
        };

        match congestion {
            CongestionLevel::None => self.try_probe(),
            CongestionLevel::Mild => self.decrease(0.85),
            CongestionLevel::Moderate => self.decrease(0.80),
            CongestionLevel::Severe => self.decrease(0.50),
        }
    }

    // -----------------------------------------------------------------------
    // Additive increase
    // -----------------------------------------------------------------------

    fn try_probe(&mut self) {
        if self.last_probe.elapsed() >= PROBE_INTERVAL
            && self.last_decrease.elapsed() >= HOLD_AFTER_DECREASE
        {
            self.last_probe = Instant::now();
            let new = ((self.current_bitrate as f64) * (1.0 + INCREASE_RATE)) as u32;
            self.set_bitrate(new.min(MAX_BITRATE_BPS));
            self.maybe_step_up_quality();
        }
    }

    // -----------------------------------------------------------------------
    // Multiplicative decrease
    // -----------------------------------------------------------------------

    fn decrease(&mut self, factor: f64) {
        self.last_decrease = Instant::now();
        let new = ((self.current_bitrate as f64) * factor) as u32;
        self.set_bitrate(new.max(MIN_BITRATE_BPS));
        self.maybe_step_down_quality();
        tracing::info!(
            "ABR: decreased to {}kbps (factor={:.2})",
            self.current_bitrate / 1000,
            factor,
        );
    }

    // -----------------------------------------------------------------------
    // Bitrate + quality helpers
    // -----------------------------------------------------------------------

    fn set_bitrate(&mut self, bps: u32) {
        self.current_bitrate = bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS);
        self.config
            .target_bitrate
            .store(self.current_bitrate, Ordering::Relaxed);
    }

    fn apply_preset(&mut self, preset: &QualityPreset) {
        self.frame_interval_ms = 1000.0 / preset.fps as f64;
        self.config.target_width.store(preset.width, Ordering::Relaxed);
        self.config.target_height.store(preset.height, Ordering::Relaxed);
        self.config.target_fps.store(preset.fps, Ordering::Relaxed);
        self.config.quality_changed.store(true, Ordering::Release);
    }

    fn maybe_step_down_quality(&mut self) {
        if self.quality_idx == 0 {
            return;
        }
        let lower = &QUALITY_LADDER[self.quality_idx - 1];
        if self.current_bitrate <= lower.bitrate_bps {
            self.quality_idx -= 1;
            self.apply_preset(lower);
            tracing::info!(
                "ABR: quality stepped down to {}x{} @ {}fps",
                lower.width, lower.height, lower.fps,
            );
        }
    }

    fn maybe_step_up_quality(&mut self) {
        if self.quality_idx + 1 >= QUALITY_LADDER.len() {
            return;
        }
        let higher = &QUALITY_LADDER[self.quality_idx + 1];
        if self.current_bitrate >= higher.bitrate_bps {
            self.quality_idx += 1;
            self.apply_preset(higher);
            tracing::info!(
                "ABR: quality stepped up to {}x{} @ {}fps",
                higher.width, higher.height, higher.fps,
            );
        }
    }
}
