use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
use crate::audio_macos;

/// Return type for `AudioEngine::start()`.
pub type AudioStartResult = Result<(AudioEngine, mpsc::Receiver<Vec<f32>>, mpsc::Sender<Vec<f32>>)>;

/// Platform-native audio engine with echo cancellation.
///
/// A single engine owns both capture and playback because AEC requires
/// the OS to know exactly what is being played through the speakers in
/// order to subtract it from the microphone signal.
pub struct AudioEngine {
    pub(crate) stop_tx: Option<std::sync::mpsc::Sender<()>>,
    pub(crate) thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioEngine {
    /// Start the audio engine. Returns:
    /// - The engine handle (drop to stop)
    /// - A receiver of echo-cancelled 960-sample f32 chunks from the microphone
    /// - A sender for decoded remote audio (960-sample f32 chunks) to play
    pub fn start() -> AudioStartResult {
        #[cfg(target_os = "macos")]
        {
            audio_macos::start()
        }

        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!("Audio not implemented for this platform")
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}
