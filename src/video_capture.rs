use anyhow::{anyhow, Result};
use camera_stream::{
    CameraDevice, CameraManager, CameraStream, PixelFormat, Ratio,
    StreamConfig,
};
use camera_stream::platform::macos::device::MacosCameraManager;

use objc2_core_foundation::*;
use objc2_core_video::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Target encode resolution.
pub const ENCODE_WIDTH: u32 = 640;
pub const ENCODE_HEIGHT: u32 = 480;

/// A captured video frame backed by a retained NV12 `CVPixelBuffer`.
///
/// The pixel buffer comes directly from the camera (or is created by the test
/// pattern generator) and can be passed to both the encoder and the preview
/// path without any pixel-format conversion.
pub struct Frame {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    pub width: u32,
    pub height: u32,
    /// When this frame was captured (monotonic, for local latency measurement).
    pub captured_at: std::time::Instant,
    /// Wall-clock capture time in microseconds since Unix epoch (for LOC timestamp).
    pub capture_timestamp_us: u64,
}

// CVPixelBuffer is refcounted and thread-safe.
unsafe impl Send for Frame {}

/// Holds the camera capture thread handle.
pub struct VideoCapture {
    _handle: std::thread::JoinHandle<()>,
}

impl VideoCapture {
    /// Start capturing from the default camera.
    ///
    /// Returns a handle and a receiver of NV12 frames. When the camera's
    /// native resolution matches `ENCODE_WIDTH×ENCODE_HEIGHT` the pixel
    /// buffer is passed through zero-copy; otherwise it is scaled with vImage.
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

            let formats: Vec<_> = formats.collect();

            for f in &formats {
                tracing::info!(
                    "  Format: {:?} {}x{} rates: {:?}",
                    f.pixel_format, f.size.width, f.size.height,
                    f.frame_rate_ranges(),
                );
            }

            let best = formats
                .iter()
                .filter(|f| {
                    f.pixel_format == PixelFormat::Nv12
                        && f.size.width >= ENCODE_WIDTH
                        && f.size.height >= ENCODE_HEIGHT
                })
                .min_by_key(|f| {
                    let pixels = f.size.width as u64 * f.size.height as u64;
                    let best_fps_diff = f
                        .frame_rate_ranges()
                        .iter()
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
                    size: fmt.size,
                    frame_rate: Ratio {
                        numerator: 15,
                        denominator: 1,
                    },
                },
                None => {
                    let fallback = formats.iter().find(|f| f.pixel_format == PixelFormat::Nv12);
                    match fallback {
                        Some(fmt) => StreamConfig {
                            pixel_format: PixelFormat::Nv12,
                            size: fmt.size,
                            frame_rate: Ratio {
                                numerator: 15,
                                denominator: 1,
                            },
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
                config.size.width,
                config.size.height,
                config.frame_rate.numerator / config.frame_rate.denominator,
            );

            let src_w = config.size.width;
            let src_h = config.size.height;
            let needs_resize = src_w != ENCODE_WIDTH || src_h != ENCODE_HEIGHT;

            // Pre-allocate resize buffers and pool (only used when camera != encode resolution).
            let ew = ENCODE_WIDTH as usize;
            let eh = ENCODE_HEIGHT as usize;
            let mut resize_y: Vec<u8> = if needs_resize { vec![0u8; ew * eh] } else { vec![] };
            let mut resize_cbcr: Vec<u8> = if needs_resize { vec![0u8; ew * (eh / 2)] } else { vec![] };
            let resize_pool = if needs_resize {
                match Nv12PixelBufferPool::new(ENCODE_WIDTH, ENCODE_HEIGHT) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        let _ = init_tx.send(Err(anyhow!("Failed to create resize pixel buffer pool: {e}")));
                        return;
                    }
                }
            } else {
                None
            };

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
                let capture_timestamp_us = crate::protocol::now_us();

                if needs_resize {
                    // Scale NV12 planes via vImage into pre-allocated buffers,
                    // then create a new CVPixelBuffer at the encode resolution.
                    let pb = frame.pixel_buffer_ref();
                    let y_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
                    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
                    let cbcr_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *const u8;
                    let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);

                    if let Err(e) = crate::vimage::scale_nv12(
                        y_ptr,
                        y_stride,
                        cbcr_ptr,
                        cbcr_stride,
                        src_w as usize,
                        src_h as usize,
                        &mut resize_y,
                        &mut resize_cbcr,
                        ew,
                        eh,
                    ) {
                        tracing::warn!("vImage NV12 scale failed: {e}");
                        return;
                    }

                    match resize_pool.as_ref().unwrap().create_filled(
                        &resize_y,
                        &resize_cbcr,
                    ) {
                        Ok(pb) => {
                            let _ = tx.try_send(Frame {
                                pixel_buffer: pb,
                                width: ENCODE_WIDTH,
                                height: ENCODE_HEIGHT,
                                captured_at,
                                capture_timestamp_us,
                            });
                        }
                        Err(e) => {
                            tracing::warn!("Failed to create resized NV12 pixel buffer: {e}");
                        }
                    }
                } else {
                    // No resize needed — retain the camera's CVPixelBuffer directly.
                    let pb_ptr = std::ptr::NonNull::from(frame.pixel_buffer_ref());
                    // SAFETY: the CVPixelBuffer is valid for the callback scope;
                    // retain bumps the refcount so it survives beyond it.
                    let retained = unsafe { CFRetained::retain(pb_ptr) };
                    let _ = tx.try_send(Frame {
                        pixel_buffer: retained,
                        width: src_w,
                        height: src_h,
                        captured_at,
                        capture_timestamp_us,
                    });
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

/// Tee a capture frame receiver into two: one for the encoder and one for
/// local preview. The local preview path converts NV12→packed u32 via vImage
/// and sends `(peer_id, RgbFrame)` to the compositor.
///
/// Returns the new receiver that should be given to the `VideoEncoder`.
pub fn tee_capture(
    mut src: mpsc::Receiver<Frame>,
    peer_id: crate::room::PeerId,
    compositor_tx: mpsc::Sender<(crate::room::PeerId, crate::video_decode::RgbFrame)>,
    cancel: tokio_util::sync::CancellationToken,
) -> mpsc::Receiver<Frame> {
    let (enc_tx, enc_rx) = mpsc::channel::<Frame>(2);

    tokio::task::spawn_blocking(move || {
        let converter = crate::vimage::Nv12Converter::new_bt601_video_range()
            .expect("failed to create vImage NV12 converter");
        let rgb_pool = crate::pool::BufPool::<u32>::new();

        while !cancel.is_cancelled() {
            let frame = match src.blocking_recv() {
                Some(f) => f,
                None => break,
            };

            let w = frame.width as usize;
            let h = frame.height as usize;
            let mut pixel_buf = rgb_pool.checkout(w * h);

            // Lock pixel buffer and convert NV12→packed u32 via vImage.
            unsafe {
                CVPixelBufferLockBaseAddress(&frame.pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
            }

            let y_ptr = CVPixelBufferGetBaseAddressOfPlane(&frame.pixel_buffer, 0) as *const u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&frame.pixel_buffer, 0);
            let cbcr_ptr = CVPixelBufferGetBaseAddressOfPlane(&frame.pixel_buffer, 1) as *const u8;
            let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(&frame.pixel_buffer, 1);

            let convert_ok = converter
                .nv12_to_packed_u32(w, h, y_ptr, y_stride, cbcr_ptr, cbcr_stride, &mut pixel_buf)
                .is_ok();

            unsafe {
                CVPixelBufferUnlockBaseAddress(
                    &frame.pixel_buffer,
                    CVPixelBufferLockFlags::ReadOnly,
                );
            }

            if convert_ok {
                let _ = compositor_tx.try_send((
                    peer_id,
                    crate::video_decode::RgbFrame {
                        group_id: 0,
                        capture_timestamp_us: frame.capture_timestamp_us,
                        width: frame.width,
                        height: frame.height,
                        data: pixel_buf.share(),
                    },
                ));
            }

            // Forward the NV12 pixel buffer to the encoder.
            let _ = enc_tx.try_send(frame);
        }
    });

    enc_rx
}

// ---------------------------------------------------------------------------
// NV12 CVPixelBuffer pool
// ---------------------------------------------------------------------------

/// A pool of NV12 CVPixelBuffers at a fixed resolution.
///
/// Reuses buffers instead of allocating/deallocating each frame, avoiding
/// per-frame kernel round-trips.
pub struct Nv12PixelBufferPool {
    pool: CFRetained<CVPixelBufferPool>,
    width: u32,
    height: u32,
}

// CVPixelBufferPool is thread-safe per Apple documentation.
unsafe impl Send for Nv12PixelBufferPool {}

impl Nv12PixelBufferPool {
    /// Create a pool that vends NV12 pixel buffers at the given resolution.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        use objc2_core_foundation::{CFDictionary, CFNumber, CFString};

        let w_key: &CFString = unsafe {
            &*(kCVPixelBufferWidthKey as *const _ as *const CFString)
        };
        let h_key: &CFString = unsafe {
            &*(kCVPixelBufferHeightKey as *const _ as *const CFString)
        };
        let fmt_key: &CFString = unsafe {
            &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const CFString)
        };

        let w_val = CFNumber::new_i32(width as i32);
        let h_val = CFNumber::new_i32(height as i32);
        let fmt_val = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);

        let keys: [*const std::ffi::c_void; 3] = [
            (w_key as *const CFString).cast(),
            (h_key as *const CFString).cast(),
            (fmt_key as *const CFString).cast(),
        ];
        let values: [*const std::ffi::c_void; 3] = [
            (&*w_val as *const CFNumber).cast(),
            (&*h_val as *const CFNumber).cast(),
            (&*fmt_val as *const CFNumber).cast(),
        ];

        let pb_attrs = unsafe {
            CFDictionary::new(
                None,
                keys.as_ptr() as *mut _,
                values.as_ptr() as *mut _,
                3,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .ok_or_else(|| anyhow!("Failed to create pixel buffer attributes dict"))?
        };

        let mut pool_ptr: *mut CVPixelBufferPool = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                Some(&pb_attrs),
                std::ptr::NonNull::new(&mut pool_ptr).unwrap(),
            )
        };
        if status != 0 || pool_ptr.is_null() {
            return Err(anyhow!(
                "CVPixelBufferPoolCreate failed: {status}"
            ));
        }

        let pool = unsafe {
            CFRetained::from_raw(std::ptr::NonNull::new(pool_ptr).unwrap())
        };

        Ok(Self { pool, width, height })
    }

    /// Get a pixel buffer from the pool and fill it with the given Y + CbCr data.
    pub fn create_filled(
        &self,
        y: &[u8],
        cbcr: &[u8],
    ) -> Result<CFRetained<CVPixelBuffer>> {
        let w = self.width as usize;
        let h = self.height as usize;

        let mut pb_ptr: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(
                None,
                &self.pool,
                std::ptr::NonNull::new(&mut pb_ptr).unwrap(),
            )
        };
        if status != 0 || pb_ptr.is_null() {
            return Err(anyhow!(
                "CVPixelBufferPoolCreatePixelBuffer failed: {status}"
            ));
        }

        let pb = unsafe {
            CFRetained::from_raw(std::ptr::NonNull::new(pb_ptr).unwrap())
        };

        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));

            // Y plane
            let y_base = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
            for row in 0..h {
                std::ptr::copy_nonoverlapping(
                    y[row * w..].as_ptr(),
                    y_base.add(row * y_stride),
                    w,
                );
            }

            // CbCr plane
            let cbcr_base = CVPixelBufferGetBaseAddressOfPlane(&pb, 1) as *mut u8;
            let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 1);
            let half_h = h / 2;
            let cbcr_row_bytes = w; // w/2 pairs × 2 bytes each
            for row in 0..half_h {
                std::ptr::copy_nonoverlapping(
                    cbcr[row * cbcr_row_bytes..].as_ptr(),
                    cbcr_base.add(row * cbcr_stride),
                    cbcr_row_bytes,
                );
            }

            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        }

        Ok(pb)
    }
}

// ---------------------------------------------------------------------------
// Test pattern generator
// ---------------------------------------------------------------------------

/// Start a test pattern generator that produces colour bars as NV12 frames.
pub fn start_test_pattern(
    cancel: CancellationToken,
    max_fps: Option<u32>,
) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel::<Frame>(2);
    let ew = ENCODE_WIDTH as usize;
    let eh = ENCODE_HEIGHT as usize;

    std::thread::spawn(move || {
        let pool = Nv12PixelBufferPool::new(ENCODE_WIDTH, ENCODE_HEIGHT)
            .expect("failed to create test pattern pixel buffer pool");
        let target_interval =
            max_fps.map(|fps| std::time::Duration::from_secs_f64(1.0 / fps as f64));
        let mut frame_num: u32 = 0;

        let mut y_buf = vec![0u8; ew * eh];
        let mut cbcr_buf = vec![0u8; ew * (eh / 2)];

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

            // Generate Y plane + NV12 CbCr plane directly.
            let half_w = ew / 2;
            for row in 0..eh {
                for col in 0..ew {
                    let shifted = (col + offset) % ew;
                    let bar_idx = shifted * bars.len() / ew;
                    let (r, g, b) = bars[bar_idx];
                    let (r, g, b) = (r as i32, g as i32, b as i32);
                    let yval = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    y_buf[row * ew + col] = yval.clamp(0, 255) as u8;
                }
                // CbCr for even rows
                if row & 1 == 0 {
                    let cbcr_row = row / 2;
                    for col_pair in 0..half_w {
                        let col = col_pair * 2;
                        let shifted = (col + offset) % ew;
                        let bar_idx = shifted * bars.len() / ew;
                        let (r, g, b) = bars[bar_idx];
                        let (r, g, b) = (r as i32, g as i32, b as i32);
                        let cb =
                            (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                        let cr =
                            (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                        let idx = cbcr_row * ew + col_pair * 2;
                        cbcr_buf[idx] = cb;
                        cbcr_buf[idx + 1] = cr;
                    }
                }
            }

            match pool.create_filled(&y_buf, &cbcr_buf) {
                Ok(pb) => {
                    let _ = tx.try_send(Frame {
                        pixel_buffer: pb,
                        width: ENCODE_WIDTH,
                        height: ENCODE_HEIGHT,
                        captured_at: std::time::Instant::now(),
                        capture_timestamp_us: crate::protocol::now_us(),
                    });
                }
                Err(e) => {
                    tracing::warn!("Test pattern pixel buffer creation failed: {e}");
                }
            }

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
