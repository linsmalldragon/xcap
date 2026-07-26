use std::{
    fmt, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use block2::StackBlock;
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeVideo, AVVideoAllowFrameReorderingKey, AVVideoAverageBitRateKey,
    AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC, AVVideoCompressionPropertiesKey,
    AVVideoEncoderSpecificationKey, AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey,
    AVVideoMaxKeyFrameIntervalKey, AVVideoWidthKey,
};
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use crate::{NativeFrameSurface, XCapError, XCapResult};

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
const PTS_TIMESCALE: i32 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVideoCodec {
    Hevc,
    H264,
}

/// A serial AVAssetWriter/VideoToolbox session. Frames remain as retained
/// CVPixelBuffers from ScreenCaptureKit and never travel through FFmpeg stdin.
pub struct NativeVideoWriter {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    partial_path: PathBuf,
    final_path: PathBuf,
    fps: f64,
    width: u32,
    height: u32,
    codec: NativeVideoCodec,
    appended_frames: usize,
    finished: bool,
}

// Calls are serialized by the owning recorder task. AVAssetWriter's status
// and error properties are thread-safe; the session is never accessed
// concurrently even if Tokio moves its owner between worker threads.
unsafe impl Send for NativeVideoWriter {}

impl fmt::Debug for NativeVideoWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeVideoWriter")
            .field("partial_path", &self.partial_path)
            .field("final_path", &self.final_path)
            .field("fps", &self.fps)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("codec", &self.codec)
            .field("appended_frames", &self.appended_frames)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct NativeVideoWriterFinish {
    pub partial_path: PathBuf,
    pub final_path: PathBuf,
    pub codec: NativeVideoCodec,
    pub frame_count: usize,
}

impl NativeVideoWriter {
    pub fn start(
        final_path: impl AsRef<Path>,
        fps: f64,
        width: u32,
        height: u32,
    ) -> XCapResult<Self> {
        if !fps.is_finite() || fps <= 0.0 || width == 0 || height == 0 {
            return Err(XCapError::new(
                "invalid native video writer dimensions or FPS",
            ));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(XCapError::new(format!(
                "native hardware HEVC/H.264 requires even frame dimensions, got {width}x{height}"
            )));
        }
        let final_path = final_path.as_ref().to_path_buf();
        let partial_path = partial_path_for_final(&final_path)?;
        ensure_partial_absent(&partial_path)?;

        match Self::start_with_codec(
            final_path.clone(),
            partial_path.clone(),
            fps,
            width,
            height,
            NativeVideoCodec::Hevc,
        ) {
            Ok(writer) => Ok(writer),
            Err(hevc_error) => {
                remove_failed_start_partial(&partial_path)?;
                match Self::start_with_codec(
                    final_path,
                    partial_path.clone(),
                    fps,
                    width,
                    height,
                    NativeVideoCodec::H264,
                ) {
                    Ok(writer) => Ok(writer),
                    Err(h264_error) => {
                        remove_failed_start_partial(&partial_path)?;
                        Err(XCapError::new(format!(
                            "native HEVC unavailable ({hevc_error}); H.264 unavailable ({h264_error})"
                        )))
                    }
                }
            }
        }
    }

    pub fn append(
        &mut self,
        surface: &NativeFrameSurface,
        effective_frame_index: usize,
    ) -> XCapResult<()> {
        if !self.try_append(surface, effective_frame_index)? {
            return Err(XCapError::new(
                "native VideoToolbox input is temporarily backpressured",
            ));
        }
        Ok(())
    }

    /// Attempts one non-blocking hand-off to AVAssetWriter.
    ///
    /// The caller owns retry scheduling so a temporarily saturated hardware
    /// encoder never blocks an async executor or a screen-capture callback.
    pub fn try_append(
        &mut self,
        surface: &NativeFrameSurface,
        effective_frame_index: usize,
    ) -> XCapResult<bool> {
        if self.finished {
            return Err(XCapError::new("native video writer is already finished"));
        }
        if let Err(error) =
            self.fail_if_writer_failed("native VideoToolbox encoder failed before append")
        {
            return self.retry_first_frame_with_h264(surface, effective_frame_index, error);
        }
        if !unsafe { self.input.isReadyForMoreMediaData() } {
            if let Err(error) = self.fail_if_writer_failed(
                "native VideoToolbox encoder failed while reporting backpressure",
            ) {
                return self.retry_first_frame_with_h264(surface, effective_frame_index, error);
            }
            return Ok(false);
        }
        let presentation_seconds = effective_frame_index.saturating_sub(1) as f64 / self.fps;
        let presentation_time =
            unsafe { CMTime::with_seconds(presentation_seconds, PTS_TIMESCALE) };
        let appended = unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(surface.pixel_buffer(), presentation_time)
        };
        if !appended {
            let error = self.writer_error("VideoToolbox rejected the native frame");
            return self.retry_first_frame_with_h264(surface, effective_frame_index, error);
        }
        self.appended_frames = self.appended_frames.saturating_add(1);
        Ok(true)
    }

    fn fail_if_writer_failed(&self, context: &str) -> XCapResult<()> {
        let status = unsafe { self.writer.status() };
        if status == AVAssetWriterStatus::Failed || status == AVAssetWriterStatus::Cancelled {
            return Err(self.writer_error(context));
        }
        Ok(())
    }

    fn writer_error(&self, context: &str) -> XCapError {
        writer_error(
            &self.writer,
            &format!(
                "{context} (codec={:?}, dimensions={}x{}, fps={:.3})",
                self.codec, self.width, self.height, self.fps
            ),
        )
    }

    fn retry_first_frame_with_h264(
        &mut self,
        surface: &NativeFrameSurface,
        effective_frame_index: usize,
        hevc_error: XCapError,
    ) -> XCapResult<bool> {
        if self.codec != NativeVideoCodec::Hevc || self.appended_frames != 0 {
            return Err(hevc_error);
        }
        unsafe {
            self.writer.cancelWriting();
        }
        self.finished = true;
        remove_failed_start_partial(&self.partial_path)?;
        let replacement = match Self::start_with_codec(
            self.final_path.clone(),
            self.partial_path.clone(),
            self.fps,
            self.width,
            self.height,
            NativeVideoCodec::H264,
        ) {
            Ok(writer) => writer,
            Err(h264_error) => {
                remove_failed_start_partial(&self.partial_path)?;
                return Err(XCapError::new(format!(
                    "native HEVC failed on first frame ({hevc_error}); H.264 unavailable ({h264_error})"
                )));
            }
        };
        *self = replacement;
        self.try_append(surface, effective_frame_index)
    }

    pub fn finish(mut self) -> XCapResult<NativeVideoWriterFinish> {
        if self.appended_frames == 0 {
            unsafe {
                self.writer.cancelWriting();
            }
            self.finished = true;
            return Err(XCapError::new("native video writer received no frames"));
        }
        unsafe {
            self.input.markAsFinished();
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let completion = StackBlock::new(move || {
            let _ = sender.send(());
        });
        unsafe {
            self.writer.finishWritingWithCompletionHandler(&completion);
        }
        receiver.recv_timeout(FINISH_TIMEOUT).map_err(|error| {
            XCapError::new(format!(
                "timed out finalizing native VideoToolbox MP4: {error}"
            ))
        })?;
        if unsafe { self.writer.status() } != AVAssetWriterStatus::Completed {
            return Err(writer_error(
                &self.writer,
                "native VideoToolbox MP4 finalization failed",
            ));
        }
        let metadata = fs::metadata(&self.partial_path).map_err(|error| {
            XCapError::new(format!(
                "native partial MP4 is unavailable after finalization: {error}"
            ))
        })?;
        if metadata.len() == 0 {
            return Err(XCapError::new(
                "native partial MP4 is empty after finalization",
            ));
        }
        self.finished = true;
        Ok(NativeVideoWriterFinish {
            partial_path: self.partial_path.clone(),
            final_path: self.final_path.clone(),
            codec: self.codec,
            frame_count: self.appended_frames,
        })
    }

    fn start_with_codec(
        final_path: PathBuf,
        partial_path: PathBuf,
        fps: f64,
        width: u32,
        height: u32,
        codec: NativeVideoCodec,
    ) -> XCapResult<Self> {
        let path = partial_path
            .to_str()
            .ok_or_else(|| XCapError::new("native MP4 path is not valid UTF-8"))?;
        let path = NSString::from_str(path);
        let output_url = NSURL::fileURLWithPath(&path);
        let file_type = unsafe { AVFileTypeMPEG4 }
            .ok_or_else(|| XCapError::new("AVFoundation MPEG-4 file type is unavailable"))?;
        let media_type = unsafe { AVMediaTypeVideo }
            .ok_or_else(|| XCapError::new("AVFoundation video media type is unavailable"))?;
        let writer =
            unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&output_url, file_type) }
                .map_err(|error| {
                    XCapError::new(format!(
                        "failed to create native AVAssetWriter: {}",
                        error.localizedDescription()
                    ))
                })?;
        unsafe {
            writer.setShouldOptimizeForNetworkUse(true);
        }
        let settings = video_settings(codec, width, height, fps)?;
        if !unsafe { writer.canApplyOutputSettings_forMediaType(Some(&settings), media_type) } {
            return Err(XCapError::new(format!(
                "AVFoundation does not support {:?} for {}x{} MPEG-4",
                codec, width, height
            )));
        }
        let input = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                media_type,
                Some(&settings),
            )
        };
        unsafe {
            input.setExpectsMediaDataInRealTime(true);
        }
        if !unsafe { writer.canAddInput(&input) } {
            return Err(XCapError::new(format!(
                "AVAssetWriter rejected {:?} video input",
                codec
            )));
        }
        unsafe {
            writer.addInput(&input);
        }
        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &input,
                None,
            )
        };
        if !unsafe { writer.startWriting() } {
            return Err(writer_error(
                &writer,
                "failed to start native VideoToolbox writer",
            ));
        }
        unsafe {
            writer.startSessionAtSourceTime(kCMTimeZero);
        }
        Ok(Self {
            writer,
            input,
            adaptor,
            partial_path,
            final_path,
            fps,
            width,
            height,
            codec,
            appended_frames: 0,
            finished: false,
        })
    }
}

impl Drop for NativeVideoWriter {
    fn drop(&mut self) {
        if !self.finished {
            unsafe {
                self.writer.cancelWriting();
            }
        }
    }
}

fn video_settings(
    codec: NativeVideoCodec,
    width: u32,
    height: u32,
    fps: f64,
) -> XCapResult<Retained<NSDictionary<NSString, AnyObject>>> {
    let codec_key = unsafe { AVVideoCodecKey }
        .ok_or_else(|| XCapError::new("AVVideoCodecKey is unavailable"))?;
    let width_key = unsafe { AVVideoWidthKey }
        .ok_or_else(|| XCapError::new("AVVideoWidthKey is unavailable"))?;
    let height_key = unsafe { AVVideoHeightKey }
        .ok_or_else(|| XCapError::new("AVVideoHeightKey is unavailable"))?;
    let compression_key = unsafe { AVVideoCompressionPropertiesKey }
        .ok_or_else(|| XCapError::new("AVVideoCompressionPropertiesKey is unavailable"))?;
    let encoder_specification_key = unsafe { AVVideoEncoderSpecificationKey }
        .ok_or_else(|| XCapError::new("AVVideoEncoderSpecificationKey is unavailable"))?;
    let bitrate_key = unsafe { AVVideoAverageBitRateKey }
        .ok_or_else(|| XCapError::new("AVVideoAverageBitRateKey is unavailable"))?;
    let frame_rate_key = unsafe { AVVideoExpectedSourceFrameRateKey }
        .ok_or_else(|| XCapError::new("AVVideoExpectedSourceFrameRateKey is unavailable"))?;
    let keyframe_key = unsafe { AVVideoMaxKeyFrameIntervalKey }
        .ok_or_else(|| XCapError::new("AVVideoMaxKeyFrameIntervalKey is unavailable"))?;
    let reorder_key = unsafe { AVVideoAllowFrameReorderingKey }
        .ok_or_else(|| XCapError::new("AVVideoAllowFrameReorderingKey is unavailable"))?;
    let codec_value = match codec {
        NativeVideoCodec::Hevc => unsafe { AVVideoCodecTypeHEVC },
        NativeVideoCodec::H264 => unsafe { AVVideoCodecTypeH264 },
    }
    .ok_or_else(|| XCapError::new("requested AVFoundation video codec is unavailable"))?;
    let width_value = NSNumber::numberWithUnsignedInt(width);
    let height_value = NSNumber::numberWithUnsignedInt(height);
    let bitrate = (f64::from(width) * f64::from(height) * fps.max(1.0) * 0.12)
        .round()
        .clamp(800_000.0, 12_000_000.0) as u32;
    let bitrate_value = NSNumber::numberWithUnsignedInt(bitrate);
    let frame_rate_value = NSNumber::numberWithDouble(fps);
    let keyframe_value =
        NSNumber::numberWithUnsignedInt((fps * 2.0).ceil().clamp(1.0, 300.0) as u32);
    let reorder_value = NSNumber::numberWithBool(false);
    let compression_values: [&AnyObject; 4] = [
        unsafe { &*(&*bitrate_value as *const NSNumber).cast::<AnyObject>() },
        unsafe { &*(&*frame_rate_value as *const NSNumber).cast::<AnyObject>() },
        unsafe { &*(&*keyframe_value as *const NSNumber).cast::<AnyObject>() },
        unsafe { &*(&*reorder_value as *const NSNumber).cast::<AnyObject>() },
    ];
    let compression = NSDictionary::from_slices(
        &[bitrate_key, frame_rate_key, keyframe_key, reorder_key],
        &compression_values,
    );
    let require_hardware_key = NSString::from_str("RequireHardwareAcceleratedVideoEncoder");
    let require_hardware_value = NSNumber::numberWithBool(true);
    let encoder_specification_values: [&AnyObject; 1] =
        [unsafe { &*(&*require_hardware_value as *const NSNumber).cast::<AnyObject>() }];
    let encoder_specification =
        NSDictionary::from_slices(&[&*require_hardware_key], &encoder_specification_values);
    let values: [&AnyObject; 5] = [
        unsafe { &*(codec_value as *const NSString).cast::<AnyObject>() },
        unsafe { &*(&*width_value as *const NSNumber).cast::<AnyObject>() },
        unsafe { &*(&*height_value as *const NSNumber).cast::<AnyObject>() },
        unsafe {
            &*(&*compression as *const NSDictionary<NSString, AnyObject>).cast::<AnyObject>()
        },
        unsafe {
            &*(&*encoder_specification as *const NSDictionary<NSString, AnyObject>)
                .cast::<AnyObject>()
        },
    ];
    Ok(NSDictionary::from_slices(
        &[
            codec_key,
            width_key,
            height_key,
            compression_key,
            encoder_specification_key,
        ],
        &values,
    ))
}

fn partial_path_for_final(final_path: &Path) -> XCapResult<PathBuf> {
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| XCapError::new("native MP4 file name is not valid UTF-8"))?;
    let partial_name = file_name
        .strip_suffix(".mp4")
        .map(|stem| format!("{stem}.partial.mp4"))
        .unwrap_or_else(|| format!("{file_name}.partial.mp4"));
    Ok(final_path.with_file_name(partial_name))
}

fn ensure_partial_absent(path: &Path) -> XCapResult<()> {
    if path.exists() {
        return Err(XCapError::new(format!(
            "refusing to overwrite existing native partial MP4: {}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_failed_start_partial(path: &Path) -> XCapResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(XCapError::new(format!(
            "failed to remove current HEVC start attempt before H.264 fallback: {error}"
        ))),
    }
}

fn writer_error(writer: &AVAssetWriter, context: &str) -> XCapError {
    let detail = unsafe { writer.error() }
        .map(|error| {
            format!(
                "{} (domain={}, code={})",
                error.localizedDescription(),
                error.domain(),
                error.code()
            )
        })
        .unwrap_or_else(|| format!("status={:?}", unsafe { writer.status() }));
    XCapError::new(format!("{context}: {detail}"))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::process::{self, Command};
    use std::ptr::{self, NonNull};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
    };

    use crate::{Monitor, NativeFrameSurface, VideoRecorderConfig};

    use super::{
        NativeVideoCodec, NativeVideoWriter, ensure_partial_absent, partial_path_for_final,
    };

    #[test]
    fn native_partial_name_is_never_uploadable_mp4_name() {
        let final_path = Path::new("/tmp/monitor_1_123.mp4");
        let partial = partial_path_for_final(final_path).unwrap();
        assert_eq!(
            partial.file_name().and_then(|value| value.to_str()),
            Some("monitor_1_123.partial.mp4")
        );
    }

    #[test]
    fn an_existing_partial_is_never_deleted_or_overwritten() {
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let partial_path = env::temp_dir().join(format!(
            "xcap-existing-native-partial-{}-{sequence}.partial.mp4",
            process::id()
        ));
        fs::write(&partial_path, b"keep-for-recovery").unwrap();

        assert!(ensure_partial_absent(&partial_path).is_err());
        assert_eq!(fs::read(&partial_path).unwrap(), b"keep-for-recovery");
        fs::remove_file(partial_path).unwrap();
    }

    #[test]
    fn odd_dimensions_are_rejected_before_starting_hardware_encoding() {
        let error =
            NativeVideoWriter::start("/tmp/xcap-odd-dimensions.mp4", 10.0, 1728, 1117).unwrap_err();
        assert!(
            error.to_string().contains("requires even frame dimensions"),
            "{error}"
        );
    }

    #[test]
    #[ignore = "requires macOS AVFoundation and a VideoToolbox encoder"]
    fn avassetwriter_encodes_retained_core_video_surfaces() {
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let final_path = env::temp_dir().join(format!(
            "xcap-native-videotoolbox-{}-{sequence}.mp4",
            process::id()
        ));
        let mut writer = NativeVideoWriter::start(&final_path, 10.0, 640, 360).unwrap();
        for frame_index in 1..=3 {
            let surface = test_surface(640, 360);
            writer.append(&surface, frame_index).unwrap();
        }
        let finished = writer.finish().unwrap();
        assert_eq!(finished.frame_count, 3);
        assert!(fs::metadata(&finished.partial_path).unwrap().len() > 0);
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_packets",
                "-show_entries",
                "stream=codec_name,nb_read_packets",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&finished.partial_path)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "ffprobe failed: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let probe_output = String::from_utf8(probe.stdout).unwrap();
        let expected_codec = match finished.codec {
            NativeVideoCodec::Hevc => "codec_name=hevc",
            NativeVideoCodec::H264 => "codec_name=h264",
        };
        assert!(probe_output.contains(expected_codec), "{probe_output}");
        assert!(probe_output.contains("nb_read_packets=3"), "{probe_output}");

        let cv2_available = Command::new("python3")
            .args(["-c", "import cv2"])
            .status()
            .is_ok_and(|status| status.success());
        if cv2_available {
            let open_cv = Command::new("python3")
                .args([
                    "-c",
                    "import cv2,sys; c=cv2.VideoCapture(sys.argv[1]); n=int(c.get(cv2.CAP_PROP_FRAME_COUNT)); c.set(cv2.CAP_PROP_POS_FRAMES,n-1); ok,frame=c.read(); raise SystemExit(0 if n==3 and ok and frame is not None else 1)",
                ])
                .arg(&finished.partial_path)
                .status()
                .unwrap();
            assert!(
                open_cv.success(),
                "OpenCV could not decode the native last frame"
            );
        }
        fs::remove_file(&finished.partial_path).unwrap();
    }

    #[test]
    #[ignore = "requires macOS AVFoundation and a VideoToolbox encoder"]
    fn dropping_an_unfinished_writer_cancels_without_publishing_a_final_file() {
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let final_path = env::temp_dir().join(format!(
            "xcap-native-videotoolbox-drop-{}-{sequence}.mp4",
            process::id()
        ));
        let partial_path = partial_path_for_final(&final_path).unwrap();
        let mut writer = NativeVideoWriter::start(&final_path, 10.0, 640, 360).unwrap();
        let surface = test_surface(640, 360);
        writer.append(&surface, 1).unwrap();
        drop(writer);

        assert!(!final_path.exists());
        if partial_path.exists() {
            fs::remove_file(partial_path).unwrap();
        }
    }

    #[test]
    #[ignore = "requires Screen Recording permission and a VideoToolbox encoder"]
    fn screencapturekit_native_surface_encodes_at_even_dimensions() {
        let monitor = Monitor::all()
            .unwrap()
            .into_iter()
            .find(|monitor| monitor.is_primary().unwrap_or(false))
            .expect("primary monitor");
        let (recorder, receiver) = monitor
            .video_recorder_with_config(VideoRecorderConfig {
                fps: 5.0,
                preserve_native_surface: true,
                ..VideoRecorderConfig::default()
            })
            .unwrap();
        recorder.start().unwrap();

        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let final_path = env::temp_dir().join(format!(
            "xcap-native-screencapturekit-{}-{sequence}.mp4",
            process::id()
        ));
        let frame = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        recorder.stop().unwrap();
        assert_eq!(frame.width % 2, 0, "captured width={}", frame.width);
        assert_eq!(frame.height % 2, 0, "captured height={}", frame.height);
        let captured_dimensions = (frame.width, frame.height);
        let surface = frame
            .native_surface
            .as_ref()
            .expect("ScreenCaptureKit did not preserve its native surface");
        let mut writer =
            NativeVideoWriter::start(&final_path, 5.0, frame.width, frame.height).unwrap();
        for frame_index in 1..=3 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !writer.try_append(surface, frame_index).unwrap() {
                assert!(
                    Instant::now() < deadline,
                    "VideoToolbox remained backpressured"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        let finished = writer.finish().unwrap();
        assert_eq!(finished.frame_count, 3);
        assert!(fs::metadata(&finished.partial_path).unwrap().len() > 0);
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_packets",
                "-show_entries",
                "stream=codec_name,width,height,nb_read_packets",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&finished.partial_path)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "ffprobe failed: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let probe_output = String::from_utf8(probe.stdout).unwrap();
        let expected_codec = match finished.codec {
            NativeVideoCodec::Hevc => "codec_name=hevc",
            NativeVideoCodec::H264 => "codec_name=h264",
        };
        assert!(probe_output.contains(expected_codec), "{probe_output}");
        assert!(
            probe_output.contains(&format!("width={}", captured_dimensions.0)),
            "{probe_output}"
        );
        assert!(
            probe_output.contains(&format!("height={}", captured_dimensions.1)),
            "{probe_output}"
        );
        assert!(probe_output.contains("nb_read_packets=3"), "{probe_output}");
        eprintln!(
            "ScreenCaptureKit {}x{} -> {:?}, 3 packets",
            captured_dimensions.0, captured_dimensions.1, finished.codec
        );
        fs::remove_file(finished.partial_path).unwrap();
    }

    fn test_surface(width: usize, height: usize) -> NativeFrameSurface {
        let mut buffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut buffer),
            )
        };
        assert_eq!(status, kCVReturnSuccess);
        let buffer: CFRetained<CVPixelBuffer> =
            unsafe { CFRetained::from_raw(NonNull::new(buffer).unwrap()) };
        NativeFrameSurface::new(buffer).unwrap()
    }
}
