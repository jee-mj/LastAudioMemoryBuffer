use crate::error::Result;
use crate::sample_ring::SampleRing;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct FakeCapture {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeCapture {
    pub fn start(ring: Arc<SampleRing>, channels: u32, frames_per_tick: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut value = 0.0f32;
            while !thread_stop.load(Ordering::Relaxed) {
                let mut samples = Vec::with_capacity(frames_per_tick as usize * channels as usize);
                for _ in 0..frames_per_tick {
                    for _ in 0..channels {
                        samples.push(value);
                        value += 0.001;
                        if value > 1.0 {
                            value = -1.0;
                        }
                    }
                }
                let _ = ring.write_interleaved(&samples, channels);
                thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
