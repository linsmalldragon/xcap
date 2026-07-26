use std::{
    collections::HashMap,
    fmt,
    io::Cursor,
    os::fd::OwnedFd as StdOwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use pipewire::{
    channel,
    context::ContextRc,
    keys::{MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE},
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamRc, StreamState},
};
use zbus::{
    blocking::Proxy,
    zvariant::{DeserializeDict, OwnedFd as ZbusOwnedFd, OwnedObjectPath, Type, Value},
};

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, FramePixelFormat, LatestFrameSender,
        VideoRecorderConfig, frame_interval, latest_frame_channel, sanitized_fps,
    },
};

use super::{
    impl_monitor::ImplMonitor,
    utils::{get_zbus_connection, get_zbus_portal_request, wait_zbus_response},
};

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastCreateSessionResponse {
    session_handle: String,
}

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastStartStream {
    pub id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub source_type: Option<u32>,
    pub mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenCastStartResponse {
    pub streams: Option<Vec<(u32, ScreenCastStartStream)>>,
    #[allow(dead_code)]
    pub restore_token: Option<String>,
}

/// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
pub struct ScreenCast<'a> {
    proxy: Proxy<'a>,
}

#[derive(Debug)]
struct PortalSession {
    path: OwnedObjectPath,
    closed: bool,
}

impl PortalSession {
    fn new(path: OwnedObjectPath) -> Self {
        Self {
            path,
            closed: false,
        }
    }

    fn path(&self) -> &OwnedObjectPath {
        &self.path
    }

    fn close(&mut self) -> XCapResult<()> {
        self.close_with(|path| {
            let connection = get_zbus_connection()?;
            let proxy = Proxy::new(
                connection,
                "org.freedesktop.portal.Desktop",
                path.as_str(),
                "org.freedesktop.portal.Session",
            )?;
            proxy.call_method("Close", &())?;
            Ok(())
        })
    }

    fn close_with(
        &mut self,
        close: impl FnOnce(&OwnedObjectPath) -> XCapResult<()>,
    ) -> XCapResult<()> {
        if self.closed {
            return Ok(());
        }
        close(&self.path)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl ScreenCast<'_> {
    pub fn new() -> XCapResult<Self> {
        let conn = get_zbus_connection()?;
        let proxy = Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )?;

        Ok(ScreenCast { proxy })
    }

    pub fn create_session(&self) -> XCapResult<OwnedObjectPath> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", Value::from(&handle_token));

        let session_handle_token = rand::random::<u32>().to_string();
        options.insert("session_handle_token", Value::from(&session_handle_token));

        self.proxy.call_method("CreateSession", &(options))?;

        let response: ScreenCastCreateSessionResponse = wait_zbus_response(&portal_request)?;

        let unique_name = conn
            .unique_name()
            .ok_or(XCapError::new("Failed to get unique name"))?;
        let unique_identifier = unique_name.trim_start_matches(':').replace('.', "_");

        let session = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{unique_identifier}/{session_handle_token}"
        ))?;

        if session.as_str() != response.session_handle {
            return Err(XCapError::new("Session handle mismatch"));
        }

        Ok(session)
    }

    pub fn select_sources(&self, session: &OwnedObjectPath) -> XCapResult<()> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", Value::from(handle_token));
        options.insert("types", Value::from(1_u32));
        options.insert("multiple", Value::from(false));

        self.proxy
            .call_method("SelectSources", &(session, options))?;

        portal_request.receive_signal("Response")?;

        Ok(())
    }

    pub fn start(&self, session: &OwnedObjectPath) -> XCapResult<ScreenCastStartResponse> {
        let conn = get_zbus_connection()?;

        let mut options = HashMap::new();

        let handle_token = rand::random::<u32>().to_string();
        let portal_request = get_zbus_portal_request(conn, &handle_token)?;

        options.insert("handle_token", Value::from(&handle_token));

        self.proxy.call_method("Start", &(session, "", options))?;

        wait_zbus_response(&portal_request)
    }

    pub fn open_pipe_wire_remote(&self, session: &OwnedObjectPath) -> XCapResult<StdOwnedFd> {
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let fd: ZbusOwnedFd = self.proxy.call("OpenPipeWireRemote", &(session, options))?;

        Ok(fd.into())
    }
}

#[derive(Clone)]
pub struct WaylandVideoRecorder {
    #[allow(dead_code)]
    monitor: ImplMonitor,
    inner: Arc<PipeWireRecorderInner>,
    config: VideoRecorderConfig,
    buffer_pool: Arc<FrameBufferPool>,
    latest_dropped: Arc<AtomicUsize>,
    callback_dropped: Arc<AtomicUsize>,
}

impl fmt::Debug for WaylandVideoRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaylandVideoRecorder")
            .field("monitor", &self.monitor)
            .field("is_running", &self.inner.is_running.load(Ordering::Acquire))
            // Sender is not Debug
            // .field("control_tx", &self.control_tx)
            .finish()
    }
}

#[derive(Clone)]
struct ListenerUserData {
    pub format: VideoInfoRaw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PipeWireCommand {
    SetActive(bool),
    Shutdown,
}

const PIPEWIRE_TERMINAL_NONE: usize = 0;
const PIPEWIRE_TERMINAL_ERROR: usize = 1;
const PIPEWIRE_TERMINAL_DISCONNECTED: usize = 2;
const PIPEWIRE_TERMINAL_CONTROL: usize = 3;

struct PipeWireRecorderInner {
    is_running: Arc<AtomicBool>,
    command_sender: channel::Sender<PipeWireCommand>,
    worker: Mutex<Option<JoinHandle<XCapResult<()>>>>,
}

impl Drop for PipeWireRecorderInner {
    fn drop(&mut self) {
        self.is_running.store(false, Ordering::Release);
        let _ = self.command_sender.send(PipeWireCommand::Shutdown);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    log::error!("PipeWire recorder worker failed: {error:?}");
                }
                Err(_) => {
                    log::error!("PipeWire recorder worker panicked");
                }
            }
        }
    }
}

impl WaylandVideoRecorder {
    pub fn new(
        monitor: ImplMonitor,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        let (sender, receiver) = latest_frame_channel();
        let latest_dropped = sender.dropped_counter();
        let callback_dropped = Arc::new(AtomicUsize::new(0));
        let (command_sender, command_receiver) = channel::channel();
        let is_running = Arc::new(AtomicBool::new(false));

        let screen_cast = ScreenCast::new()?;
        let session = PortalSession::new(screen_cast.create_session()?);
        screen_cast.select_sources(session.path())?;
        let response = screen_cast.start(session.path())?;
        let pipewire_remote = screen_cast.open_pipe_wire_remote(session.path())?;

        // 获取流节点ID
        let stream = response
            .streams
            .ok_or(XCapError::new("Stream ID not found"))?
            .into_iter()
            .next()
            .ok_or(XCapError::new("Stream ID not found"))?;
        let stream_id = stream.0;
        let source_size = stream
            .1
            .size
            .and_then(|(width, height)| {
                (width > 0 && height > 0).then_some((width as u32, height as u32))
            })
            .or_else(|| monitor.width().ok().zip(monitor.height().ok()))
            .ok_or_else(|| XCapError::new("PipeWire source size is unavailable"))?;
        let requested_size = config.output_dimensions(source_size.0, source_size.1, true);
        if requested_size != source_size {
            log::info!(
                "PipeWire requesting capture-side scale {}x{} -> {}x{}",
                source_size.0,
                source_size.1,
                requested_size.0,
                requested_size.1
            );
        }

        let recorder = Self {
            monitor,
            inner: Arc::new(PipeWireRecorderInner {
                is_running,
                command_sender,
                worker: Mutex::new(None),
            }),
            config,
            buffer_pool: FrameBufferPool::new(2),
            latest_dropped,
            callback_dropped,
        };

        recorder.pipewire_capturer(
            stream_id,
            command_receiver,
            sender,
            source_size,
            requested_size,
            pipewire_remote,
            session,
        )?;

        Ok((recorder, receiver))
    }

    pub fn pipewire_capturer(
        &self,
        stream_id: u32,
        command_receiver: channel::Receiver<PipeWireCommand>,
        sender: LatestFrameSender,
        source_size: (u32, u32),
        requested_size: (u32, u32),
        pipewire_remote: StdOwnedFd,
        portal_session: PortalSession,
    ) -> XCapResult<()> {
        let is_running = self.inner.is_running.clone();
        let config = self.config;
        let buffer_pool = self.buffer_pool.clone();
        let callback_dropped = self.callback_dropped.clone();
        let frame_interval = frame_interval(config.fps);
        let (readiness_sender, readiness_receiver) = mpsc::sync_channel(1);

        let worker = thread::spawn(move || {
            let mut portal_session = portal_session;
            let capture_result = (|| -> XCapResult<()> {
                pipewire::init();

                let main_loop = MainLoopRc::new(None)?;
                let context = ContextRc::new(&main_loop, None)?;
                let core = context.connect_fd_rc(pipewire_remote, None)?;

                let user_data = ListenerUserData {
                    format: Default::default(),
                };

                let stream = StreamRc::new(
                    core,
                    "XCap",
                    properties::properties! {
                        *MEDIA_TYPE => "Video",
                        *MEDIA_CATEGORY => "Capture",
                        *MEDIA_ROLE => "Screen",
                    },
                )?;

                let process_running = is_running.clone();
                let state_running = is_running.clone();
                let state_loop = main_loop.clone();
                let process_dropped = callback_dropped.clone();
                let callback_errors = Arc::new(AtomicUsize::new(0));
                let terminal_reason = Arc::new(AtomicUsize::new(PIPEWIRE_TERMINAL_NONE));
                let negotiated_width = Arc::new(AtomicUsize::new(0));
                let negotiated_height = Arc::new(AtomicUsize::new(0));
                let state_terminal_reason = terminal_reason.clone();
                let param_callback_errors = callback_errors.clone();
                let param_negotiated_width = negotiated_width.clone();
                let param_negotiated_height = negotiated_height.clone();
                let mut last_frame_at: Option<Instant> = None;
                let _listener = stream
                    .add_local_listener_with_user_data(user_data)
                    .state_changed(move |_, _, old, new| {
                        let terminal_reason = match new {
                            StreamState::Error(_) => PIPEWIRE_TERMINAL_ERROR,
                            StreamState::Unconnected
                                if !matches!(old, StreamState::Unconnected) =>
                            {
                                PIPEWIRE_TERMINAL_DISCONNECTED
                            }
                            _ => PIPEWIRE_TERMINAL_NONE,
                        };
                        if terminal_reason == PIPEWIRE_TERMINAL_NONE {
                            return;
                        }

                        let _ = state_terminal_reason.compare_exchange(
                            PIPEWIRE_TERMINAL_NONE,
                            terminal_reason,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        state_running.store(false, Ordering::Release);
                        state_loop.quit();
                    })
                    .param_changed(move |_, user_data, id, param| {
                        let Some(param) = param else {
                            return;
                        };

                        if id != ParamType::Format.as_raw() {
                            return;
                        }

                        let (media_type, media_subtype) = match format_utils::parse_format(param) {
                            Ok(v) => v,
                            Err(_) => {
                                param_callback_errors.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                        };

                        if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                            return;
                        }

                        if user_data.format.parse(param).is_err() {
                            param_callback_errors.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        let negotiated = user_data.format.size();
                        if (negotiated.width, negotiated.height) != requested_size {
                            param_negotiated_width
                                .store(negotiated.width as usize, Ordering::Relaxed);
                            param_negotiated_height
                                .store(negotiated.height as usize, Ordering::Relaxed);
                        }
                    })
                    .process(move |stream, user_data| {
                        let state = process_running.load(Ordering::Relaxed);
                        if !state {
                            // Always dequeue so PipeWire can recycle the buffer,
                            // but an intentionally paused stream is not a drop.
                            let _ = stream.dequeue_buffer();
                            return;
                        }
                        match stream.dequeue_buffer() {
                            None => {
                                process_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(mut buffer) => {
                                let now = Instant::now();
                                if last_frame_at
                                    .is_some_and(|last| now.duration_since(last) < frame_interval)
                                {
                                    return;
                                }
                                last_frame_at = Some(now);
                                let captured_at = SystemTime::now();
                                let captured_monotonic_at = Instant::now();
                                let datas = buffer.datas_mut();
                                if datas.is_empty() {
                                    process_dropped.fetch_add(1, Ordering::Relaxed);
                                    return;
                                }
                                let size = user_data.format.size();
                                let chunk_offset = datas[0].chunk().offset() as usize;
                                let chunk_size = datas[0].chunk().size() as usize;
                                let chunk_stride = datas[0].chunk().stride();
                                let Some(mapped_data) = datas[0].data() else {
                                    process_dropped.fetch_add(1, Ordering::Relaxed);
                                    return;
                                };
                                let Some(output_len) = (size.width as usize)
                                    .checked_mul(size.height as usize)
                                    .and_then(|pixels| pixels.checked_mul(4))
                                else {
                                    process_dropped.fetch_add(1, Ordering::Relaxed);
                                    return;
                                };
                                let Some(mut output) = buffer_pool.try_acquire(output_len) else {
                                    return;
                                };
                                let Some(pixel_format) = copy_pipewire_frame(
                                    user_data.format.format(),
                                    size.width,
                                    size.height,
                                    mapped_data,
                                    chunk_offset,
                                    chunk_size,
                                    chunk_stride,
                                    &mut output,
                                ) else {
                                    process_dropped.fetch_add(1, Ordering::Relaxed);
                                    return;
                                };

                                let frame = Frame::from_pooled(
                                    size.width,
                                    size.height,
                                    size.width as usize * 4,
                                    output,
                                    pixel_format,
                                    captured_at,
                                    captured_monotonic_at,
                                    CaptureBackendKind::LinuxPipeWire,
                                );
                                if sender.send_latest(frame).is_err() {
                                    process_running.store(false, Ordering::Release);
                                }
                            }
                        }
                    })
                    .register()?;

                let obj = pod::object!(
                    SpaTypes::ObjectParamFormat,
                    ParamType::EnumFormat,
                    pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
                    pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
                    pod::property!(
                        FormatProperties::VideoFormat,
                        Choice,
                        Enum,
                        Id,
                        // PipeWire screen-cast portals commonly expose BGRx. Keep
                        // it as BGRA all the way to the encoder instead of
                        // swapping every pixel on the capture callback.
                        VideoFormat::BGRx,
                        VideoFormat::RGB,
                        VideoFormat::RGBA,
                        VideoFormat::RGBx,
                        // VideoFormat::YUY2,
                        // VideoFormat::I420,
                    ),
                    pod::property!(
                        FormatProperties::VideoSize,
                        Choice,
                        Range,
                        Rectangle,
                        Rectangle {
                            width: requested_size.0,
                            height: requested_size.1
                        },
                        Rectangle {
                            width: 1,
                            height: 1
                        },
                        Rectangle {
                            width: source_size.0.max(requested_size.0).max(4096),
                            height: source_size.1.max(requested_size.1).max(4096)
                        }
                    ),
                    pod::property!(
                        FormatProperties::VideoFramerate,
                        Choice,
                        Range,
                        Fraction,
                        Fraction {
                            num: (sanitized_fps(config.fps) * 1000.0).round() as u32,
                            denom: 1000
                        },
                        Fraction { num: 0, denom: 1 },
                        Fraction { num: 30, denom: 1 }
                    ),
                );
                let values =
                    PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
                        .map_err(XCapError::new)?
                        .0
                        .into_inner();

                let mut params =
                    [Pod::from_bytes(&values).ok_or(XCapError::new("Failed to create Pod"))?];

                stream.connect(
                    Direction::Input,
                    Some(stream_id),
                    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                    &mut params,
                )?;

                // Used to pause/resume or tear down the persistent stream.
                let control_loop = main_loop.clone();
                let control_running = is_running.clone();
                let control_callback_errors = callback_errors.clone();
                let control_terminal_reason = terminal_reason.clone();
                let _attached = command_receiver.attach(main_loop.loop_(), {
                    move |command| match command {
                        PipeWireCommand::SetActive(active) => {
                            if stream.set_active(active).is_err() {
                                control_callback_errors.fetch_add(1, Ordering::Relaxed);
                                if active {
                                    let _ = control_terminal_reason.compare_exchange(
                                        PIPEWIRE_TERMINAL_NONE,
                                        PIPEWIRE_TERMINAL_CONTROL,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    );
                                    control_running.store(false, Ordering::Release);
                                    control_loop.quit();
                                    return;
                                }
                            }
                            if !active && stream.flush(true).is_err() {
                                control_callback_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        PipeWireCommand::Shutdown => {
                            control_running.store(false, Ordering::Release);
                            if stream.set_active(false).is_err() {
                                control_callback_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            if stream.flush(true).is_err() {
                                control_callback_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            control_loop.quit();
                        }
                    }
                });

                let _ = readiness_sender.send(());
                main_loop.run();

                let callback_error_count = callback_errors.load(Ordering::Relaxed);
                if callback_error_count > 0 {
                    log::warn!(
                        "PipeWire capture callbacks reported {callback_error_count} errors before exit"
                    );
                }
                let negotiated_width = negotiated_width.load(Ordering::Relaxed);
                let negotiated_height = negotiated_height.load(Ordering::Relaxed);
                if negotiated_width > 0 && negotiated_height > 0 {
                    log::warn!(
                        "PipeWire compositor did not honor requested {}x{} capture size; negotiated {}x{}",
                        requested_size.0,
                        requested_size.1,
                        negotiated_width,
                        negotiated_height
                    );
                }
                match terminal_reason.load(Ordering::Acquire) {
                    PIPEWIRE_TERMINAL_ERROR => Err(XCapError::new(
                        "PipeWire capture stream entered an error state",
                    )),
                    PIPEWIRE_TERMINAL_DISCONNECTED => {
                        Err(XCapError::new("PipeWire capture stream disconnected"))
                    }
                    PIPEWIRE_TERMINAL_CONTROL => {
                        Err(XCapError::new("PipeWire capture control failed"))
                    }
                    _ => Ok(()),
                }
            })();
            // Every PipeWire object above is dropped at the end of the nested
            // scope before the portal session is explicitly closed.
            let close_result = portal_session.close();
            match (capture_result, close_result) {
                (Err(capture_error), Err(close_error)) => Err(XCapError::new(format!(
                    "{capture_error}; portal session close also failed: {close_error}"
                ))),
                (Err(capture_error), Ok(())) => Err(capture_error),
                (Ok(()), close_result) => close_result,
            }
        });
        let mut worker_slot = self.inner.worker.lock().map_err(XCapError::from)?;
        if worker_slot.is_some() {
            return Err(XCapError::new(
                "PipeWire recorder worker is already attached",
            ));
        }
        *worker_slot = Some(worker);
        drop(worker_slot);

        match readiness_receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(XCapError::new(
                    "PipeWire recorder worker readiness timed out",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(XCapError::new(
                    "PipeWire recorder worker exited before becoming ready",
                ));
            }
        }

        Ok(())
    }

    pub fn start(&self) -> XCapResult<()> {
        self.inner.is_running.store(true, Ordering::Release);
        if self
            .inner
            .command_sender
            .send(PipeWireCommand::SetActive(true))
            .is_err()
        {
            self.inner.is_running.store(false, Ordering::Release);
            return Err(XCapError::new("PipeWire recorder command channel closed"));
        }
        Ok(())
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.inner.is_running.store(false, Ordering::Release);
        self.inner
            .command_sender
            .send(PipeWireCommand::SetActive(false))
            .map_err(|_| XCapError::new("PipeWire recorder command channel closed"))?;
        Ok(())
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.buffer_pool
            .dropped_frames()
            .saturating_add(self.latest_dropped.load(Ordering::Relaxed))
            .saturating_add(self.callback_dropped.load(Ordering::Relaxed))
    }
}

fn copy_pipewire_frame(
    format: VideoFormat,
    width: u32,
    height: u32,
    mapped_data: &[u8],
    chunk_offset: usize,
    chunk_size: usize,
    chunk_stride: i32,
    output: &mut [u8],
) -> Option<FramePixelFormat> {
    let (source_pixel_bytes, pixel_format) = match format {
        VideoFormat::RGB => (3_usize, FramePixelFormat::Rgba8),
        VideoFormat::RGBA | VideoFormat::RGBx => (4, FramePixelFormat::Rgba8),
        VideoFormat::BGRx => (4, FramePixelFormat::Bgra8),
        _ => return None,
    };
    let width = width as usize;
    let height = height as usize;
    let source_row_bytes = width.checked_mul(source_pixel_bytes)?;
    let output_row_bytes = width.checked_mul(4)?;
    let required_output = output_row_bytes.checked_mul(height)?;
    if width == 0 || height == 0 || output.len() < required_output {
        return None;
    }

    let source_stride = if chunk_stride == 0 {
        source_row_bytes
    } else {
        usize::try_from(chunk_stride.unsigned_abs()).ok()?
    };
    if source_stride < source_row_bytes {
        return None;
    }
    let chunk_end = if chunk_size == 0 {
        mapped_data.len()
    } else {
        chunk_offset.checked_add(chunk_size)?.min(mapped_data.len())
    };

    for row in 0..height {
        let source_start = if chunk_stride < 0 {
            chunk_offset.checked_sub(row.checked_mul(source_stride)?)?
        } else {
            chunk_offset.checked_add(row.checked_mul(source_stride)?)?
        };
        let source_end = source_start.checked_add(source_row_bytes)?;
        if source_end > mapped_data.len() || (chunk_stride >= 0 && source_end > chunk_end) {
            return None;
        }
        let destination_start = row.checked_mul(output_row_bytes)?;
        let destination_end = destination_start.checked_add(output_row_bytes)?;
        let source_row = &mapped_data[source_start..source_end];
        let destination_row = &mut output[destination_start..destination_end];

        if source_pixel_bytes == 4 {
            destination_row.copy_from_slice(source_row);
        } else {
            for (source, destination) in source_row
                .chunks_exact(3)
                .zip(destination_row.chunks_exact_mut(4))
            {
                destination[0] = source[0];
                destination[1] = source[1];
                destination[2] = source[2];
                destination[3] = u8::MAX;
            }
        }
    }

    Some(pixel_format)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn portal_session_close_is_once_after_success_and_retriable_after_failure() {
        let path =
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/xcap/test").unwrap();
        let mut session = PortalSession::new(path);
        let attempts = Cell::new(0_u32);

        assert!(
            session
                .close_with(|_| {
                    attempts.set(attempts.get() + 1);
                    Err(XCapError::new("injected close failure"))
                })
                .is_err()
        );
        assert_eq!(attempts.get(), 1);

        session
            .close_with(|_| {
                attempts.set(attempts.get() + 1);
                Ok(())
            })
            .unwrap();
        session
            .close_with(|_| {
                attempts.set(attempts.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn copies_bgrx_without_channel_swap_and_respects_stride() {
        let mapped = [
            1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, 7, 8, 9, 0, 10, 11, 12, 0, 99, 99, 99, 99,
        ];
        let mut output = [0_u8; 16];
        let format = copy_pipewire_frame(
            VideoFormat::BGRx,
            2,
            2,
            &mapped,
            0,
            mapped.len(),
            12,
            &mut output,
        );

        assert_eq!(format, Some(FramePixelFormat::Bgra8));
        assert_eq!(output, [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0]);
    }

    #[test]
    fn expands_rgb_and_rejects_truncated_frames() {
        let mut output = [0_u8; 8];
        assert_eq!(
            copy_pipewire_frame(
                VideoFormat::RGB,
                2,
                1,
                &[1, 2, 3, 4, 5, 6],
                0,
                6,
                6,
                &mut output,
            ),
            Some(FramePixelFormat::Rgba8)
        );
        assert_eq!(output, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(
            copy_pipewire_frame(VideoFormat::RGBA, 2, 1, &[1, 2, 3, 4], 0, 4, 8, &mut output,)
                .is_none()
        );
    }
}
