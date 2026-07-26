use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::Receiver,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    error::XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, FramePixelFormat, LatestFrameSender,
        RecorderWorkerControl, VideoRecorderConfig, frame_interval, latest_frame_channel,
        set_current_thread_utility_priority,
    },
};

use super::impl_monitor::ImplMonitor;

#[derive(Debug, Clone)]
pub struct XorgVideoRecorder {
    monitor: ImplMonitor,
    worker_control: Arc<RecorderWorkerControl>,
    frame_interval: Duration,
    buffer_pool: Arc<FrameBufferPool>,
    latest_dropped: Arc<AtomicUsize>,
}

impl XorgVideoRecorder {
    pub fn new(
        monitor: ImplMonitor,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        if let Ok(source_size) = monitor.capture_dimensions() {
            let requested_size = config.output_dimensions(source_size.0, source_size.1, true);
            if requested_size != source_size {
                log::warn!(
                    "XGetImage cannot scale at the X server capture boundary; capturing native {}x{} instead of requested {}x{}",
                    source_size.0,
                    source_size.1,
                    requested_size.0,
                    requested_size.1
                );
            }
        }
        let (sender, receiver) = latest_frame_channel();
        let latest_dropped = sender.dropped_counter();
        let worker_control = RecorderWorkerControl::new();
        let recorder = Self {
            monitor,
            worker_control,
            frame_interval: frame_interval(config.fps),
            buffer_pool: FrameBufferPool::new(2),
            latest_dropped,
        };

        recorder.on_frame(sender)?;

        Ok((recorder, receiver))
    }

    fn on_frame(&self, sender: LatestFrameSender) -> XCapResult<()> {
        let monitor = self.monitor.clone();
        let recorder_waker = self.worker_control.waker();
        let shutdown = self.worker_control.shutdown_flag();
        let frame_interval = self.frame_interval;
        let buffer_pool = self.buffer_pool.clone();

        let worker = thread::spawn(move || {
            set_current_thread_utility_priority();
            let result = (|| -> XCapResult<()> {
                loop {
                    recorder_waker.wait()?;
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }

                    match monitor.capture_image() {
                        Ok(image) => {
                            let captured_at = SystemTime::now();
                            let captured_monotonic_at = Instant::now();
                            let width = image.width();
                            let height = image.height();
                            let raw = image.into_raw();
                            if let Some(mut buffer) = buffer_pool.try_acquire(raw.len()) {
                                buffer.copy_from_slice(&raw);
                                let frame = Frame::from_pooled(
                                    width,
                                    height,
                                    width as usize * 4,
                                    buffer,
                                    FramePixelFormat::Rgba8,
                                    captured_at,
                                    captured_monotonic_at,
                                    CaptureBackendKind::LinuxXorg,
                                );
                                if sender.send_latest(frame).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            log::error!("Failed to capture frame: {error:?}");
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    }

                    let _ = recorder_waker.wait_timeout_while_running(frame_interval)?;
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                log::error!("XGetImage recorder worker failed: {error:?}");
            }
        });
        self.worker_control.attach_worker(worker)?;

        Ok(())
    }

    pub fn start(&self) -> XCapResult<()> {
        self.worker_control.start()
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.worker_control.stop()
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.buffer_pool
            .dropped_frames()
            .saturating_add(self.latest_dropped.load(Ordering::Relaxed))
    }
}
