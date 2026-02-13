use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};
use objc2_core_foundation::*;
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_video_toolbox::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::bandwidth::{BandwidthEvent, EncoderConfig, QUALITY_LADDER};
use crate::broadcaster::Broadcaster;
use crate::pool::{BufPool, SharedBuf};
use crate::protocol;
use crate::room::PeerId;
use crate::video_capture::Frame;

/// An encoded packet with its capture timestamp.
///
/// The payload is backed by a [`SharedBuf`] from a pool — cloning is a
/// refcount bump, and when the last reference drops the buffer returns to
/// the pool (not freed).
pub struct EncodedPacket {
    pub data: SharedBuf<u8>,
    /// Wall-clock capture time in microseconds since Unix epoch.
    pub capture_timestamp_us: u64,
    /// Whether this packet contains a keyframe (independently decodable).
    pub is_keyframe: bool,
}

/// A persistent video encode pipeline.
///
/// Uses macOS VideoToolbox hardware HEVC encoder for low-latency encoding.
/// Use `send_loop` to broadcast to all peers via a `Broadcaster`.
pub struct VideoEncoder {
    /// Encoded HEVC packets produced by the encode thread.
    packet_rx: mpsc::Receiver<EncodedPacket>,
    /// Signal the encode thread to emit a keyframe on the next frame.
    force_keyframe: Arc<AtomicBool>,
    /// Keep the handle alive so the thread isn't detached.
    _encode_handle: JoinHandle<()>,
}

impl VideoEncoder {
    /// Spawn the encode thread. Consumes the capture receiver permanently.
    ///
    /// If `encoder_config` is `None`, a fixed 500 kbps configuration is used
    /// (backwards-compatible with the pre-ABR behaviour).
    pub fn start(
        rx: mpsc::Receiver<Frame>,
        encoder_config: Option<Arc<EncoderConfig>>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let (packet_tx, packet_rx) = mpsc::channel::<EncodedPacket>(4);
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let fk = force_keyframe.clone();

        let encode_handle = tokio::task::spawn_blocking(move || {
            encode_loop(rx, packet_tx, fk, encoder_config, cancel);
        });

        Ok(Self {
            packet_rx,
            force_keyframe,
            _encode_handle: encode_handle,
        })
    }

    /// Get a clone of the force_keyframe flag (for the Broadcaster to trigger keyframes).
    pub fn force_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.force_keyframe.clone()
    }

    /// Broadcast encoded packets to all peers via a Broadcaster. Runs until cancelled.
    ///
    /// When `bw_tx` is provided, send completion timings and periodic RTT
    /// samples are reported to the bandwidth estimator.
    pub async fn send_loop(
        mut self,
        broadcaster: Broadcaster,
        bw_tx: Option<mpsc::Sender<BandwidthEvent>>,
        cancel: CancellationToken,
    ) {
        self.force_keyframe.store(true, Ordering::Relaxed);

        let mut frame_seq: u64 = 0;
        let mut in_flight: HashMap<PeerId, Vec<(u64, JoinHandle<()>)>> = HashMap::new();
        let mut current_gop: u64 = 0;
        let mut rtt_poll_counter: u64 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                packet = self.packet_rx.recv() => {
                    match packet {
                        Some(pkt) => {
                            if broadcaster.peer_count() == 0 {
                                abort_all(&mut in_flight);
                                continue;
                            }

                            frame_seq += 1;
                            let subgroup_id = if pkt.is_keyframe { 0u64 } else { 1u64 };

                            if pkt.is_keyframe {
                                current_gop += 1;
                                tracing::info!(
                                    "Sending keyframe: seq={} gop={} payload={}B to {} peer(s)",
                                    frame_seq, current_gop, pkt.data.len(),
                                    broadcaster.peer_count(),
                                );
                                abort_all(&mut in_flight);
                            }

                            for tasks in in_flight.values_mut() {
                                tasks.retain(|(_, h)| !h.is_finished());
                            }

                            let peers = broadcaster.peers();
                            let payload = pkt.data;

                            for (peer_id, conn) in &peers {
                                let payload = payload.clone(); // SharedBuf: refcount bump only
                                let group_id = frame_seq;
                                let capture_ts = pkt.capture_timestamp_us;
                                let quic_priority = frame_seq.min(i32::MAX as u64) as i32;
                                let bw = bw_tx.clone();
                                let payload_len = payload.len();
                                let conn = conn.clone();
                                let peer_id = *peer_id;

                                let handle = tokio::spawn(async move {
                                    let start = Instant::now();
                                    let result = send_video_frame(
                                        &conn, group_id, subgroup_id,
                                        quic_priority, capture_ts, &payload,
                                    ).await;
                                    let elapsed = start.elapsed();

                                    match result {
                                        Ok(()) => {
                                            if let Some(ref tx) = bw {
                                                let _ = tx.try_send(BandwidthEvent::SendComplete {
                                                    duration: elapsed,
                                                    payload_bytes: payload_len,
                                                });
                                            }
                                        }
                                        Err(e) => {
                                            tracing::trace!(
                                                "Video send to peer {} failed: {}", peer_id, e,
                                            );
                                        }
                                    }
                                });

                                in_flight.entry(peer_id)
                                    .or_default()
                                    .push((frame_seq, handle));
                            }

                            // Poll RTT from peer connections every 30 frames (~2 s at 15 fps).
                            rtt_poll_counter += 1;
                            if rtt_poll_counter % 30 == 0 {
                                if let Some(ref tx) = bw_tx {
                                    for (_peer_id, conn) in &peers {
                                        use iroh::Watcher;
                                        let paths = conn.paths().get();
                                        if let Some(selected) = paths.iter().find(|p| p.is_selected()) {
                                            let _ = tx.try_send(BandwidthEvent::RttSample {
                                                rtt_us: selected.rtt().as_micros() as u64,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        abort_all(&mut in_flight);
    }
}

fn abort_all(in_flight: &mut HashMap<PeerId, Vec<(u64, JoinHandle<()>)>>) {
    for (_, tasks) in in_flight.drain() {
        for (_, handle) in tasks {
            handle.abort();
        }
    }
}

async fn send_video_frame(
    conn: &iroh::endpoint::Connection,
    group_id: u64,
    subgroup_id: u64,
    quic_priority: i32,
    capture_timestamp_us: u64,
    payload: &[u8],
) -> Result<()> {
    let mut uni = protocol::open_video_subgroup(
        conn, group_id, subgroup_id, quic_priority, protocol::PRIORITY_VIDEO,
    ).await?;
    protocol::write_object(&mut uni, 0, capture_timestamp_us, Some(protocol::Codec::Hevc), payload).await?;
    uni.finish()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// VideoToolbox HEVC encode loop (with adaptive bitrate support)
// ---------------------------------------------------------------------------

/// Context shared between the encode loop and the VT output callback.
struct EncodeCallbackContext {
    pool: BufPool<u8>,
    tx: std::sync::mpsc::SyncSender<EncodedOutput>,
}

struct EncodedOutput {
    data: SharedBuf<u8>,
    is_keyframe: bool,
}

fn encode_loop(
    mut rx: mpsc::Receiver<Frame>,
    packet_tx: mpsc::Sender<EncodedPacket>,
    force_keyframe: Arc<AtomicBool>,
    encoder_config: Option<Arc<EncoderConfig>>,
    cancel: CancellationToken,
) {
    // Channel for the VT output callback to send encoded frames back to this thread.
    let (cb_tx, cb_rx) = std::sync::mpsc::sync_channel::<EncodedOutput>(1);
    let cb_ctx = Box::new(EncodeCallbackContext {
        pool: BufPool::new(),
        tx: cb_tx,
    });
    let cb_tx_ptr = Box::into_raw(cb_ctx);

    // Determine initial encode dimensions and bitrate.
    let initial_preset = QUALITY_LADDER.last().unwrap();
    let mut enc_width = encoder_config.as_ref()
        .map(|c| c.target_width.load(Ordering::Relaxed))
        .unwrap_or(initial_preset.width);
    let mut enc_height = encoder_config.as_ref()
        .map(|c| c.target_height.load(Ordering::Relaxed))
        .unwrap_or(initial_preset.height);
    let mut enc_fps = encoder_config.as_ref()
        .map(|c| c.target_fps.load(Ordering::Relaxed))
        .unwrap_or(initial_preset.fps);
    let mut enc_bitrate = encoder_config.as_ref()
        .map(|c| c.target_bitrate.load(Ordering::Relaxed))
        .unwrap_or(initial_preset.bitrate_bps);
    let mut applied_bitrate = enc_bitrate;

    let mut session = match create_compression_session(
        enc_width as i32, enc_height as i32, enc_fps, enc_bitrate,
        cb_tx_ptr as *mut c_void,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to create VideoToolbox compression session: {e}");
            unsafe { drop(Box::from_raw(cb_tx_ptr)); }
            return;
        }
    };

    let mut frame_count: u64 = 0;
    let mut encode_latency_sum: f64 = 0.0;

    // Reusable resize buffers and pool for NV12 scaling (only used when ABR changes resolution).
    let mut resize_y: Vec<u8> = Vec::new();
    let mut resize_cbcr: Vec<u8> = Vec::new();
    let mut resize_pool: Option<crate::video_capture::Nv12PixelBufferPool> = None;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // --- Check for quality preset change (resolution / fps) ---
        if let Some(ref cfg) = encoder_config {
            if cfg.quality_changed.swap(false, Ordering::Acquire) {
                let new_w = cfg.target_width.load(Ordering::Relaxed);
                let new_h = cfg.target_height.load(Ordering::Relaxed);
                let new_fps = cfg.target_fps.load(Ordering::Relaxed);
                let new_br = cfg.target_bitrate.load(Ordering::Relaxed);

                if new_w != enc_width || new_h != enc_height || new_fps != enc_fps {
                    tracing::info!(
                        "Encoder: recreating session for {}x{} @ {}fps {}kbps",
                        new_w, new_h, new_fps, new_br / 1000,
                    );

                    // Invalidate old session.
                    unsafe { session.invalidate(); }

                    enc_width = new_w;
                    enc_height = new_h;
                    enc_fps = new_fps;
                    enc_bitrate = new_br;
                    applied_bitrate = new_br;
                    frame_count = 0;
                    resize_pool = None; // invalidate pool for new dimensions

                    match create_compression_session(
                        enc_width as i32, enc_height as i32, enc_fps, enc_bitrate,
                        cb_tx_ptr as *mut c_void,
                    ) {
                        Ok(s) => {
                            session = s;
                            // Force keyframe so the decoder can pick up the new parameters.
                            force_keyframe.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            tracing::error!("Failed to recreate compression session: {e}");
                            unsafe { drop(Box::from_raw(cb_tx_ptr)); }
                            return;
                        }
                    }
                }
            }

            // --- Check for bitrate-only change ---
            let target_br = cfg.target_bitrate.load(Ordering::Relaxed);
            if target_br != applied_bitrate {
                applied_bitrate = target_br;
                unsafe {
                    let s: &CFType = &*((&*session) as *const VTCompressionSession as *const CFType);
                    set_number_property(s, &kVTCompressionPropertyKey_AverageBitRate, applied_bitrate as i64);
                }
                tracing::debug!("Encoder: bitrate updated to {}kbps", applied_bitrate / 1000);
            }
        }

        let frame_data = match rx.blocking_recv() {
            Some(f) => f,
            None => break,
        };

        let capture_timestamp_us = frame_data.capture_timestamp_us;
        let capture_to_encode_ms = frame_data.captured_at.elapsed().as_secs_f64() * 1000.0;
        encode_latency_sum += capture_to_encode_ms;
        frame_count += 1;
        if frame_count.is_multiple_of(30) {
            tracing::debug!(
                "Capture→encode latency (avg over 30): {:.1}ms",
                encode_latency_sum / 30.0,
            );
            encode_latency_sum = 0.0;
        }

        // --- Get or create the pixel buffer for encoding ---
        // When the frame's resolution matches the encode target, pass the
        // CVPixelBuffer directly (zero-copy from camera).  When ABR has
        // lowered the encode resolution, scale the NV12 planes into a new
        // CVPixelBuffer.
        let pixel_buffer = if frame_data.width == enc_width && frame_data.height == enc_height {
            // Zero-copy path: use the camera's pixel buffer directly.
            frame_data.pixel_buffer
        } else {
            // ABR resize: scale NV12 planes via vImage.
            let ew = enc_width as usize;
            let eh = enc_height as usize;

            resize_y.resize(ew * eh, 0);
            resize_cbcr.resize(ew * (eh / 2), 0);

            unsafe {
                CVPixelBufferLockBaseAddress(
                    &frame_data.pixel_buffer,
                    CVPixelBufferLockFlags::ReadOnly,
                );
            }

            let y_ptr = CVPixelBufferGetBaseAddressOfPlane(&frame_data.pixel_buffer, 0) as *const u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&frame_data.pixel_buffer, 0);
            let cbcr_ptr = CVPixelBufferGetBaseAddressOfPlane(&frame_data.pixel_buffer, 1) as *const u8;
            let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(&frame_data.pixel_buffer, 1);

            let scale_result = crate::vimage::scale_nv12(
                y_ptr, y_stride,
                cbcr_ptr, cbcr_stride,
                frame_data.width as usize, frame_data.height as usize,
                &mut resize_y, &mut resize_cbcr,
                ew, eh,
            );

            unsafe {
                CVPixelBufferUnlockBaseAddress(
                    &frame_data.pixel_buffer,
                    CVPixelBufferLockFlags::ReadOnly,
                );
            }

            if let Err(e) = scale_result {
                tracing::warn!("vImage NV12 scale failed in encoder: {e}");
                continue;
            }

            // Lazily create (or recreate) the resize pool when dimensions change.
            let pool = resize_pool.get_or_insert_with(|| {
                crate::video_capture::Nv12PixelBufferPool::new(enc_width, enc_height)
                    .expect("failed to create encoder resize pixel buffer pool")
            });
            match pool.create_filled(&resize_y, &resize_cbcr) {
                Ok(pb) => pb,
                Err(e) => {
                    tracing::warn!("Failed to get resized NV12 pixel buffer from pool: {e}");
                    continue;
                }
            }
        };

        let frame_props = if force_keyframe.swap(false, Ordering::Relaxed) {
            Some(make_force_keyframe_dict())
        } else {
            None
        };

        let pts = unsafe { CMTime::new(frame_count as i64, enc_fps as i32) };
        let duration = unsafe { CMTime::new(1, enc_fps as i32) };

        let status = unsafe {
            session.encode_frame(
                &pixel_buffer,
                pts,
                duration,
                frame_props.as_deref(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };

        if status != 0 {
            tracing::warn!("VTCompressionSessionEncodeFrame failed: {status}");
            continue;
        }

        // Force synchronous completion — callback will fire before this returns
        let status = unsafe { session.complete_frames(pts) };
        if status != 0 {
            tracing::warn!("VTCompressionSessionCompleteFrames failed: {status}");
        }

        // Retrieve output sent by the callback via the channel
        if let Ok(output) = cb_rx.try_recv() {
            match packet_tx.try_send(EncodedPacket {
                data: output.data,
                capture_timestamp_us,
                is_keyframe: output.is_keyframe,
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("Encode→network channel full, dropping packet");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    break;
                }
            }
        }
    }

    unsafe {
        session.invalidate();
        drop(Box::from_raw(cb_tx_ptr));
    }
}

/// The VTCompressionOutputCallback invoked by VideoToolbox when a frame is encoded.
unsafe extern "C-unwind" fn compression_output_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if status != 0 || sample_buffer.is_null() {
        tracing::warn!("Compression callback: status={status}, buffer null={}", sample_buffer.is_null());
        return;
    }

    let ctx = unsafe { &*(output_callback_ref_con as *const EncodeCallbackContext) };
    let sb = unsafe { &*sample_buffer };
    let is_keyframe = unsafe { is_sample_keyframe(sb) };

    match unsafe { extract_annex_b(sb, is_keyframe, &ctx.pool) } {
        Ok(data) => {
            let _ = ctx.tx.try_send(EncodedOutput { data, is_keyframe });
        }
        Err(e) => {
            tracing::warn!("Failed to extract encoded data: {e}");
        }
    }
}

/// Check if a CMSampleBuffer represents a keyframe.
unsafe fn is_sample_keyframe(sb: &CMSampleBuffer) -> bool {
    let attachments = unsafe { sb.sample_attachments_array(false) };
    match attachments {
        Some(arr) => {
            if arr.count() == 0 {
                return true;
            }
            let dict_ptr = unsafe { arr.value_at_index(0) };
            if dict_ptr.is_null() {
                return true;
            }
            let dict: &CFDictionary = unsafe { &*(dict_ptr as *const CFDictionary) };
            let not_sync_key: &CFString = unsafe { &*kCMSampleAttachmentKey_NotSync };
            let val = unsafe { dict.value((not_sync_key as *const CFString).cast()) };
            if val.is_null() {
                true
            } else {
                let bool_ref = unsafe { &*(val as *const CFBoolean) };
                !bool_ref.as_bool()
            }
        }
        None => true,
    }
}

/// Extract Annex B byte stream from a CMSampleBuffer into a pooled buffer.
unsafe fn extract_annex_b(sb: &CMSampleBuffer, is_keyframe: bool, pool: &BufPool<u8>) -> Result<SharedBuf<u8>> {
    // Start with a generous estimate to avoid re-checkout; the PoolBuf will
    // be truncated to the actual written length before sharing.
    let block_buffer_ref = unsafe { sb.data_buffer() }
        .ok_or_else(|| anyhow!("No data buffer in sample buffer"))?;
    let total_len = unsafe { block_buffer_ref.data_length() };
    // Estimate: param sets (~200B) + start codes (4B each) + payload
    let estimate = total_len + 256;
    let mut buf = pool.checkout(estimate);
    let mut write_pos = 0;

    macro_rules! push_bytes {
        ($slice:expr) => {{
            let s: &[u8] = $slice;
            let new_end = write_pos + s.len();
            if new_end > buf.len() {
                // Rare: estimate was too small. We can't resize a PoolBuf,
                // so fall back to a slightly larger checkout. This shouldn't
                // happen in practice since our estimate accounts for overhead.
                // For correctness we just return an error and let the caller
                // skip this frame.
                return Err(anyhow!("Annex B output exceeded estimate"));
            }
            buf[write_pos..new_end].copy_from_slice(s);
            write_pos = new_end;
        }};
    }

    if is_keyframe {
        let fmt_desc = unsafe { sb.format_description() }
            .ok_or_else(|| anyhow!("No format description on keyframe"))?;

        let mut param_count: usize = 0;
        let status = unsafe {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                &fmt_desc, 0, ptr::null_mut(), ptr::null_mut(), &mut param_count, ptr::null_mut(),
            )
        };
        if param_count == 0 && status != 0 {
            return Err(anyhow!("Failed to get HEVC parameter set count: {status}"));
        }

        for i in 0..param_count {
            let mut ps_ptr: *const u8 = ptr::null();
            let mut ps_size: usize = 0;
            let status = unsafe {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    &fmt_desc, i, &mut ps_ptr, &mut ps_size, ptr::null_mut(), ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(anyhow!("Failed to get HEVC parameter set {i}: {status}"));
            }
            push_bytes!(&[0, 0, 0, 1]);
            push_bytes!(unsafe { std::slice::from_raw_parts(ps_ptr, ps_size) });
        }
    }

    let mut data_ptr: *mut u8 = ptr::null_mut();
    let status = unsafe {
        block_buffer_ref.data_pointer(
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            (&mut data_ptr as *mut *mut u8).cast(),
        )
    };
    if status != 0 || data_ptr.is_null() {
        return Err(anyhow!("CMBlockBufferGetDataPointer failed: {status}"));
    }

    let data = unsafe { std::slice::from_raw_parts(data_ptr, total_len) };

    let nal_header_len = {
        let fmt_desc = unsafe { sb.format_description() }.unwrap();
        let mut header_len: std::ffi::c_int = 4;
        unsafe {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                &fmt_desc, 0, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), &mut header_len,
            );
        }
        header_len as usize
    };

    let mut offset = 0;
    while offset + nal_header_len <= total_len {
        let nalu_len = match nal_header_len {
            4 => u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]) as usize,
            2 => u16::from_be_bytes([data[offset], data[offset+1]]) as usize,
            1 => data[offset] as usize,
            _ => return Err(anyhow!("Unsupported NAL header length: {nal_header_len}")),
        };
        offset += nal_header_len;

        if offset + nalu_len > total_len {
            return Err(anyhow!("NALU extends beyond buffer"));
        }

        push_bytes!(&[0, 0, 0, 1]);
        push_bytes!(&data[offset..offset + nalu_len]);
        offset += nalu_len;
    }

    // Truncate the pooled buffer to the actual written length and share.
    Ok(buf.truncate_and_share(write_pos))
}

/// Create a VTCompressionSession for HEVC with low-latency settings.
fn create_compression_session(
    width: i32,
    height: i32,
    fps: u32,
    bitrate_bps: u32,
    callback_ref_con: *mut c_void,
) -> Result<CFRetained<VTCompressionSession>> {
    let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None, width, height,
            kCMVideoCodecType_HEVC,
            None, None, None,
            Some(compression_output_callback),
            callback_ref_con,
            NonNull::new(&mut session_ptr).unwrap(),
        )
    };

    if status != 0 || session_ptr.is_null() {
        return Err(anyhow!("VTCompressionSessionCreate failed: {status}"));
    }

    let session = unsafe { CFRetained::from_raw(NonNull::new(session_ptr).unwrap()) };

    unsafe {
        let s: &CFType = &*((&*session) as *const VTCompressionSession as *const CFType);

        set_bool_property(s, &kVTCompressionPropertyKey_RealTime, true);
        set_bool_property(s, &kVTCompressionPropertyKey_AllowFrameReordering, false);
        set_bool_property(s, &kVTCompressionPropertyKey_AllowOpenGOP, false);
        set_bool_property(s, &kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, true);

        VTSessionSetProperty(
            s,
            &kVTCompressionPropertyKey_ProfileLevel,
            Some(&*(&*kVTProfileLevel_HEVC_Main_AutoLevel as *const CFString as *const CFType)),
        );

        set_number_property(s, &kVTCompressionPropertyKey_MaxKeyFrameInterval, 150);
        set_f64_property(s, &kVTCompressionPropertyKey_ExpectedFrameRate, fps as f64);
        set_number_property(s, &kVTCompressionPropertyKey_AverageBitRate, bitrate_bps as i64);
        set_number_property(s, &kVTCompressionPropertyKey_MaxFrameDelayCount, 0);

        let status = session.prepare_to_encode_frames();
        if status != 0 {
            return Err(anyhow!("VTCompressionSessionPrepareToEncodeFrames failed: {status}"));
        }
    }

    Ok(session)
}

unsafe fn set_bool_property(session: &CFType, key: &CFString, value: bool) {
    let cf_value = CFBoolean::new(value);
    unsafe {
        VTSessionSetProperty(
            session, key,
            Some(&*(cf_value as *const CFBoolean as *const CFType)),
        );
    }
}

unsafe fn set_number_property(session: &CFType, key: &CFString, value: i64) {
    let num = CFNumber::new_i64(value);
    unsafe {
        VTSessionSetProperty(
            session, key,
            Some(&*(&*num as *const CFNumber as *const CFType)),
        );
    }
}

unsafe fn set_f64_property(session: &CFType, key: &CFString, value: f64) {
    let num = CFNumber::new_f64(value);
    unsafe {
        VTSessionSetProperty(
            session, key,
            Some(&*(&*num as *const CFNumber as *const CFType)),
        );
    }
}

/// Build a CFDictionary with kVTEncodeFrameOptionKey_ForceKeyFrame = true.
fn make_force_keyframe_dict() -> CFRetained<CFDictionary> {
    unsafe {
        let key: *const c_void = (&*kVTEncodeFrameOptionKey_ForceKeyFrame as *const CFString).cast();
        let value: *const c_void = (CFBoolean::new(true) as *const CFBoolean).cast();
        let mut keys = [key];
        let mut values = [value];
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ).expect("Failed to create force-keyframe dictionary")
    }
}
