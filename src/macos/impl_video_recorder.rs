use std::{
    fmt, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant, SystemTime},
};

use block2::StackBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::{
    AllocAnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayBounds};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};
use scopeguard::defer;

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, FramePixelFormat, LatestFrameSender,
        VideoRecorderConfig, frame_interval, latest_frame_channel,
    },
};

use super::native_frame::NativeFramePool;
use super::{
    capture::fetch_shareable_content_for_display, impl_monitor::native_pixel_dimensions_for_display,
};

#[derive(Clone, Copy, Debug)]
enum PersistentStreamTerminal {
    NativeSurfaceIncompatible,
    StreamStopped { error_code: isize },
    StopFailed,
}

#[derive(Debug)]
struct PersistentStreamOutputVars {
    sender: LatestFrameSender,
    buffer_pool: Arc<FrameBufferPool>,
    native_frame_pool: Arc<NativeFramePool>,
    preserve_native_surface: bool,
    frame_sequence: AtomicU64,
    readiness_lock: Mutex<()>,
    readiness_changed: Condvar,
    terminal: AtomicBool,
    terminal_error: Mutex<Option<PersistentStreamTerminal>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "XCapPersistentStreamOutput"]
    #[ivars = PersistentStreamOutputVars]
    #[derive(Debug)]
    struct PersistentStreamOutput;

    unsafe impl SCStreamOutput for PersistentStreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            let Some(pixel_buffer) = (unsafe { CMSampleBuffer::image_buffer(sample_buffer) })
            else {
                return;
            };
            let captured_at = SystemTime::now();
            let captured_monotonic_at = Instant::now();
            let frame = if self.ivars().preserve_native_surface {
                match self.ivars().native_frame_pool.try_wrap(pixel_buffer) {
                    Ok(surface) => surface.map(|surface| {
                        Frame::from_native_surface(
                            surface,
                            captured_at,
                            captured_monotonic_at,
                            CaptureBackendKind::MacScreenCaptureKit,
                        )
                    }),
                    Err(_) => {
                        self.mark_terminal(PersistentStreamTerminal::NativeSurfaceIncompatible);
                        None
                    }
                }
            } else {
                pixel_buffer_to_bgra_frame(
                    &pixel_buffer,
                    &self.ivars().buffer_pool,
                    captured_at,
                    captured_monotonic_at,
                )
            };
            if let Some(frame) = frame {
                if self.ivars().sender.send_latest(frame).is_ok() {
                    self.ivars().frame_sequence.fetch_add(1, Ordering::Release);
                    self.ivars().readiness_changed.notify_all();
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for PersistentStreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            self.mark_terminal(PersistentStreamTerminal::StreamStopped {
                error_code: error.code(),
            });
        }
    }
);

unsafe impl NSObjectProtocol for PersistentStreamOutput {}

impl PersistentStreamOutput {
    fn new(
        sender: LatestFrameSender,
        buffer_pool: Arc<FrameBufferPool>,
        preserve_native_surface: bool,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PersistentStreamOutputVars {
            sender,
            buffer_pool,
            native_frame_pool: NativeFramePool::new(2),
            preserve_native_surface,
            frame_sequence: AtomicU64::new(0),
            readiness_lock: Mutex::new(()),
            readiness_changed: Condvar::new(),
            terminal: AtomicBool::new(false),
            terminal_error: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn mark_terminal(&self, terminal: PersistentStreamTerminal) {
        if self.ivars().terminal.load(Ordering::Acquire) {
            return;
        }
        let mut terminal_error = self
            .ivars()
            .terminal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.ivars().terminal.swap(true, Ordering::AcqRel) {
            return;
        }
        *terminal_error = Some(terminal);
        self.ivars().readiness_changed.notify_all();
    }

    fn terminal_error(&self) -> Option<String> {
        if !self.ivars().terminal.load(Ordering::Acquire) {
            return None;
        }
        self.ivars()
            .terminal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .map(|terminal| match terminal {
                PersistentStreamTerminal::NativeSurfaceIncompatible => {
                    "ScreenCaptureKit native surface is incompatible".to_string()
                }
                PersistentStreamTerminal::StreamStopped { error_code } => {
                    format!("ScreenCaptureKit stream stopped (NSError code {error_code})")
                }
                PersistentStreamTerminal::StopFailed => {
                    "ScreenCaptureKit stream could not be stopped cleanly".to_string()
                }
            })
    }
}

fn pixel_buffer_to_bgra_frame(
    pixel_buffer: &CVPixelBuffer,
    buffer_pool: &Arc<FrameBufferPool>,
    captured_at: SystemTime,
    captured_monotonic_at: Instant,
) -> Option<Frame> {
    unsafe {
        if CVPixelBufferGetPixelFormatType(pixel_buffer) != kCVPixelFormatType_32BGRA {
            return None;
        }
        CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        defer! {
            CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        }

        let width = CVPixelBufferGetWidth(pixel_buffer);
        let height = CVPixelBufferGetHeight(pixel_buffer);
        let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
        let base_address = CVPixelBufferGetBaseAddress(pixel_buffer);
        if base_address.is_null() {
            return None;
        }
        let compact_row_bytes = width * 4;
        let data = slice::from_raw_parts(base_address.cast::<u8>(), bytes_per_row * height);
        let compact_len = compact_row_bytes * height;
        let mut raw = buffer_pool.try_acquire(compact_len)?;
        if bytes_per_row == compact_row_bytes {
            raw.copy_from_slice(&data[..compact_len]);
        } else {
            for (source, destination) in data
                .chunks_exact(bytes_per_row)
                .zip(raw.chunks_exact_mut(compact_row_bytes))
            {
                destination.copy_from_slice(&source[..compact_row_bytes]);
            }
        }
        Some(Frame::from_pooled(
            width as u32,
            height as u32,
            compact_row_bytes,
            raw,
            FramePixelFormat::Bgra8,
            captured_at,
            captured_monotonic_at,
            CaptureBackendKind::MacScreenCaptureKit,
        ))
    }
}

struct RecorderInner {
    stream: Retained<SCStream>,
    output: Retained<PersistentStreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    lifecycle: Mutex<()>,
    started: AtomicBool,
    output_attached: AtomicBool,
    first_frame_timeout: Duration,
}

impl fmt::Debug for RecorderInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecorderInner")
            .field("started", &self.started.load(Ordering::Acquire))
            .field(
                "output_attached",
                &self.output_attached.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl RecorderInner {
    fn request_stop_without_waiting(&self) {
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(None);
        }
    }

    fn stop_capture_and_wait(&self) -> XCapResult<()> {
        let (sender, receiver) = mpsc::channel();
        let completion = StackBlock::new(move |error_ptr: *mut NSError| {
            let result = unsafe {
                error_ptr
                    .as_ref()
                    .map(|error| Err(error.localizedDescription().to_string()))
                    .unwrap_or(Ok(()))
            };
            let _ = sender.send(result);
        });
        unsafe {
            self.stream
                .stopCaptureWithCompletionHandler(Some(&completion));
        }
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(XCapError::new(message)),
            Err(error) => Err(XCapError::new(format!(
                "ScreenCaptureKit stop failed: {error}"
            ))),
        }
    }

    fn fail_start(&self, message: impl Into<String>) -> XCapResult<()> {
        let message = message.into();
        let cleanup_result = self.stop_capture_and_wait();
        match cleanup_result {
            Ok(()) => {
                self.started.store(false, Ordering::Release);
                Err(XCapError::new(message))
            }
            Err(cleanup_error) => {
                self.output
                    .mark_terminal(PersistentStreamTerminal::StopFailed);
                self.request_stop_without_waiting();
                Err(XCapError::new(format!(
                    "{message}; ScreenCaptureKit start cleanup failed: {cleanup_error}"
                )))
            }
        }
    }

    fn wait_for_first_frame(&self, previous_sequence: u64) -> XCapResult<()> {
        let deadline = Instant::now() + self.first_frame_timeout;
        let output = self.output.ivars();
        let mut readiness = output
            .readiness_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if output.frame_sequence.load(Ordering::Acquire) > previous_sequence {
                return Ok(());
            }
            if let Some(error) = self.output.terminal_error() {
                return self.fail_start(error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.fail_start(format!(
                    "ScreenCaptureKit did not deliver a valid first frame within {:.1}s",
                    self.first_frame_timeout.as_secs_f64()
                ));
            }
            let (next_readiness, wait_result) = output
                .readiness_changed
                .wait_timeout(readiness, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            readiness = next_readiness;
            if wait_result.timed_out()
                && output.frame_sequence.load(Ordering::Acquire) <= previous_sequence
            {
                return self.fail_start(format!(
                    "ScreenCaptureKit did not deliver a valid first frame within {:.1}s",
                    self.first_frame_timeout.as_secs_f64()
                ));
            }
        }
    }

    fn start(&self) -> XCapResult<()> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(error) = self.output.terminal_error() {
            return Err(XCapError::new(error));
        }
        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let previous_sequence = self.output.ivars().frame_sequence.load(Ordering::Acquire);
        let (sender, receiver) = mpsc::channel();
        let completion = StackBlock::new(move |error_ptr: *mut NSError| {
            let result = unsafe {
                error_ptr
                    .as_ref()
                    .map(|error| Err(error.localizedDescription().to_string()))
                    .unwrap_or(Ok(()))
            };
            let _ = sender.send(result);
        });
        unsafe {
            self.stream
                .startCaptureWithCompletionHandler(Some(&completion));
        }
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => self.wait_for_first_frame(previous_sequence),
            Ok(Err(message)) => self.fail_start(message),
            Err(error) => {
                self.fail_start(format!("ScreenCaptureKit start readiness failed: {error}"))
            }
        }
    }

    fn stop(&self) -> XCapResult<()> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self.stop_capture_and_wait();
        match result {
            Ok(()) => {
                self.started.store(false, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.output
                    .mark_terminal(PersistentStreamTerminal::StopFailed);
                self.request_stop_without_waiting();
                Err(error)
            }
        }
    }
}

impl Drop for RecorderInner {
    fn drop(&mut self) {
        if self.started.swap(false, Ordering::AcqRel) {
            if self.stop_capture_and_wait().is_err() {
                self.request_stop_without_waiting();
            }
        }
        if self.output_attached.swap(false, Ordering::AcqRel) {
            let output = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*self.output);
            unsafe {
                let _ = self
                    .stream
                    .removeStreamOutput_type_error(output.as_ref(), SCStreamOutputType::Screen);
            }
        }
    }
}

#[derive(Clone)]
pub struct ImplVideoRecorder {
    inner: Arc<RecorderInner>,
}

impl fmt::Debug for ImplVideoRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImplVideoRecorder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl ImplVideoRecorder {
    pub fn new(
        display_id: CGDirectDisplayID,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        unsafe {
            // Recorder construction is infrequent and follows topology/config
            // changes. Force a fresh ScreenCaptureKit snapshot here so a
            // same-ID resolution or rotation change cannot reuse stale
            // SCDisplay metadata for a long-lived session.
            let (_shareable_content, display) =
                fetch_shareable_content_for_display(false, display_id, true)?;

            let excluded_windows = NSArray::new();
            let content_filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display.as_ref(),
                excluded_windows.as_ref(),
            );
            let bounds = CGDisplayBounds(display_id);
            let legacy_source_width = bounds.size.width.max(1.0).round() as u32;
            let legacy_source_height = bounds.size.height.max(1.0).round() as u32;
            let (source_width, source_height) = if config.output_size.is_some() {
                // Explicit output sizing is based on physical platform pixels.
                // An invalid/missing mode is a topology transition, not a
                // logical-size substitute: fail readiness so the caller can
                // rebuild or fall back without silently recording the wrong
                // geometry.
                native_pixel_dimensions_for_display(display_id)?
            } else {
                // Keep the historical logical-pixel source dimensions for
                // callers that only use scale_factor/max_pixels.
                (legacy_source_width, legacy_source_height)
            };
            let (mut width, mut height) =
                config.output_dimensions(source_width, source_height, true);
            if config.preserve_native_surface {
                // AVAssetWriter's hardware HEVC/H.264 encoders consume
                // 4:2:0 surfaces and reject odd frame dimensions with
                // kVTCouldNotFindVideoEncoderErr / AVErrorEncoderNotFound.
                // Scale at the ScreenCaptureKit output boundary so the native
                // path remains zero-copy instead of padding after capture.
                width = encoder_compatible_dimension(width);
                height = encoder_compatible_dimension(height);
            }

            let stream_config = SCStreamConfiguration::new();
            stream_config.setWidth(width as usize);
            stream_config.setHeight(height as usize);
            stream_config.setPixelFormat(kCVPixelFormatType_32BGRA);
            stream_config.setQueueDepth(1);
            stream_config.setScalesToFit(true);
            stream_config.setMinimumFrameInterval(CMTime::with_seconds(
                frame_interval(config.fps).as_secs_f64(),
                60_000,
            ));

            let (sender, receiver) = latest_frame_channel();
            let buffer_pool = FrameBufferPool::new(2);
            let output =
                PersistentStreamOutput::new(sender, buffer_pool, config.preserve_native_surface);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                content_filter.as_ref(),
                stream_config.as_ref(),
                Some(ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output)),
            );
            let output_protocol = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
            let queue = DispatchQueue::new(
                "XCapPersistentScreenCaptureKitOutput",
                DispatchQueueAttr::SERIAL,
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    output_protocol.as_ref(),
                    SCStreamOutputType::Screen,
                    Some(queue.as_ref()),
                )
                .map_err(|error| XCapError::new(error.localizedDescription().to_string()))?;

            Ok((
                Self {
                    inner: Arc::new(RecorderInner {
                        stream,
                        output,
                        _queue: queue,
                        lifecycle: Mutex::new(()),
                        started: AtomicBool::new(false),
                        output_attached: AtomicBool::new(true),
                        first_frame_timeout: frame_interval(config.fps)
                            .mul_f64(2.0)
                            .clamp(Duration::from_secs(3), Duration::from_secs(30)),
                    }),
                },
                receiver,
            ))
        }
    }

    pub fn start(&self) -> XCapResult<()> {
        self.inner.start()
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.inner.stop()
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        let output = self.inner.output.ivars();
        output
            .buffer_pool
            .dropped_frames()
            .saturating_add(output.native_frame_pool.dropped_frames())
            .saturating_add(output.sender.dropped_frames())
    }

    pub(crate) fn terminal_error(&self) -> Option<String> {
        self.inner.output.terminal_error()
    }
}

fn encoder_compatible_dimension(value: u32) -> u32 {
    value.max(2) & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_error_is_a_single_permanent_state_transition() {
        let (sender, _receiver) = latest_frame_channel();
        let output = PersistentStreamOutput::new(sender, FrameBufferPool::new(2), false);

        output.mark_terminal(PersistentStreamTerminal::NativeSurfaceIncompatible);
        output.mark_terminal(PersistentStreamTerminal::StreamStopped { error_code: 42 });
        assert_eq!(
            output.terminal_error().as_deref(),
            Some("ScreenCaptureKit native surface is incompatible")
        );

        output.mark_terminal(PersistentStreamTerminal::StopFailed);
        assert_eq!(
            output.terminal_error().as_deref(),
            Some("ScreenCaptureKit native surface is incompatible")
        );
    }

    #[test]
    fn native_capture_dimensions_are_even_for_hardware_encoders() {
        assert_eq!(encoder_compatible_dimension(1728), 1728);
        assert_eq!(encoder_compatible_dimension(1117), 1116);
        assert_eq!(encoder_compatible_dimension(1), 2);
    }
}
