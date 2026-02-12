use anyhow::Result;
use dav1d::PlanarImageComponent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::MediaObject;
use crate::room::PeerId;

/// An RGB24 frame ready for display, with the MoQ group_id preserved for correlation.
pub struct RgbFrame {
    pub group_id: u64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // RGB24, row-major, 3 bytes per pixel
}

/// Receives dispatched AV1 payloads, decodes with dav1d, converts to RGB,
/// and sends tagged frames to the compositor channel.
pub async fn run_video_decoder(
    peer_id: PeerId,
    mut rx: mpsc::Receiver<MediaObject>,
    compositor_tx: mpsc::Sender<(PeerId, RgbFrame)>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut settings = dav1d::Settings::new();
    settings.set_max_frame_delay(1);
    settings.set_n_threads(2);
    // Silence dav1d's internal stderr logging
    unsafe {
        let inner = &mut settings as *mut dav1d::Settings as *mut dav1d_sys::Dav1dSettings;
        (*inner).logger.callback = std::ptr::null_mut();
    }
    let mut decoder = dav1d::Decoder::with_settings(&settings)?;

    let mut rgb_buf: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            packet = rx.recv() => {
                match packet {
                    Some(obj) => {
                        let group_id = obj.group_id;

                        if decoder.send_data(obj.payload, None, None, None).is_err() {
                            continue;
                        }

                        while let Ok(picture) = decoder.get_picture() {
                            let w = picture.width() as usize;
                            let h = picture.height() as usize;
                            let needed = w * h * 3;
                            rgb_buf.resize(needed, 0);

                            picture_to_rgb_fast(&picture, &mut rgb_buf);

                            let _ = compositor_tx.try_send((peer_id, RgbFrame {
                                group_id,
                                width: w as u32,
                                height: h as u32,
                                data: rgb_buf.clone(),
                            }));
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}

/// Convert a dav1d I420 picture to RGB24 using fixed-point integer math.
#[inline]
fn picture_to_rgb_fast(pic: &dav1d::Picture, rgb: &mut [u8]) {
    let width = pic.width() as usize;
    let height = pic.height() as usize;

    let y_plane = pic.plane(PlanarImageComponent::Y);
    let u_plane = pic.plane(PlanarImageComponent::U);
    let v_plane = pic.plane(PlanarImageComponent::V);

    let y_stride = pic.stride(PlanarImageComponent::Y) as usize;
    let u_stride = pic.stride(PlanarImageComponent::U) as usize;

    for row in 0..height {
        let y_row = row * y_stride;
        let uv_row = (row >> 1) * u_stride;
        let rgb_row = row * width * 3;

        for col in 0..width {
            let y = y_plane[y_row + col] as i32;
            let u = u_plane[uv_row + (col >> 1)] as i32;
            let v = v_plane[uv_row + (col >> 1)] as i32;

            let cb = u - 128;
            let cr = v - 128;

            let r = y + ((359 * cr) >> 8);
            let g = y - ((88 * cb + 183 * cr) >> 8);
            let b = y + ((454 * cb) >> 8);

            let idx = rgb_row + col * 3;
            rgb[idx] = r.clamp(0, 255) as u8;
            rgb[idx + 1] = g.clamp(0, 255) as u8;
            rgb[idx + 2] = b.clamp(0, 255) as u8;
        }
    }
}
