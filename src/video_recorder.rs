use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use crate::{XCapError, XCapResult, platform::impl_video_recorder::ImplVideoRecorder};

#[cfg(target_os = "macos")]
use crate::platform::native_frame::NativeFrameSurface;

#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramePixelFormat {
    Rgba8,
    Bgra8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBackendKind {
    Unknown,
    MacScreenCaptureKit,
    WindowsDxgi,
    WindowsGraphicsCapture,
    LinuxPipeWire,
    LinuxXorg,
    LinuxXShm,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub raw: FrameBuffer,
    pub pixel_format: FramePixelFormat,
    /// Wall-clock time captured at the platform callback/acquisition boundary.
    pub captured_at: SystemTime,
    /// Monotonic companion used to measure callback-to-consumer latency.
    pub captured_monotonic_at: Instant,
    /// Concrete producer used for this frame, including runtime fallback.
    pub backend_kind: CaptureBackendKind,
    /// Retained IOSurface-backed ScreenCaptureKit frame. This is populated
    /// only when explicitly requested; compatibility capture keeps using
    /// `raw`.
    #[cfg(target_os = "macos")]
    pub native_surface: Option<NativeFrameSurface>,
}

pub struct FrameBuffer {
    data: Option<Vec<u8>>,
    pool: Option<Arc<FrameBufferPool>>,
}

impl FrameBuffer {
    fn owned(data: Vec<u8>) -> Self {
        Self {
            data: Some(data),
            pool: None,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data.as_deref().unwrap_or_default()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data.as_deref_mut().unwrap_or_default()
    }
}

impl Clone for FrameBuffer {
    fn clone(&self) -> Self {
        Self::owned(self.as_slice().to_vec())
    }
}

impl fmt::Debug for FrameBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameBuffer")
            .field("len", &self.len())
            .field("pooled", &self.pool.is_some())
            .finish()
    }
}

impl Deref for FrameBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl DerefMut for FrameBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        if let (Some(pool), Some(mut data)) = (self.pool.as_ref(), self.data.take()) {
            data.clear();
            pool.recycle(data);
        }
    }
}

#[derive(Debug)]
pub(crate) struct FrameBufferPool {
    max_buffers: usize,
    allocated: AtomicUsize,
    dropped: AtomicUsize,
    available: Mutex<Vec<Vec<u8>>>,
}

impl FrameBufferPool {
    pub(crate) fn new(max_buffers: usize) -> Arc<Self> {
        Arc::new(Self {
            max_buffers: max_buffers.max(1),
            allocated: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            available: Mutex::new(Vec::with_capacity(max_buffers.max(1))),
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>, len: usize) -> Option<FrameBuffer> {
        let buffer = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .or_else(|| {
                let mut allocated = self.allocated.load(Ordering::Acquire);
                loop {
                    if allocated >= self.max_buffers {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    match self.allocated.compare_exchange_weak(
                        allocated,
                        allocated + 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break Some(Vec::with_capacity(len)),
                        Err(actual) => allocated = actual,
                    }
                }
            })?;
        let mut buffer = buffer;
        buffer.resize(len, 0);
        Some(FrameBuffer {
            data: Some(buffer),
            pool: Some(self.clone()),
        })
    }

    fn recycle(&self, buffer: Vec<u8>) {
        self.available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(buffer);
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoRecorderConfig {
    pub fps: f64,
    /// Preferred platform capture scale. `None` keeps the platform default.
    pub scale_factor: Option<f32>,
    /// Preferred upper bound for captured pixels.
    pub max_pixels: Option<u32>,
    /// Prefer Windows Graphics Capture when its build feature is present.
    /// Initialization failure must fall back to DXGI.
    pub prefer_windows_wgc: bool,
    /// Prefer persistent X11 MIT-SHM capture when its build feature is
    /// present. Initialization failure must fall back to XGetImage.
    pub prefer_x11_xshm: bool,
    /// Preserve ScreenCaptureKit's CVPixelBuffer so a native VideoToolbox
    /// writer can encode it without copying through FFmpeg stdin.
    pub preserve_native_surface: bool,
}

impl Default for VideoRecorderConfig {
    fn default() -> Self {
        Self {
            fps: 30.0,
            scale_factor: None,
            max_pixels: None,
            prefer_windows_wgc: false,
            prefer_x11_xshm: false,
            preserve_native_surface: false,
        }
    }
}

impl VideoRecorderConfig {
    pub(crate) fn output_dimensions(
        self,
        width: u32,
        height: u32,
        apply_scale: bool,
    ) -> (u32, u32) {
        let scale = if apply_scale {
            self.scale_factor
                .filter(|scale| scale.is_finite() && *scale > 0.0)
                .unwrap_or(1.0)
        } else {
            1.0
        };
        let mut output_width = ((width as f64 * f64::from(scale)).round() as u32).max(1);
        let mut output_height = ((height as f64 * f64::from(scale)).round() as u32).max(1);
        if let Some(max_pixels) = self.max_pixels.filter(|max_pixels| *max_pixels > 0) {
            let pixels = u64::from(output_width) * u64::from(output_height);
            if pixels > u64::from(max_pixels) {
                let factor = (f64::from(max_pixels) / pixels as f64).sqrt();
                output_width = ((f64::from(output_width) * factor).floor() as u32).max(1);
                output_height = ((f64::from(output_height) * factor).floor() as u32).max(1);
            }
        }
        (output_width, output_height)
    }
}

impl Frame {
    pub fn new(width: u32, height: u32, raw: Vec<u8>) -> Self {
        let captured_at = SystemTime::now();
        let captured_monotonic_at = Instant::now();
        Self {
            width,
            height,
            stride: width as usize * 4,
            raw: FrameBuffer::owned(raw),
            pixel_format: FramePixelFormat::Rgba8,
            captured_at,
            captured_monotonic_at,
            backend_kind: CaptureBackendKind::Unknown,
            #[cfg(target_os = "macos")]
            native_surface: None,
        }
    }

    pub fn new_bgra(width: u32, height: u32, raw: Vec<u8>) -> Self {
        let captured_at = SystemTime::now();
        let captured_monotonic_at = Instant::now();
        Self {
            width,
            height,
            stride: width as usize * 4,
            raw: FrameBuffer::owned(raw),
            pixel_format: FramePixelFormat::Bgra8,
            captured_at,
            captured_monotonic_at,
            backend_kind: CaptureBackendKind::Unknown,
            #[cfg(target_os = "macos")]
            native_surface: None,
        }
    }

    pub(crate) fn from_pooled(
        width: u32,
        height: u32,
        stride: usize,
        raw: FrameBuffer,
        pixel_format: FramePixelFormat,
        captured_at: SystemTime,
        captured_monotonic_at: Instant,
        backend_kind: CaptureBackendKind,
    ) -> Self {
        Self {
            width,
            height,
            stride,
            raw,
            pixel_format,
            captured_at,
            captured_monotonic_at,
            backend_kind,
            #[cfg(target_os = "macos")]
            native_surface: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn from_native_surface(
        surface: NativeFrameSurface,
        captured_at: SystemTime,
        captured_monotonic_at: Instant,
        backend_kind: CaptureBackendKind,
    ) -> Self {
        Self {
            width: surface.width(),
            height: surface.height(),
            stride: surface.stride(),
            raw: FrameBuffer::owned(Vec::new()),
            pixel_format: FramePixelFormat::Bgra8,
            captured_at,
            captured_monotonic_at,
            backend_kind,
            native_surface: Some(surface),
        }
    }

    pub fn byte_len(&self) -> usize {
        #[cfg(target_os = "macos")]
        if let Some(surface) = self.native_surface.as_ref() {
            return surface.byte_len();
        }
        self.raw.len()
    }
}

#[derive(Debug)]
struct LatestFrameState {
    receiver_alive: AtomicBool,
    sender_count: AtomicUsize,
    dropped: Arc<AtomicUsize>,
    slot: Mutex<Option<Frame>>,
    available: Condvar,
}

/// Non-blocking producer side of the video recorder's bounded latest-frame
/// bridge. A blocked public receiver can retain at most one in-flight frame
/// while this slot retains one newer replacement.
#[derive(Debug)]
pub(crate) struct LatestFrameSender {
    state: Arc<LatestFrameState>,
}

impl LatestFrameSender {
    pub(crate) fn send_latest(&self, frame: Frame) -> Result<Option<Frame>, Frame> {
        if !self.state.receiver_alive.load(Ordering::Acquire) {
            return Err(frame);
        }
        let displaced = {
            let mut slot = self
                .state
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !self.state.receiver_alive.load(Ordering::Acquire) {
                return Err(frame);
            }
            slot.replace(frame)
        };
        if displaced.is_some() {
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.state.available.notify_one();
        Ok(displaced)
    }

    pub(crate) fn dropped_counter(&self) -> Arc<AtomicUsize> {
        self.state.dropped.clone()
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Clone for LatestFrameSender {
    fn clone(&self) -> Self {
        self.state.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            state: self.state.clone(),
        }
    }
}

impl Drop for LatestFrameSender {
    fn drop(&mut self) {
        if self.state.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.state.available.notify_all();
        }
    }
}

pub(crate) fn latest_frame_channel() -> (LatestFrameSender, Receiver<Frame>) {
    let state = Arc::new(LatestFrameState {
        receiver_alive: AtomicBool::new(true),
        sender_count: AtomicUsize::new(1),
        dropped: Arc::new(AtomicUsize::new(0)),
        slot: Mutex::new(None),
        available: Condvar::new(),
    });
    let sender = LatestFrameSender {
        state: state.clone(),
    };
    let (public_sender, public_receiver) = sync_channel(0);
    thread::spawn(move || {
        // This bridge only moves the newest owned frame into the public
        // receiver. Native ScreenCaptureKit/PipeWire callbacks retain their
        // platform-managed priority.
        set_current_thread_utility_priority();
        loop {
            let frame = {
                let mut slot = state
                    .slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                while slot.is_none() && state.sender_count.load(Ordering::Acquire) > 0 {
                    slot = state
                        .available
                        .wait(slot)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                match slot.take() {
                    Some(frame) => frame,
                    None => break,
                }
            };
            if public_sender.send(frame).is_err() {
                state.receiver_alive.store(false, Ordering::Release);
                let pending = {
                    let mut slot = state
                        .slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slot.take()
                };
                drop(pending);
                break;
            }
        }
        state.receiver_alive.store(false, Ordering::Release);
    });
    (sender, public_receiver)
}

/// Lowers xcap-owned, long-lived polling/bridge workers. Platform realtime
/// callbacks intentionally do not call this helper.
pub(crate) fn set_current_thread_utility_priority() -> bool {
    #[cfg(target_os = "macos")]
    {
        return unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) == 0
        };
    }

    #[cfg(target_os = "linux")]
    {
        return unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 5) == 0 };
    }

    #[cfg(windows)]
    {
        return unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) }
            .is_ok();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        false
    }
}

pub(crate) fn sanitized_fps(fps: f64) -> f64 {
    if fps.is_finite() && fps > 0.0 {
        fps.clamp(0.1, 30.0)
    } else {
        30.0
    }
}

pub(crate) fn frame_interval(fps: f64) -> Duration {
    Duration::from_secs_f64(1.0 / sanitized_fps(fps))
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct RecorderWaker {
    parking: Mutex<bool>,
    condvar: Condvar,
}

#[derive(Debug)]
pub(crate) struct RecorderWorkerControl {
    recorder_waker: Arc<RecorderWaker>,
    shutdown: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RecorderWorkerControl {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            recorder_waker: Arc::new(RecorderWaker::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        })
    }

    pub(crate) fn waker(&self) -> Arc<RecorderWaker> {
        self.recorder_waker.clone()
    }

    pub(crate) fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    pub(crate) fn attach_worker(&self, worker: JoinHandle<()>) -> XCapResult<()> {
        let mut worker_slot = self.worker.lock()?;
        if worker_slot.is_some() {
            return Err(XCapError::new("recorder worker is already attached"));
        }
        *worker_slot = Some(worker);
        Ok(())
    }

    pub(crate) fn start(&self) -> XCapResult<()> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(XCapError::new("recorder worker is shutting down"));
        }
        self.recorder_waker.wake()
    }

    pub(crate) fn stop(&self) -> XCapResult<()> {
        self.recorder_waker.sleep()
    }
}

impl Drop for RecorderWorkerControl {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.recorder_waker.wake();
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl RecorderWaker {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            parking: Mutex::new(true),
            condvar: Condvar::new(),
        }
    }
    #[allow(dead_code)]
    pub fn wake(&self) -> XCapResult<()> {
        let mut parking = self.parking.lock()?;
        *parking = false;
        self.condvar.notify_one();

        Ok(())
    }
    #[allow(dead_code)]
    pub fn sleep(&self) -> XCapResult<()> {
        let mut parking = self.parking.lock()?;
        *parking = true;
        self.condvar.notify_all();

        Ok(())
    }
    #[allow(dead_code)]
    pub fn wait(&self) -> XCapResult<()> {
        let mut parking = self.parking.lock()?;
        while *parking {
            parking = self.condvar.wait(parking)?;
        }

        Ok(())
    }

    /// Waits for the next capture deadline while allowing `sleep()` to wake
    /// the recorder immediately.
    #[allow(dead_code)]
    pub fn wait_timeout_while_running(&self, timeout: Duration) -> XCapResult<bool> {
        let parking = self.parking.lock()?;
        if *parking {
            return Ok(false);
        }
        let (parking, _) = self.condvar.wait_timeout(parking, timeout)?;
        Ok(!*parking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8) -> Frame {
        Frame::new(1, 1, vec![value; 4])
    }

    #[test]
    fn latest_bridge_replaces_stale_pending_frame() {
        let (sender, receiver) = latest_frame_channel();
        sender.send_latest(frame(1)).unwrap();
        // Wait until the dispatcher has taken frame 1 and is blocked on the
        // zero-capacity public handoff. Avoid a timing-based sleep so this
        // remains deterministic on slow CI workers.
        for _ in 0..10_000 {
            if sender
                .state
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
            {
                break;
            }
            thread::yield_now();
        }
        assert!(
            sender
                .state
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        );
        // The dispatcher blocks delivering frame 1 while the public receiver
        // is idle; its one pending slot must keep only frame 3.
        sender.send_latest(frame(2)).unwrap();
        sender.send_latest(frame(3)).unwrap();
        assert_eq!(sender.dropped_frames(), 1);
        assert_eq!(receiver.recv().unwrap().raw[0], 1);
        assert_eq!(receiver.recv().unwrap().raw[0], 3);
    }

    #[test]
    fn frame_interval_is_bounded() {
        assert_eq!(frame_interval(0.0), Duration::from_secs_f64(1.0 / 30.0));
        assert_eq!(frame_interval(-1.0), Duration::from_secs_f64(1.0 / 30.0));
        assert_eq!(
            frame_interval(f64::NAN),
            Duration::from_secs_f64(1.0 / 30.0)
        );
        assert_eq!(
            frame_interval(f64::INFINITY),
            Duration::from_secs_f64(1.0 / 30.0)
        );
        assert_eq!(
            frame_interval(f64::NEG_INFINITY),
            Duration::from_secs_f64(1.0 / 30.0)
        );
        assert_eq!(frame_interval(60.0), Duration::from_secs_f64(1.0 / 30.0));
        assert_eq!(frame_interval(0.5), Duration::from_secs(2));
    }

    #[test]
    fn two_buffer_pool_drops_third_and_reuses_after_drop() {
        let pool = FrameBufferPool::new(2);
        let first = pool.try_acquire(16).unwrap();
        let second = pool.try_acquire(16).unwrap();
        assert!(pool.try_acquire(16).is_none());
        assert_eq!(pool.dropped_frames(), 1);
        drop(first);
        assert!(pool.try_acquire(16).is_some());
        drop(second);
    }
}

#[derive(Debug, Clone)]
pub struct VideoRecorder {
    impl_video_recorder: ImplVideoRecorder,
}

impl VideoRecorder {
    pub(crate) fn new(impl_video_recorder: ImplVideoRecorder) -> VideoRecorder {
        VideoRecorder {
            impl_video_recorder,
        }
    }

    pub fn start(&self) -> XCapResult<()> {
        self.impl_video_recorder.start()
    }
    pub fn stop(&self) -> XCapResult<()> {
        self.impl_video_recorder.stop()
    }

    /// Frames intentionally discarded before the public consumer could
    /// receive them. This includes bounded capture-buffer exhaustion and
    /// replacement of stale latest-frame slots.
    pub fn dropped_frames(&self) -> usize {
        self.impl_video_recorder.dropped_frames()
    }

    /// Returns a terminal platform-capture error reported asynchronously
    /// after this recorder had become ready.
    pub fn terminal_error(&self) -> Option<String> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            return self.impl_video_recorder.terminal_error();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            None
        }
    }
}
