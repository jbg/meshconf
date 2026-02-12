use std::ptr::NonNull;

use anyhow::{anyhow, Result};
use block2::RcBlock;
use objc2::AnyThread;
use objc2_avf_audio::{
    AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioPlayerNode, AVAudioTime,
};
use tokio::sync::mpsc;

use crate::audio::AudioEngine;

const SAMPLE_RATE: f64 = 48000.0;
const CHANNELS: u32 = 1;
const CHUNK_SIZE: usize = 960; // 20ms @ 48kHz

pub fn start() -> crate::audio::AudioStartResult {
    let (capture_tx, capture_rx) = mpsc::channel::<Vec<f32>>(10);
    let (playback_tx, playback_rx) = mpsc::channel::<Vec<f32>>(50);
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
    capture_tx: mpsc::Sender<Vec<f32>>,
    mut playback_rx: mpsc::Receiver<Vec<f32>>,
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

        // Enable voice processing on input node (AEC + noise suppression)
        let input_node = engine.inputNode();
        if let Err(e) = input_node.setVoiceProcessingEnabled_error(true) {
            let _ = init_tx.send(Err(anyhow!(
                "Failed to enable voice processing: {:?}",
                e
            )));
            return;
        }
        input_node.setVoiceProcessingAGCEnabled(true);

        // Install tap on input node for capture
        let tap_block = RcBlock::new(move |buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            let buf = buffer.as_ref();
            let frame_length = buf.frameLength() as usize;
            if frame_length == 0 {
                return;
            }
            let channel_data_ptr = buf.floatChannelData();
            if channel_data_ptr.is_null() {
                return;
            }
            // channel_data_ptr is *mut NonNull<f32>, pointing to array of channel pointers
            let ch0_ptr = (*channel_data_ptr).as_ptr();
            let samples = std::slice::from_raw_parts(ch0_ptr, frame_length);

            // Accumulate and emit CHUNK_SIZE chunks
            // We use a thread-local buffer since this callback is always on the same CoreAudio thread
            thread_local! {
                static ACCUM: std::cell::RefCell<Vec<f32>> = std::cell::RefCell::new(Vec::with_capacity(CHUNK_SIZE * 2));
            }
            ACCUM.with(|accum| {
                let mut accum = accum.borrow_mut();
                accum.extend_from_slice(samples);
                while accum.len() >= CHUNK_SIZE {
                    let chunk: Vec<f32> = accum.drain(..CHUNK_SIZE).collect();
                    let _ = capture_tx.try_send(chunk);
                }
            });
        });

        input_node.installTapOnBus_bufferSize_format_block(
            0,
            CHUNK_SIZE as u32,
            Some(&format),
            &*tap_block as *const _ as *mut _,
        );

        // Create player node for playback
        let player = AVAudioPlayerNode::new();
        engine.attachNode(&player);
        engine.connect_to_format(&player, &engine.mainMixerNode(), Some(&format));

        // Start engine
        if let Err(e) = engine.startAndReturnError() {
            let _ = init_tx.send(Err(anyhow!("Failed to start AVAudioEngine: {:?}", e)));
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
