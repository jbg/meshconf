use anyhow::{anyhow, Result};
use camera_stream::{
    CameraDevice, CameraManager, CameraStream, Frame as CsFrame, PixelFormat,
    StreamConfig, FrameRate,
};
use camera_stream::platform::macos::device::MacosCameraManager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Target encode resolution.
pub const ENCODE_WIDTH: u32 = 640;
pub const ENCODE_HEIGHT: u32 = 480;

/// An I420 (YUV 4:2:0) video frame.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// When this frame was captured.
    pub captured_at: std::time::Instant,
}

/// Holds the camera capture thread handle.
pub struct VideoCapture {
    _handle: std::thread::JoinHandle<()>,
}

impl VideoCapture {
    /// Start capturing from the default camera.
    /// Returns a handle and a receiver of I420 frames at ENCODE_WIDTH×ENCODE_HEIGHT.
    pub fn start(cancel: CancellationToken) -> Result<(Self, mpsc::Receiver<Frame>)> {
        let (tx, rx) = mpsc::channel::<Frame>(2);
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();

        let handle = std::thread::spawn(move || {
            let manager = MacosCameraManager;

            let device = match manager.default_device() {
                Ok(Some(d)) => d,
                Ok(None) => {
                    let _ = init_tx.send(Err(anyhow!("No camera found")));
                    return;
                }
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("Failed to discover cameras: {}", e)));
                    return;
                }
            };

            tracing::info!("Camera: {}", device.name());

            // Find the best NV12 format: smallest resolution >= encode target, closest to 15fps.
            let formats = match device.supported_formats() {
                Ok(f) => f,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("Failed to get formats: {}", e)));
                    return;
                }
            };

            for f in &formats {
                tracing::info!(
                    "  Format: {:?} {}x{} rates: {:?}",
                    f.pixel_format, f.resolution.width, f.resolution.height,
                    f.frame_rate_ranges,
                );
            }

            let best = formats
                .iter()
                .filter(|f| {
                    f.pixel_format == PixelFormat::Nv12
                        && f.resolution.width >= ENCODE_WIDTH
                        && f.resolution.height >= ENCODE_HEIGHT
                })
                .min_by_key(|f| {
                    let pixels = f.resolution.width as u64 * f.resolution.height as u64;
                    // Find closest max fps to 15
                    let best_fps_diff = f.frame_rate_ranges.iter()
                        .map(|r| {
                            let max_fps = r.max.as_f64();
                            ((max_fps - 15.0).abs() * 1000.0) as u64
                        })
                        .min()
                        .unwrap_or(u64::MAX);
                    (pixels, best_fps_diff)
                });

            let config = match best {
                Some(fmt) => StreamConfig {
                    pixel_format: PixelFormat::Nv12,
                    resolution: fmt.resolution,
                    frame_rate: FrameRate { numerator: 15, denominator: 1 },
                },
                None => {
                    // Fallback: try any NV12 format
                    let fallback = formats.iter().find(|f| f.pixel_format == PixelFormat::Nv12);
                    match fallback {
                        Some(fmt) => StreamConfig {
                            pixel_format: PixelFormat::Nv12,
                            resolution: fmt.resolution,
                            frame_rate: FrameRate { numerator: 15, denominator: 1 },
                        },
                        None => {
                            let _ = init_tx.send(Err(anyhow!("No NV12 camera format found")));
                            return;
                        }
                    }
                }
            };

            tracing::info!(
                "Using format: {:?} {}x{} @ {}fps",
                config.pixel_format,
                config.resolution.width,
                config.resolution.height,
                config.frame_rate.numerator / config.frame_rate.denominator,
            );

            let src_w = config.resolution.width as usize;
            let src_h = config.resolution.height as usize;
            let ew = ENCODE_WIDTH as usize;
            let eh = ENCODE_HEIGHT as usize;
            let needs_resize = src_w != ew || src_h != eh;

            let mut stream = match device.open(&config) {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("Failed to open camera stream: {}", e)));
                    return;
                }
            };

            let cancel_clone = cancel.clone();
            let result = stream.start(move |frame| {
                if cancel_clone.is_cancelled() {
                    return;
                }

                let captured_at = std::time::Instant::now();
                let res = frame.resolution();
                let planes = frame.planes();

                if planes.len() < 2 {
                    return;
                }

                let fw = res.width as usize;
                let fh = res.height as usize;

                // NV12: plane 0 = Y, plane 1 = interleaved UV
                let y_plane = &planes[0];
                let uv_plane = &planes[1];

                if needs_resize {
                    // Extract full Y/U/V from NV12 at source resolution, then
                    // downsample to encode resolution.
                    let mut y_src = vec![0u8; fw * fh];
                    let mut u_src = vec![0u8; (fw / 2) * (fh / 2)];
                    let mut v_src = vec![0u8; (fw / 2) * (fh / 2)];

                    // Copy Y with stride handling
                    for row in 0..fh {
                        let src_start = row * y_plane.bytes_per_row;
                        let dst_start = row * fw;
                        y_src[dst_start..dst_start + fw]
                            .copy_from_slice(&y_plane.data[src_start..src_start + fw]);
                    }

                    // De-interleave UV
                    let half_w = fw / 2;
                    let half_h = fh / 2;
                    for row in 0..half_h {
                        let uv_start = row * uv_plane.bytes_per_row;
                        let dst_start = row * half_w;
                        for col in 0..half_w {
                            u_src[dst_start + col] = uv_plane.data[uv_start + col * 2];
                            v_src[dst_start + col] = uv_plane.data[uv_start + col * 2 + 1];
                        }
                    }

                    // Nearest-neighbor resize each plane
                    let mut y_buf = vec![0u8; ew * eh];
                    let mut u_buf = vec![0u8; (ew / 2) * (eh / 2)];
                    let mut v_buf = vec![0u8; (ew / 2) * (eh / 2)];

                    nn_resize(&y_src, fw, fh, &mut y_buf, ew, eh);
                    nn_resize(&u_src, half_w, half_h, &mut u_buf, ew / 2, eh / 2);
                    nn_resize(&v_src, half_w, half_h, &mut v_buf, ew / 2, eh / 2);

                    let frame = Frame {
                        width: ENCODE_WIDTH,
                        height: ENCODE_HEIGHT,
                        y: y_buf,
                        u: u_buf,
                        v: v_buf,
                        captured_at,
                    };
                    let _ = tx.try_send(frame);
                } else {
                    // No resize needed — just convert NV12 → I420
                    let mut y_buf = vec![0u8; ew * eh];
                    let mut u_buf = vec![0u8; (ew / 2) * (eh / 2)];
                    let mut v_buf = vec![0u8; (ew / 2) * (eh / 2)];

                    for row in 0..eh {
                        let src_start = row * y_plane.bytes_per_row;
                        let dst_start = row * ew;
                        y_buf[dst_start..dst_start + ew]
                            .copy_from_slice(&y_plane.data[src_start..src_start + ew]);
                    }

                    let half_w = ew / 2;
                    let half_h = eh / 2;
                    for row in 0..half_h {
                        let uv_start = row * uv_plane.bytes_per_row;
                        let dst_start = row * half_w;
                        for col in 0..half_w {
                            u_buf[dst_start + col] = uv_plane.data[uv_start + col * 2];
                            v_buf[dst_start + col] = uv_plane.data[uv_start + col * 2 + 1];
                        }
                    }

                    let frame = Frame {
                        width: ENCODE_WIDTH,
                        height: ENCODE_HEIGHT,
                        y: y_buf,
                        u: u_buf,
                        v: v_buf,
                        captured_at,
                    };
                    let _ = tx.try_send(frame);
                }
            });

            match result {
                Ok(()) => {
                    let _ = init_tx.send(Ok(()));
                }
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("Failed to start camera: {}", e)));
                    return;
                }
            }

            // Keep thread alive until cancelled (stream runs on its own dispatch queue)
            while !cancel.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            let _ = stream.stop();
        });

        init_rx
            .recv()
            .map_err(|_| anyhow!("Camera thread died during init"))??;

        Ok((VideoCapture { _handle: handle }, rx))
    }
}

/// Nearest-neighbor resize for a single-channel plane.
#[inline]
fn nn_resize(
    src: &[u8], sw: usize, sh: usize,
    dst: &mut [u8], dw: usize, dh: usize,
) {
    for dy in 0..dh {
        let sy = dy * sh / dh;
        for dx in 0..dw {
            let sx = dx * sw / dw;
            dst[dy * dw + dx] = src[sy * sw + sx];
        }
    }
}

/// Start a test pattern generator that produces colour bars.
pub fn start_test_pattern(cancel: CancellationToken, max_fps: Option<u32>) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel::<Frame>(2);
    let ew = ENCODE_WIDTH as usize;
    let eh = ENCODE_HEIGHT as usize;

    std::thread::spawn(move || {
        let target_interval = max_fps.map(|fps| std::time::Duration::from_secs_f64(1.0 / fps as f64));
        let mut frame_num: u32 = 0;

        let mut rgb = vec![0u8; ew * eh * 3];
        let mut y_buf = vec![0u8; ew * eh];
        let mut u_buf = vec![0u8; (ew / 2) * (eh / 2)];
        let mut v_buf = vec![0u8; (ew / 2) * (eh / 2)];

        let bars: [(u8, u8, u8); 8] = [
            (192, 192, 192),
            (192, 192, 0),
            (0, 192, 192),
            (0, 192, 0),
            (192, 0, 192),
            (192, 0, 0),
            (0, 0, 192),
            (0, 0, 0),
        ];

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let start = std::time::Instant::now();

            let offset = (frame_num as usize * 2) % ew;

            for row in 0..eh {
                for col in 0..ew {
                    let shifted = (col + offset) % ew;
                    let bar_idx = shifted * bars.len() / ew;
                    let (r, g, b) = bars[bar_idx];
                    let idx = (row * ew + col) * 3;
                    rgb[idx] = r;
                    rgb[idx + 1] = g;
                    rgb[idx + 2] = b;
                }
            }

            rgb_to_i420_fast(ew, eh, &rgb, &mut y_buf, &mut u_buf, &mut v_buf);

            let frame = Frame {
                width: ENCODE_WIDTH,
                height: ENCODE_HEIGHT,
                y: y_buf.clone(),
                u: u_buf.clone(),
                v: v_buf.clone(),
                captured_at: std::time::Instant::now(),
            };
            let _ = tx.try_send(frame);

            frame_num = frame_num.wrapping_add(1);

            if let Some(interval) = target_interval {
                let elapsed = start.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }
        }
    });

    rx
}

/// Convert RGB24 to I420 using fixed-point integer arithmetic.
#[inline]
fn rgb_to_i420_fast(w: usize, h: usize, rgb: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    let half_w = w / 2;

    for row in 0..h {
        let row_offset = row * w;
        let uv_row = row / 2;
        let is_even_row = row & 1 == 0;

        for col in 0..w {
            let idx = (row_offset + col) * 3;
            let r = rgb[idx] as i32;
            let g = rgb[idx + 1] as i32;
            let b = rgb[idx + 2] as i32;

            let yval = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y[row_offset + col] = yval.clamp(0, 255) as u8;

            if is_even_row && (col & 1 == 0) {
                let uv_idx = uv_row * half_w + (col >> 1);
                u[uv_idx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                v[uv_idx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        }
    }
}
