use std::ffi::c_void;
use std::ptr::{self, NonNull};

use anyhow::{Result, anyhow};
use objc2_core_foundation::*;
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_video_toolbox::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::pool::{BufPool, PoolBuf, SharedBuf};
use crate::protocol::MediaObject;
use crate::room::PeerId;
use crate::vimage::{I420Converter, Nv12Converter};
use crate::video_compositor::VideoDropCounter;

/// A frame ready for display, with MoQ/LOC metadata preserved.
///
/// Pixels are packed `0x00RRGGBB` u32 values (one per pixel, row-major),
/// suitable for direct blitting into a minifb window buffer.
///
/// The pixel data is backed by a [`SharedBuf`] from a pool — cloning a
/// frame is a reference-count bump with no data copy, and when the last
/// reference is dropped the buffer is returned to the pool (not freed).
pub struct RgbFrame {
    pub group_id: u64,
    /// Wall-clock capture time in microseconds since Unix epoch (LOC Capture Timestamp).
    pub capture_timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    pub data: SharedBuf<u32>,
}

/// Receives dispatched HEVC payloads, decodes with VideoToolbox, converts to RGB,
/// and sends tagged frames to the compositor channel.
pub async fn run_video_decoder(
    peer_id: PeerId,
    mut rx: mpsc::Receiver<MediaObject>,
    compositor_tx: mpsc::Sender<(PeerId, RgbFrame)>,
    video_drops: VideoDropCounter,
    cancel: CancellationToken,
) -> Result<()> {
    let mut decoder: Option<HevcDecoder> = None;
    let rgb_pool = BufPool::<u32>::new();

    let mut got_keyframe = false;
    let mut keyframe_group: u64 = 0;
    let mut last_decoded_group: u64 = 0;


    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            packet = rx.recv() => {
                match packet {
                    Some(obj) => {
                        let group_id = obj.group_id;
                        let subgroup_id = obj.subgroup_id;
                        let capture_timestamp_us = obj.capture_timestamp_us;
                        let is_keyframe = subgroup_id == 0;

                        tracing::trace!(
                            "Decoder for peer {}: received group={} sub={} key={} payload={}B",
                            peer_id, group_id, subgroup_id, is_keyframe, obj.payload.len(),
                        );

                        // --- Gap detection ---
                        if got_keyframe && !is_keyframe {
                            if group_id != last_decoded_group + 1 {
                                tracing::warn!(
                                    "Video decoder for peer {}: gap detected \
                                     (last decoded group {}, got group {}), \
                                     waiting for next keyframe",
                                    peer_id, last_decoded_group, group_id,
                                );
                                got_keyframe = false;
                                video_drops.increment();
                                decoder = None;
                            }
                        }

                        // --- Keyframe gate ---
                        if !got_keyframe {
                            if !is_keyframe {
                                video_drops.increment();
                                continue;
                            }
                            got_keyframe = true;
                            keyframe_group = group_id;
                            tracing::info!(
                                "Video decoder for peer {}: got keyframe (group {})",
                                peer_id, group_id,
                            );
                        }

                        if is_keyframe && group_id > keyframe_group {
                            keyframe_group = group_id;
                        }

                        // On keyframe, (re)create decoder only if parameter sets changed.
                        if is_keyframe {
                            let needs_new_session = match decoder.as_ref() {
                                Some(dec) => !dec.params_match(&obj.payload),
                                None => true,
                            };
                            if needs_new_session {
                                match HevcDecoder::new_from_annex_b(&obj.payload, rgb_pool.clone()) {
                                    Ok(d) => decoder = Some(d),
                                    Err(e) => {
                                        tracing::warn!("Failed to create HEVC decoder: {e}");
                                        got_keyframe = false;
                                        video_drops.increment();
                                        continue;
                                    }
                                }
                            }
                        }

                        let dec = match decoder.as_mut() {
                            Some(d) => d,
                            None => {
                                video_drops.increment();
                                continue;
                            }
                        };

                        // Decode — the callback converts directly from
                        // CVPixelBuffer planes to a pooled RGB buffer via
                        // vImage, with no intermediate I420 copy.
                        match dec.decode_annex_b(&obj.payload) {
                            Ok(Some(frame)) => {
                                last_decoded_group = group_id;

                                if compositor_tx.try_send((peer_id, RgbFrame {
                                    group_id,
                                    capture_timestamp_us,
                                    width: frame.width as u32,
                                    height: frame.height as u32,
                                    data: frame.data.share(),
                                })).is_err() {
                                    video_drops.increment();
                                }
                            }
                            Ok(None) => {
                                // Decoder didn't produce output (e.g. buffering)
                                last_decoded_group = group_id;
                            }
                            Err(e) => {
                                tracing::warn!("HEVC decode error: {e}");
                                video_drops.increment();
                                got_keyframe = false;
                                decoder = None;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// VideoToolbox HEVC Decoder
// ---------------------------------------------------------------------------

/// Context shared between the decoder and its VT output callback.
///
/// The callback converts CVPixelBuffer planes directly to packed RGB via
/// vImage — no intermediate I420 buffer, no per-frame allocation.
struct CallbackContext {
    rgb_pool: BufPool<u32>,
    i420_converter: I420Converter,
    nv12_converter: Nv12Converter,
    tx: std::sync::mpsc::SyncSender<Result<DecodedRgbFrame>>,
}

/// Decoded frame already converted to packed RGB, backed by the pool.
struct DecodedRgbFrame {
    width: usize,
    height: usize,
    data: PoolBuf<u32>,
}

struct HevcDecoder {
    session: CFRetained<VTDecompressionSession>,
    format_desc: CFRetained<CMFormatDescription>,
    /// Receives decoded+converted RGB frames from the VT callback.
    cb_rx: std::sync::mpsc::Receiver<Result<DecodedRgbFrame>>,
    /// Prevent the context from being dropped while the session is alive.
    _cb_ctx_ptr: *mut CallbackContext,
    /// Reusable buffer for Annex B → HVCC conversion.
    hvcc_buf: Vec<u8>,
    /// Concatenated VPS/SPS/PPS bytes used to create this session.
    /// Used to detect when parameter sets change across keyframes.
    param_set_bytes: Vec<u8>,
}

// VTDecompressionSession is thread-safe per Apple docs
unsafe impl Send for HevcDecoder {}
unsafe impl Sync for HevcDecoder {}

impl Drop for HevcDecoder {
    fn drop(&mut self) {
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self._cb_ctx_ptr));
        }
    }
}

/// Concatenate VPS/SPS/PPS parameter set bytes from an Annex B keyframe
/// into `out`. Used for fast equality comparison across keyframes.
fn collect_param_set_bytes(annex_b: &[u8], out: &mut Vec<u8>) {
    out.clear();
    for (start, end) in parse_annex_b_nalus(annex_b) {
        let nalu = &annex_b[start..end];
        if nalu.len() < 2 { continue; }
        let nalu_type = (nalu[0] >> 1) & 0x3F;
        if nalu_type == 32 || nalu_type == 33 || nalu_type == 34 {
            out.extend_from_slice(nalu);
        }
    }
}

impl HevcDecoder {
    /// Returns true if the parameter sets in `annex_b` match the ones
    /// this session was created with (i.e. no session rebuild needed).
    fn params_match(&self, annex_b: &[u8]) -> bool {
        let mut scratch = Vec::new();
        collect_param_set_bytes(annex_b, &mut scratch);
        scratch == self.param_set_bytes
    }

    /// Create a new decoder from an Annex B keyframe.
    /// Extracts VPS/SPS/PPS parameter sets from the stream.
    fn new_from_annex_b(annex_b: &[u8], rgb_pool: BufPool<u32>) -> Result<Self> {
        // Collect parameter sets: VPS (type 32), SPS (type 33), PPS (type 34)
        let mut param_sets: Vec<&[u8]> = Vec::new();
        let mut param_set_bytes = Vec::new();
        for (start, end) in parse_annex_b_nalus(annex_b) {
            let nalu = &annex_b[start..end];
            if nalu.len() < 2 { continue; }
            let nalu_type = (nalu[0] >> 1) & 0x3F;
            if nalu_type == 32 || nalu_type == 33 || nalu_type == 34 {
                param_sets.push(nalu);
                param_set_bytes.extend_from_slice(nalu);
            }
        }

        if param_sets.is_empty() {
            return Err(anyhow!("No HEVC parameter sets found in keyframe"));
        }

        let param_ptrs: Vec<NonNull<u8>> = param_sets.iter()
            .map(|ps| NonNull::new(ps.as_ptr() as *mut u8).unwrap())
            .collect();
        let param_sizes: Vec<usize> = param_sets.iter().map(|ps| ps.len()).collect();

        let mut fmt_desc_ptr: *const CMFormatDescription = ptr::null();
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                param_sets.len(),
                NonNull::new(param_ptrs.as_ptr() as *mut NonNull<u8>).unwrap(),
                NonNull::new(param_sizes.as_ptr() as *mut usize).unwrap(),
                4, // NAL unit header length
                None,
                NonNull::new(&mut fmt_desc_ptr as *mut *const CMFormatDescription).unwrap(),
            )
        };

        if status != 0 || fmt_desc_ptr.is_null() {
            return Err(anyhow!("CMVideoFormatDescriptionCreateFromHEVCParameterSets failed: {status}"));
        }

        let format_desc = unsafe {
            CFRetained::from_raw(NonNull::new(fmt_desc_ptr as *mut CMFormatDescription).unwrap())
        };

        // Channel for the VT callback to send decoded RGB frames back.
        let (cb_tx, cb_rx) = std::sync::mpsc::sync_channel::<Result<DecodedRgbFrame>>(1);

        let i420_converter = I420Converter::new_bt601_video_range()
            .expect("failed to create vImage I420 converter");
        let nv12_converter = Nv12Converter::new_bt601_video_range()
            .expect("failed to create vImage NV12 converter");

        let ctx = Box::new(CallbackContext {
            rgb_pool,
            i420_converter,
            nv12_converter,
            tx: cb_tx,
        });
        let ctx_ptr = Box::into_raw(ctx);

        let callback_record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decompression_output_callback),
            decompressionOutputRefCon: ctx_ptr as *mut c_void,
        };

        // Don't request a specific pixel format — let VT pick its preferred
        // format (usually NV12 or I420). We handle both in the callback.
        let mut session_ptr: *mut VTDecompressionSession = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format_desc,
                None,
                None,
                &callback_record,
                NonNull::new(&mut session_ptr).unwrap(),
            )
        };

        if status != 0 || session_ptr.is_null() {
            unsafe { drop(Box::from_raw(ctx_ptr)); }
            return Err(anyhow!("VTDecompressionSessionCreate failed: {status}"));
        }

        let session = unsafe { CFRetained::from_raw(NonNull::new(session_ptr).unwrap()) };

        Ok(Self {
            session,
            format_desc,
            cb_rx,
            _cb_ctx_ptr: ctx_ptr,
            hvcc_buf: Vec::new(),
            param_set_bytes,
        })
    }

    /// Decode an Annex B HEVC frame. Returns a pooled RGB buffer or None.
    fn decode_annex_b(&mut self, annex_b: &[u8]) -> Result<Option<DecodedRgbFrame>> {
        // Convert Annex B to HVCC (length-prefixed), reusing buffer.
        annex_b_to_hvcc_into(annex_b, &mut self.hvcc_buf);
        if self.hvcc_buf.is_empty() {
            return Ok(None);
        }

        // Create CMBlockBuffer from the HVCC data
        let mut block_buf_ptr: *mut CMBlockBuffer = ptr::null_mut();
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                self.hvcc_buf.as_ptr() as *mut c_void,
                self.hvcc_buf.len(),
                kCFAllocatorNull,
                ptr::null(),
                0,
                self.hvcc_buf.len(),
                0, // no flags
                NonNull::new(&mut block_buf_ptr).unwrap(),
            )
        };

        if status != 0 || block_buf_ptr.is_null() {
            return Err(anyhow!("CMBlockBufferCreateWithMemoryBlock failed: {status}"));
        }

        let block_buffer = unsafe { CFRetained::from_raw(NonNull::new(block_buf_ptr).unwrap()) };

        // Create CMSampleBuffer
        let timing = CMSampleTimingInfo {
            duration: unsafe { kCMTimeInvalid },
            presentationTimeStamp: unsafe { kCMTimeZero },
            decodeTimeStamp: unsafe { kCMTimeInvalid },
        };
        let sample_size = self.hvcc_buf.len();

        let mut sample_buf_ptr: *mut CMSampleBuffer = ptr::null_mut();
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block_buffer),
                Some(&self.format_desc),
                1, // numSamples
                1, // numSampleTimingEntries
                &timing,
                1, // numSampleSizeEntries
                &sample_size,
                NonNull::new(&mut sample_buf_ptr).unwrap(),
            )
        };

        if status != 0 || sample_buf_ptr.is_null() {
            return Err(anyhow!("CMSampleBufferCreateReady failed: {status}"));
        }

        let sample_buffer = unsafe { CFRetained::from_raw(NonNull::new(sample_buf_ptr).unwrap()) };

        // Decode synchronously (no async, no temporal processing)
        let decode_flags = VTDecodeFrameFlags(0);
        let mut info_flags = VTDecodeInfoFlags(0);

        let status = unsafe {
            self.session.decode_frame(
                &sample_buffer,
                decode_flags,
                ptr::null_mut(),
                &mut info_flags,
            )
        };

        if status != 0 {
            return Err(anyhow!("VTDecompressionSessionDecodeFrame failed: {status}"));
        }

        // The callback should have fired synchronously (decode_flags = 0).
        match self.cb_rx.try_recv() {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }
}

/// VTDecompressionOutputCallback — converts CVPixelBuffer planes directly
/// to packed RGB via vImage into a pooled buffer. No intermediate I420 copy.
unsafe extern "C-unwind" fn decompression_output_callback(
    ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let ctx = unsafe { &*(ref_con as *const CallbackContext) };

    if status != 0 || image_buffer.is_null() {
        let _ = ctx.tx.try_send(Err(anyhow!("Decode callback error: status={status}")));
        return;
    }

    let pb = unsafe { &*(image_buffer as *const CVPixelBuffer) };

    unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(1)) }; // read-only

    let w = CVPixelBufferGetWidth(pb);
    let h = CVPixelBufferGetHeight(pb);
    let plane_count = CVPixelBufferGetPlaneCount(pb);

    let mut rgb_buf = ctx.rgb_pool.checkout(w * h);

    let result = if plane_count >= 3 {
        // I420: 3 separate planes (Y, U, V)
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        let u_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *const u8;
        let u_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
        let v_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 2) as *const u8;
        let v_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 2);
        ctx.i420_converter.i420_planes_to_packed_u32(
            w, h, y_ptr, y_stride, u_ptr, u_stride, v_ptr, v_stride, &mut rgb_buf,
        )
    } else if plane_count == 2 {
        // NV12: Y plane + interleaved CbCr plane
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        let cbcr_ptr = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *const u8;
        let cbcr_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
        ctx.nv12_converter.nv12_to_packed_u32(
            w, h, y_ptr, y_stride, cbcr_ptr, cbcr_stride, &mut rgb_buf,
        )
    } else {
        Err(anyhow!("Unexpected plane count: {plane_count}"))
    };

    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(1)) };

    match result {
        Ok(()) => {
            let _ = ctx.tx.try_send(Ok(DecodedRgbFrame {
                width: w,
                height: h,
                data: rgb_buf,
            }));
        }
        Err(e) => {
            let _ = ctx.tx.try_send(Err(e));
        }
    }
}

// ---------------------------------------------------------------------------
// Annex B parsing and conversion
// ---------------------------------------------------------------------------

/// Parse Annex B byte stream into individual NAL units (without start codes).
/// Zero-allocation iterator over Annex B NAL unit boundaries.
///
/// Yields `(start, end)` byte offset pairs into the original data slice.
struct AnnexBNalus<'a> {
    data: &'a [u8],
    pos: usize,
}

fn parse_annex_b_nalus(data: &[u8]) -> AnnexBNalus<'_> {
    AnnexBNalus { data, pos: 0 }
}

impl<'a> Iterator for AnnexBNalus<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        let data = self.data;
        while self.pos < data.len() {
            let i = self.pos;
            if i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 {
                let (sc_len, found) = if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    (4, true)
                } else if data[i + 2] == 1 {
                    (3, true)
                } else {
                    (0, false)
                };

                if found {
                    let nalu_start = i + sc_len;
                    let mut j = nalu_start;
                    while j < data.len() {
                        if j + 3 <= data.len() && data[j] == 0 && data[j + 1] == 0 {
                            if (j + 4 <= data.len() && data[j + 2] == 0 && data[j + 3] == 1)
                                || data[j + 2] == 1
                            {
                                break;
                            }
                        }
                        j += 1;
                    }
                    self.pos = j;
                    if nalu_start < j {
                        return Some((nalu_start, j));
                    }
                    continue;
                }
            }
            self.pos += 1;
        }
        None
    }
}

/// Convert Annex B NALUs to HVCC format (4-byte length prefix) into `output`.
/// Skips parameter set NALUs (VPS/SPS/PPS) as they're in the format description.
/// Reuses the `output` buffer's allocation across calls.
fn annex_b_to_hvcc_into(annex_b: &[u8], output: &mut Vec<u8>) {
    output.clear();

    for (start, end) in parse_annex_b_nalus(annex_b) {
        let nalu = &annex_b[start..end];
        if nalu.len() < 2 { continue; }
        let nalu_type = (nalu[0] >> 1) & 0x3F;
        // Skip parameter sets (VPS=32, SPS=33, PPS=34)
        if nalu_type == 32 || nalu_type == 33 || nalu_type == 34 {
            continue;
        }
        let len = nalu.len() as u32;
        output.extend_from_slice(&len.to_be_bytes());
        output.extend_from_slice(nalu);
    }
}


