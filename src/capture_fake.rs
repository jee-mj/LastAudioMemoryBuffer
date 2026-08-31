use crate::capture_arena::CaptureIngress;
use crate::error::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct FakeCapture {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeCapture {
    pub fn start(ingress: CaptureIngress, channels: u32, frames_per_tick: u32) -> Result<Self> {
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
                let _ = ingress.try_push_interleaved(&samples, channels);
                thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_stops_and_joins_fake_capture_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_exited = Arc::clone(&exited);
        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            worker_exited.store(true, Ordering::Release);
        });
        let capture = FakeCapture {
            stop,
            handle: Some(handle),
        };
        drop(capture);
        assert!(exited.load(Ordering::Acquire));
    }
}
