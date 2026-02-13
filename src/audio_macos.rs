use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use block2::RcBlock;
use objc2::AnyThread;
use objc2_avf_audio::{
    AVAudioConverter, AVAudioConverterInputStatus,
    AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioPlayerNode, AVAudioTime,
};
use tokio::sync::mpsc;

use crate::audio::AudioEngine;
use crate::pool::{BufPool, SharedBuf};

const SAMPLE_RATE: f64 = 48000.0;
const CHANNELS: u32 = 1;
const CHUNK_SIZE: usize = 960; // 20ms @ 48kHz

pub fn start() -> crate::audio::AudioStartResult {
    let (capture_tx, capture_rx) = mpsc::channel::<SharedBuf<f32>>(10);
    let (playback_tx, playback_rx) = mpsc::channel::<SharedBuf<f32>>(50);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();

    let thread = std::thread::Builder::new()
        .name("audio-engine".into())
        .spawn(move || {
            engine_thread(capture_tx, playback_rx, stop_rx, init_tx);
        })?;

    match init_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(anyhow!("Audio engine thread died during init")),
    }

    Ok((
        AudioEngine {
            stop_tx: Some(stop_tx),
            thread: Some(thread),
        },
        capture_rx,
        playback_tx,
    ))
}

fn engine_thread(
    capture_tx: mpsc::Sender<SharedBuf<f32>>,
    mut playback_rx: mpsc::Receiver<SharedBuf<f32>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    init_tx: std::sync::mpsc::Sender<Result<()>>,
) {
    // All ObjC objects created and used on this single thread.
    unsafe {
        let engine = AVAudioEngine::new();

        let format = match AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            SAMPLE_RATE,
            CHANNELS,
        ) {
            Some(f) => f,
            None => {
                let _ = init_tx.send(Err(anyhow!("Failed to create AVAudioFormat")));
                return;
            }
        };

        // Start the engine *before* touching the input node.
        // Accessing engine.inputNode() implicitly connects it into the
        // graph; if voice processing hasn't been configured for the
        // hardware format yet, this leads to -10875 on start.
        //
        // Sequence (from Apple docs & empirical):
        //  1. Create engine
        //  2. Attach & connect the player node (output-only path)
        //  3. Start the engine (output path only, no input yet)
        //  4. Access inputNode, enable voice processing
        //  5. Stop & restart the engine (now with VP-configured input)
        //  6. Install the capture tap

        // --- Phase 1: output-only start ---
        let player = AVAudioPlayerNode::new();
        engine.attachNode(&player);
        let mixer = engine.mainMixerNode();
        engine.connect_to_format(&player, &mixer, Some(&format));

        if let Err(e) = engine.startAndReturnError() {
            let _ = init_tx.send(Err(anyhow!("Failed to start AVAudioEngine (phase 1): {:?}", e)));
            return;
        }

        // --- Phase 2: enable voice processing & restart ---
        engine.stop();

        let input_node = engine.inputNode();
        if let Err(e) = input_node.setVoiceProcessingEnabled_error(true) {
            let _ = init_tx.send(Err(anyhow!(
                "Failed to enable voice processing: {:?}",
                e
            )));
            return;
        }
        input_node.setVoiceProcessingAGCEnabled(true);

        // After enabling VP the input node's format may have changed.
        let vp_format = input_node.outputFormatForBus(0);
        let vp_sample_rate = vp_format.sampleRate();
        let vp_channels = vp_format.channelCount();
        tracing::info!(
            "Audio input format (after voice processing): {vp_sample_rate}Hz, {vp_channels} channels"
        );

        // Set up AVAudioConverter to resample/downmix from the VP
        // hardware format to our target 48 kHz mono format.
        let needs_conversion =
            (vp_sample_rate - SAMPLE_RATE).abs() >= 1.0 || vp_channels != CHANNELS;

        let converter: Option<objc2::rc::Retained<AVAudioConverter>> = if needs_conversion {
            match AVAudioConverter::initFromFormat_toFormat(
                AVAudioConverter::alloc(),
                &vp_format,
                &format,
            ) {
                Some(c) => {
                    tracing::info!("Created AVAudioConverter: {vp_sample_rate}Hz/{vp_channels}ch -> {SAMPLE_RATE}Hz/{CHANNELS}ch");
                    Some(c)
                }
                None => {
                    let _ = init_tx.send(Err(anyhow!(
                        "Failed to create AVAudioConverter ({vp_sample_rate}Hz/{vp_channels}ch -> {SAMPLE_RATE}Hz/{CHANNELS}ch)"
                    )));
                    return;
                }
            }
        } else {
            None
        };

        // Install tap using the VP node's native format.
        // The converter (if any) is used inside the tap to produce 48 kHz mono.
        let tap_block = RcBlock::new(move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            let input_buf = buffer.as_ref();
            let frame_length = input_buf.frameLength() as usize;
            if frame_length == 0 {
                return;
            }

            // If conversion is needed, run through AVAudioConverter.
            // Otherwise use the buffer directly.
            let (samples_ptr, samples_len) = if let Some(ref conv) = converter {
                // Compute output frame count based on sample rate ratio
                let ratio = SAMPLE_RATE / vp_sample_rate;
                let out_frames = (frame_length as f64 * ratio).ceil() as u32;

                let out_buf = match AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    conv.outputFormat().as_ref(),
                    out_frames,
                ) {
                    Some(b) => b,
                    None => return,
                };

                // The input block is called by the converter to get source data.
                // We supply our tap buffer exactly once.
                let input_buf_ptr = NonNull::from(input_buf);
                let supplied = std::cell::Cell::new(false);
                let input_block = RcBlock::new(
                    move |_: u32, out_status: NonNull<AVAudioConverterInputStatus>| -> *mut AVAudioPCMBuffer {
                        if supplied.get() {
                            out_status.as_ptr().write(AVAudioConverterInputStatus::EndOfStream);
                            return std::ptr::null_mut();
                        }
                        supplied.set(true);
                        out_status.as_ptr().write(AVAudioConverterInputStatus::HaveData);
                        input_buf_ptr.as_ptr() as *mut AVAudioPCMBuffer
                    },
                );

                let mut error: Option<objc2::rc::Retained<objc2_foundation::NSError>> = None;
                let _ = conv.convertToBuffer_error_withInputFromBlock(
                    &out_buf,
                    Some(&mut error),
                    &*input_block as *const _ as *mut _,
                );

                let converted_len = out_buf.frameLength() as usize;
                let ch_data = out_buf.floatChannelData();
                if ch_data.is_null() || converted_len == 0 {
                    return;
                }
                let ptr = (*ch_data).as_ptr();
                let slice = std::slice::from_raw_parts(ptr, converted_len);
                thread_local! {
                    static ACCUM: std::cell::RefCell<Vec<f32>> = std::cell::RefCell::new(Vec::with_capacity(CHUNK_SIZE * 2));
                    static POOL: BufPool<f32> = BufPool::new();
                }
                ACCUM.with(|accum| {
                    let mut accum = accum.borrow_mut();
                    accum.extend_from_slice(slice);
                    POOL.with(|pool| {
                        while accum.len() >= CHUNK_SIZE {
                            let mut buf = pool.checkout(CHUNK_SIZE);
                            buf.copy_from_slice(&accum[..CHUNK_SIZE]);
                            accum.drain(..CHUNK_SIZE);
                            let _ = capture_tx.try_send(buf.share());
                        }
                    });
                });
                return;
            } else {
                let ch_data = input_buf.floatChannelData();
                if ch_data.is_null() {
                    return;
                }
                ((*ch_data).as_ptr(), frame_length)
            };

            let samples = std::slice::from_raw_parts(samples_ptr, samples_len);

            thread_local! {
                static ACCUM2: std::cell::RefCell<Vec<f32>> = std::cell::RefCell::new(Vec::with_capacity(CHUNK_SIZE * 2));
                static POOL2: BufPool<f32> = BufPool::new();
            }
            ACCUM2.with(|accum| {
                let mut accum = accum.borrow_mut();
                accum.extend_from_slice(samples);
                POOL2.with(|pool| {
                    while accum.len() >= CHUNK_SIZE {
                        let mut buf = pool.checkout(CHUNK_SIZE);
                        buf.copy_from_slice(&accum[..CHUNK_SIZE]);
                        accum.drain(..CHUNK_SIZE);
                        let _ = capture_tx.try_send(buf.share());
                    }
                });
            });
        });

        input_node.installTapOnBus_bufferSize_format_block(
            0,
            CHUNK_SIZE as u32,
            Some(&vp_format),
            &*tap_block as *const _ as *mut _,
        );

        // Restart with input node now in the graph
        if let Err(e) = engine.startAndReturnError() {
            let _ = init_tx.send(Err(anyhow!("Failed to start AVAudioEngine (phase 2): {:?}", e)));
            return;
        }
        player.play();

        // Signal success
        let _ = init_tx.send(Ok(()));

        // Main loop: pump playback audio and check for stop signal
        let playback_format = format;
        loop {
            // Check stop
            match stop_rx.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            // Try to get playback audio (non-blocking, sleep if empty)
            match playback_rx.try_recv() {
                Ok(samples) => {
                    let frame_count = samples.len() as u32;
                    if let Some(pcm_buffer) = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                        AVAudioPCMBuffer::alloc(),
                        &playback_format,
                        frame_count,
                    ) {
                        pcm_buffer.setFrameLength(frame_count);
                        let channel_data = pcm_buffer.floatChannelData();
                        if !channel_data.is_null() {
                            let dst = (*channel_data).as_ptr();
                            std::ptr::copy_nonoverlapping(
                                samples.as_ptr(),
                                dst,
                                samples.len(),
                            );
                        }
                        player.scheduleBuffer_completionHandler(&pcm_buffer, std::ptr::null_mut());
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // Cleanup
        input_node.removeTapOnBus(0);
        engine.stop();
    }
}
