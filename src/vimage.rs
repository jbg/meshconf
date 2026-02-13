//! macOS vImage (Accelerate.framework) bindings for fast YUV↔RGB conversion
//! and NV12 plane scaling.

use std::ffi::c_void;

use anyhow::{Result, anyhow};

// ---------------------------------------------------------------------------
// FFI types
// ---------------------------------------------------------------------------

type VImagePixelCount = usize;
type VImageError = isize;
type VImageFlags = u32;

#[allow(non_upper_case_globals)]
const kvImageNoFlags: VImageFlags = 0;

#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: VImagePixelCount,
    width: VImagePixelCount,
    row_bytes: usize,
}

#[repr(C)]
struct VImageYpCbCrToARGBMatrix {
    yp: f32,
    cr_r: f32,
    cr_g: f32,
    cb_g: f32,
    cb_b: f32,
}

#[repr(C)]
struct VImageYpCbCrPixelRange {
    yp_bias: i32,
    cb_cr_bias: i32,
    yp_range_max: i32,
    cb_cr_range_max: i32,
    yp_max: i32,
    yp_min: i32,
    cb_cr_max: i32,
    cb_cr_min: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VImageYpCbCrToARGB {
    opaque: [u8; 128],
}

// vImageYpCbCrType constants
#[allow(non_upper_case_globals)]
const kvImage420Yp8_Cb8_Cr8: i32 = 3;
#[allow(non_upper_case_globals)]
const kvImage420Yp8_CbCr8: i32 = 4;

// vImageARGBType constants
#[allow(non_upper_case_globals)]
const kvImageARGB8888: i32 = 0;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    static kvImage_YpCbCrToARGBMatrix_ITU_R_601_4: *const VImageYpCbCrToARGBMatrix;

    fn vImageConvert_YpCbCrToARGB_GenerateConversion(
        matrix: *const VImageYpCbCrToARGBMatrix,
        pixel_range: *const VImageYpCbCrPixelRange,
        out_info: *mut VImageYpCbCrToARGB,
        in_yp_cb_cr_type: i32,
        out_argb_type: i32,
        flags: VImageFlags,
    ) -> VImageError;

    fn vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
        src_yp: *const VImageBuffer,
        src_cb: *const VImageBuffer,
        src_cr: *const VImageBuffer,
        dest: *const VImageBuffer,
        info: *const VImageYpCbCrToARGB,
        permute_map: *const u8,
        alpha: u8,
        flags: VImageFlags,
    ) -> VImageError;

    fn vImageConvert_420Yp8_CbCr8ToARGB8888(
        src_yp: *const VImageBuffer,
        src_cb_cr: *const VImageBuffer,
        dest: *const VImageBuffer,
        info: *const VImageYpCbCrToARGB,
        permute_map: *const u8,
        alpha: u8,
        flags: VImageFlags,
    ) -> VImageError;

    fn vImageScale_Planar8(
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        temp_buffer: *mut c_void,
        flags: VImageFlags,
    ) -> VImageError;

    fn vImageScale_Planar16U(
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        temp_buffer: *mut c_void,
        flags: VImageFlags,
    ) -> VImageError;
}

/// permuteMap that produces bytes [B, G, R, A] in memory, which reads as
/// `0xAARRGGBB` on little-endian (= `0x00RRGGBB` with alpha=0).
const PERMUTE_TO_MINIFB: [u8; 4] = [3, 2, 1, 0];

// ---------------------------------------------------------------------------
// Pixel range presets
// ---------------------------------------------------------------------------

fn video_range() -> VImageYpCbCrPixelRange {
    VImageYpCbCrPixelRange {
        yp_bias: 16,
        cb_cr_bias: 128,
        yp_range_max: 235,
        cb_cr_range_max: 240,
        yp_max: 235,
        yp_min: 16,
        cb_cr_max: 240,
        cb_cr_min: 16,
    }
}

fn generate_conversion(
    ypcbcr_type: i32,
    pixel_range: &VImageYpCbCrPixelRange,
) -> Result<VImageYpCbCrToARGB> {
    let mut info = VImageYpCbCrToARGB { opaque: [0u8; 128] };
    let status = unsafe {
        vImageConvert_YpCbCrToARGB_GenerateConversion(
            kvImage_YpCbCrToARGBMatrix_ITU_R_601_4,
            pixel_range,
            &mut info,
            ypcbcr_type,
            kvImageARGB8888,
            0,
        )
    };
    if status != 0 {
        return Err(anyhow!(
            "vImageConvert_YpCbCrToARGB_GenerateConversion failed: {status}"
        ));
    }
    Ok(info)
}

// ===========================================================================
// I420 converter (used by the decode path)
// ===========================================================================

/// Pre-computed conversion info for I420 → packed u32.
pub struct I420Converter {
    info: VImageYpCbCrToARGB,
}

unsafe impl Send for I420Converter {}
unsafe impl Sync for I420Converter {}

impl I420Converter {
    pub fn new_bt601_video_range() -> Result<Self> {
        Ok(Self {
            info: generate_conversion(kvImage420Yp8_Cb8_Cr8, &video_range())?,
        })
    }

    /// Convert I420 (separate Y, U, V planes) to packed `0x00RRGGBB` u32 pixels.
    pub fn i420_to_packed_u32(
        &self,
        w: usize,
        h: usize,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        dst: &mut [u32],
    ) -> Result<()> {
        let half_w = w / 2;
        let src_yp = VImageBuffer {
            data: y.as_ptr() as *mut c_void,
            height: h,
            width: w,
            row_bytes: w,
        };
        let src_cb = VImageBuffer {
            data: u.as_ptr() as *mut c_void,
            height: h / 2,
            width: half_w,
            row_bytes: half_w,
        };
        let src_cr = VImageBuffer {
            data: v.as_ptr() as *mut c_void,
            height: h / 2,
            width: half_w,
            row_bytes: half_w,
        };
        let dest = VImageBuffer {
            data: dst.as_mut_ptr() as *mut c_void,
            height: h,
            width: w,
            row_bytes: w * 4,
        };
        let status = unsafe {
            vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
                &src_yp, &src_cb, &src_cr, &dest, &self.info,
                PERMUTE_TO_MINIFB.as_ptr(), 0, kvImageNoFlags,
            )
        };
        if status != 0 {
            return Err(anyhow!(
                "vImageConvert_420Yp8_Cb8_Cr8ToARGB8888 failed: {status}"
            ));
        }
        Ok(())
    }

    /// Convert concatenated I420 data (Y+U+V in one buffer) to packed `0x00RRGGBB`.
    pub fn i420_concat_to_packed_u32(
        &self,
        w: usize,
        h: usize,
        yuv: &[u8],
        dst: &mut [u32],
    ) -> Result<()> {
        let half_w = w / 2;
        let half_h = h / 2;
        let y = &yuv[..w * h];
        let u = &yuv[w * h..w * h + half_w * half_h];
        let v = &yuv[w * h + half_w * half_h..];
        self.i420_to_packed_u32(w, h, y, u, v, dst)
    }

    /// Convert I420 planes given as raw pointers with strides to packed `0x00RRGGBB`.
    ///
    /// Works directly on `CVPixelBuffer` plane pointers without any intermediate copy.
    pub fn i420_planes_to_packed_u32(
        &self,
        w: usize,
        h: usize,
        y_ptr: *const u8,
        y_stride: usize,
        u_ptr: *const u8,
        u_stride: usize,
        v_ptr: *const u8,
        v_stride: usize,
        dst: &mut [u32],
    ) -> Result<()> {
        let half_w = w / 2;
        let src_yp = VImageBuffer {
            data: y_ptr as *mut c_void,
            height: h,
            width: w,
            row_bytes: y_stride,
        };
        let src_cb = VImageBuffer {
            data: u_ptr as *mut c_void,
            height: h / 2,
            width: half_w,
            row_bytes: u_stride,
        };
        let src_cr = VImageBuffer {
            data: v_ptr as *mut c_void,
            height: h / 2,
            width: half_w,
            row_bytes: v_stride,
        };
        let dest = VImageBuffer {
            data: dst.as_mut_ptr() as *mut c_void,
            height: h,
            width: w,
            row_bytes: w * 4,
        };
        let status = unsafe {
            vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
                &src_yp, &src_cb, &src_cr, &dest, &self.info,
                PERMUTE_TO_MINIFB.as_ptr(), 0, kvImageNoFlags,
            )
        };
        if status != 0 {
            return Err(anyhow!(
                "vImageConvert_420Yp8_Cb8_Cr8ToARGB8888 failed: {status}"
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// NV12 converter (used by the capture/preview path)
// ===========================================================================

/// Pre-computed conversion info for NV12 → packed u32.
pub struct Nv12Converter {
    info: VImageYpCbCrToARGB,
}

unsafe impl Send for Nv12Converter {}
unsafe impl Sync for Nv12Converter {}

impl Nv12Converter {
    pub fn new_bt601_video_range() -> Result<Self> {
        Ok(Self {
            info: generate_conversion(kvImage420Yp8_CbCr8, &video_range())?,
        })
    }

    /// Convert NV12 planes (Y + interleaved CbCr) to packed `0x00RRGGBB`.
    ///
    /// Accepts raw plane pointers with stride, so it works directly on a
    /// locked CVPixelBuffer's planes without any intermediate copy.
    pub fn nv12_to_packed_u32(
        &self,
        w: usize,
        h: usize,
        y_ptr: *const u8,
        y_stride: usize,
        cbcr_ptr: *const u8,
        cbcr_stride: usize,
        dst: &mut [u32],
    ) -> Result<()> {
        let src_yp = VImageBuffer {
            data: y_ptr as *mut c_void,
            height: h,
            width: w,
            row_bytes: y_stride,
        };
        let src_cbcr = VImageBuffer {
            data: cbcr_ptr as *mut c_void,
            height: h / 2,
            width: w / 2,
            row_bytes: cbcr_stride,
        };
        let dest = VImageBuffer {
            data: dst.as_mut_ptr() as *mut c_void,
            height: h,
            width: w,
            row_bytes: w * 4,
        };
        let status = unsafe {
            vImageConvert_420Yp8_CbCr8ToARGB8888(
                &src_yp, &src_cbcr, &dest, &self.info,
                PERMUTE_TO_MINIFB.as_ptr(), 0, kvImageNoFlags,
            )
        };
        if status != 0 {
            return Err(anyhow!(
                "vImageConvert_420Yp8_CbCr8ToARGB8888 failed: {status}"
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// NV12 scaling (used when camera resolution ≠ encode resolution)
// ===========================================================================

/// Scale NV12 planes in-place from (src_w, src_h) to (dst_w, dst_h).
///
/// Y plane is scaled with `vImageScale_Planar8`.
/// CbCr plane (interleaved) is scaled with `vImageScale_Planar16U`,
/// treating each CbCr pair as a single 16-bit pixel.
///
/// The destination slices must be pre-allocated to the correct sizes:
/// - `dst_y`: `dst_w * dst_h` bytes
/// - `dst_cbcr`: `dst_w * (dst_h / 2)` bytes  (i.e. `(dst_w/2) * (dst_h/2)` pairs × 2)
pub fn scale_nv12(
    src_y: *const u8,
    src_y_stride: usize,
    src_cbcr: *const u8,
    src_cbcr_stride: usize,
    src_w: usize,
    src_h: usize,
    dst_y: &mut [u8],
    dst_cbcr: &mut [u8],
    dst_w: usize,
    dst_h: usize,
) -> Result<()> {
    // Scale Y plane
    let src_y_buf = VImageBuffer {
        data: src_y as *mut c_void,
        height: src_h,
        width: src_w,
        row_bytes: src_y_stride,
    };
    let dst_y_buf = VImageBuffer {
        data: dst_y.as_mut_ptr() as *mut c_void,
        height: dst_h,
        width: dst_w,
        row_bytes: dst_w,
    };
    let status = unsafe {
        vImageScale_Planar8(&src_y_buf, &dst_y_buf, std::ptr::null_mut(), kvImageNoFlags)
    };
    if status != 0 {
        return Err(anyhow!("vImageScale_Planar8 (Y) failed: {status}"));
    }

    // Scale CbCr plane — treat each CbCr pair as a 16-bit pixel.
    let src_half_w = src_w / 2;
    let src_half_h = src_h / 2;
    let dst_half_w = dst_w / 2;
    let dst_half_h = dst_h / 2;

    let src_cbcr_buf = VImageBuffer {
        data: src_cbcr as *mut c_void,
        height: src_half_h,
        width: src_half_w,
        row_bytes: src_cbcr_stride,
    };
    let dst_cbcr_buf = VImageBuffer {
        data: dst_cbcr.as_mut_ptr() as *mut c_void,
        height: dst_half_h,
        width: dst_half_w,
        row_bytes: dst_half_w * 2,
    };
    let status = unsafe {
        vImageScale_Planar16U(
            &src_cbcr_buf,
            &dst_cbcr_buf,
            std::ptr::null_mut(),
            kvImageNoFlags,
        )
    };
    if status != 0 {
        return Err(anyhow!("vImageScale_Planar16U (CbCr) failed: {status}"));
    }

    Ok(())
}
