use std::{
    ffi::c_void,
    fmt, io, ptr, slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use xcb::{
    Connection, Extension,
    render::{self, Picture},
    shm::{self, Seg},
    x::{self, Drawable, ImageFormat, ImageOrder, Pixmap, Window},
};

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, FramePixelFormat, LatestFrameSender,
        VideoRecorderConfig, frame_interval, latest_frame_channel,
        set_current_thread_utility_priority,
    },
};

use super::{
    impl_monitor::ImplMonitor,
    utils::{get_monitor_info_buf, wayland_detect},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerCommand {
    Start,
    Stop,
    Shutdown,
}

struct SysvSegment {
    id: libc::c_int,
    address: usize,
    len: usize,
    marked_for_deletion: bool,
}

impl SysvSegment {
    fn new(len: usize) -> XCapResult<Self> {
        let id = unsafe {
            libc::shmget(
                libc::IPC_PRIVATE,
                len,
                libc::IPC_CREAT | libc::IPC_EXCL | 0o600,
            )
        };
        if id < 0 {
            return Err(io::Error::last_os_error().into());
        }

        let address = unsafe { libc::shmat(id, ptr::null(), 0) };
        if address == (-1_isize) as *mut c_void {
            let error = io::Error::last_os_error();
            unsafe {
                libc::shmctl(id, libc::IPC_RMID, ptr::null_mut());
            }
            return Err(error.into());
        }

        Ok(Self {
            id,
            address: address as usize,
            len,
            marked_for_deletion: false,
        })
    }

    fn mark_for_deletion(&mut self) -> XCapResult<()> {
        if unsafe { libc::shmctl(self.id, libc::IPC_RMID, ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.marked_for_deletion = true;
        Ok(())
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `address` is the successful result of shmat for `len`
        // bytes. The X server has completed GetImage before this is read.
        unsafe { slice::from_raw_parts(self.address as *const u8, self.len) }
    }
}

impl Drop for SysvSegment {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.address as *const c_void);
            if !self.marked_for_deletion {
                libc::shmctl(self.id, libc::IPC_RMID, ptr::null_mut());
            }
        }
    }
}

struct XShmCapture {
    connection: Connection,
    drawable: Drawable,
    segment_id: Seg,
    segment: SysvSegment,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    frame_len: usize,
    scaler: Option<XRenderScaler>,
}

struct XRenderScaler {
    pixmap: Pixmap,
    source_picture: Picture,
    destination_picture: Picture,
    width: u16,
    height: u16,
}

impl XRenderScaler {
    fn new(
        connection: &Connection,
        screen_index: usize,
        root: Window,
        root_visual: x::Visualid,
        depth: u8,
        source_x: i16,
        source_y: i16,
        source_width: u16,
        source_height: u16,
        output_width: u16,
        output_height: u16,
    ) -> XCapResult<Self> {
        let formats_cookie = connection.send_request(&render::QueryPictFormats {});
        let formats = connection.wait_for_reply(formats_cookie)?;
        let picture_format = formats
            .screens()
            .nth(screen_index)
            .and_then(|screen| {
                screen.depths().find_map(|depth| {
                    depth
                        .visuals()
                        .iter()
                        .find(|visual| visual.visual == root_visual)
                        .map(|visual| visual.format)
                })
            })
            .ok_or_else(|| XCapError::new("XRender root visual format not found"))?;

        let pixmap: Pixmap = connection.generate_id();
        let source_picture: Picture = connection.generate_id();
        let destination_picture: Picture = connection.generate_id();
        let create_pixmap = connection.send_request_checked(&x::CreatePixmap {
            depth,
            pid: pixmap,
            drawable: Drawable::Window(root),
            width: output_width,
            height: output_height,
        });
        check_xcb_request(connection, create_pixmap)?;

        let create_source = connection.send_request_checked(&render::CreatePicture {
            pid: source_picture,
            drawable: Drawable::Window(root),
            format: picture_format,
            value_list: &[],
        });
        if let Err(error) = check_xcb_request(connection, create_source) {
            free_xrender_resources(connection, Some(pixmap), None, None);
            return Err(error);
        }

        let create_destination = connection.send_request_checked(&render::CreatePicture {
            pid: destination_picture,
            drawable: Drawable::Pixmap(pixmap),
            format: picture_format,
            value_list: &[],
        });
        if let Err(error) = check_xcb_request(connection, create_destination) {
            free_xrender_resources(connection, Some(pixmap), Some(source_picture), None);
            return Err(error);
        }

        let transform = render::Transform {
            matrix11: fixed_16_16(f64::from(source_width) / f64::from(output_width))?,
            matrix12: 0,
            matrix13: fixed_16_16(f64::from(source_x))?,
            matrix21: 0,
            matrix22: fixed_16_16(f64::from(source_height) / f64::from(output_height))?,
            matrix23: fixed_16_16(f64::from(source_y))?,
            matrix31: 0,
            matrix32: 0,
            matrix33: 1 << 16,
        };
        let set_transform = connection.send_request_checked(&render::SetPictureTransform {
            picture: source_picture,
            transform,
        });
        if let Err(error) = check_xcb_request(connection, set_transform) {
            free_xrender_resources(
                connection,
                Some(pixmap),
                Some(source_picture),
                Some(destination_picture),
            );
            return Err(error);
        }

        let set_filter = connection.send_request_checked(&render::SetPictureFilter {
            picture: source_picture,
            filter: b"bilinear",
            values: &[],
        });
        if let Err(error) = check_xcb_request(connection, set_filter) {
            log::warn!(
                "XRender bilinear filter unavailable; capture-side scale uses server default filter: {error}"
            );
        }

        Ok(Self {
            pixmap,
            source_picture,
            destination_picture,
            width: output_width,
            height: output_height,
        })
    }

    fn composite(&self, connection: &Connection) -> xcb::VoidCookieChecked {
        connection.send_request_checked(&render::Composite {
            op: render::PictOp::Src,
            src: self.source_picture,
            mask: render::PICTURE_NONE,
            dst: self.destination_picture,
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: self.width,
            height: self.height,
        })
    }

    fn free(&self, connection: &Connection) {
        free_xrender_resources(
            connection,
            Some(self.pixmap),
            Some(self.source_picture),
            Some(self.destination_picture),
        );
    }
}

impl XShmCapture {
    fn new(monitor: &ImplMonitor, config: VideoRecorderConfig) -> XCapResult<Self> {
        if wayland_detect() {
            return Err(XCapError::new("XShm requires an X11 session"));
        }

        let monitor_info = get_monitor_info_buf(monitor.output)?;
        let x = i16::try_from(monitor_info.x())
            .map_err(|_| XCapError::new("XShm monitor x coordinate is out of range"))?;
        let y = i16::try_from(monitor_info.y())
            .map_err(|_| XCapError::new("XShm monitor y coordinate is out of range"))?;
        let width = u16::try_from(monitor_info.width())
            .map_err(|_| XCapError::new("XShm monitor width is out of range"))?;
        let height = u16::try_from(monitor_info.height())
            .map_err(|_| XCapError::new("XShm monitor height is out of range"))?;
        validate_geometry(width, height)?;
        let (requested_width, requested_height) =
            config.output_dimensions(u32::from(width), u32::from(height), true);
        let requested_width = u16::try_from(requested_width)
            .map_err(|_| XCapError::new("XShm requested width is out of range"))?;
        let requested_height = u16::try_from(requested_height)
            .map_err(|_| XCapError::new("XShm requested height is out of range"))?;

        let (connection, screen_index) =
            Connection::connect_with_extensions(None, &[], &[Extension::Shm, Extension::Render])?;
        if !connection
            .active_extensions()
            .any(|extension| extension == Extension::Shm)
        {
            return Err(XCapError::new("X11 MIT-SHM extension is unavailable"));
        }
        let setup = connection.get_setup();
        let screen = setup
            .roots()
            .nth(screen_index as usize)
            .ok_or_else(|| XCapError::new("XShm X11 screen not found"))?;
        let root = screen.root();
        let depth = screen.root_depth();
        let root_visual = screen.root_visual();
        let pixmap_format = setup
            .pixmap_formats()
            .iter()
            .find(|format| format.depth() == depth)
            .ok_or_else(|| XCapError::new("XShm pixmap format not found"))?;
        validate_pixel_format(
            depth,
            pixmap_format.bits_per_pixel(),
            pixmap_format.scanline_pad(),
            setup.image_byte_order(),
        )?;

        let scaler = if (requested_width, requested_height) != (width, height) {
            if connection
                .active_extensions()
                .any(|extension| extension == Extension::Render)
            {
                match XRenderScaler::new(
                    &connection,
                    screen_index as usize,
                    root,
                    root_visual,
                    depth,
                    x,
                    y,
                    width,
                    height,
                    requested_width,
                    requested_height,
                ) {
                    Ok(scaler) => {
                        log::info!(
                            "XShm capture-side XRender scale enabled: {}x{} -> {}x{}",
                            width,
                            height,
                            requested_width,
                            requested_height
                        );
                        Some(scaler)
                    }
                    Err(error) => {
                        log::warn!(
                            "XRender capture-side scale initialization failed; retaining persistent native XShm {}x{} instead of requested {}x{}: {error}",
                            width,
                            height,
                            requested_width,
                            requested_height
                        );
                        None
                    }
                }
            } else {
                log::warn!(
                    "XRender is unavailable; retaining persistent native XShm {}x{} instead of requested {}x{}",
                    width,
                    height,
                    requested_width,
                    requested_height
                );
                None
            }
        } else {
            None
        };
        let (drawable, capture_x, capture_y, capture_width, capture_height) =
            if let Some(scaler) = scaler.as_ref() {
                (
                    Drawable::Pixmap(scaler.pixmap),
                    0,
                    0,
                    scaler.width,
                    scaler.height,
                )
            } else {
                (Drawable::Window(root), x, y, width, height)
            };
        let stride = scanline_stride(
            usize::from(capture_width),
            usize::from(pixmap_format.bits_per_pixel()),
            usize::from(pixmap_format.scanline_pad()),
        )?;
        let frame_len = stride
            .checked_mul(usize::from(capture_height))
            .ok_or_else(|| XCapError::new("XShm frame size overflow"))?;

        let version_cookie = connection.send_request(&shm::QueryVersion {});
        let _version = connection.wait_for_reply(version_cookie)?;

        let mut segment = SysvSegment::new(frame_len)?;
        let segment_id = connection.generate_id();
        let attach_cookie = connection.send_request_checked(&shm::Attach {
            shmseg: segment_id,
            shmid: segment.id as u32,
            read_only: false,
        });
        connection
            .check_request(attach_cookie)
            .map_err(|error| XCapError::from(xcb::Error::from(error)))?;
        segment.mark_for_deletion()?;

        Ok(Self {
            connection,
            drawable,
            segment_id,
            segment,
            x: capture_x,
            y: capture_y,
            width: capture_width,
            height: capture_height,
            frame_len,
            scaler,
        })
    }

    fn capture(&self, buffer_pool: &Arc<FrameBufferPool>) -> XCapResult<Option<Frame>> {
        let composite_cookie = self
            .scaler
            .as_ref()
            .map(|scaler| scaler.composite(&self.connection));
        let cookie = self.connection.send_request(&shm::GetImage {
            drawable: self.drawable,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            plane_mask: u32::MAX,
            format: ImageFormat::ZPixmap as u8,
            shmseg: self.segment_id,
            offset: 0,
        });
        let reply = self.connection.wait_for_reply(cookie)?;
        if let Some(composite_cookie) = composite_cookie {
            check_xcb_request(&self.connection, composite_cookie)?;
        }
        let written_len = reply.size() as usize;
        if written_len < self.frame_len || written_len > self.segment.len {
            return Err(XCapError::new(format!(
                "XShm invalid image size {written_len}, expected {}",
                self.frame_len
            )));
        }

        let captured_at = SystemTime::now();
        let captured_monotonic_at = Instant::now();
        let Some(mut output) = buffer_pool.try_acquire(self.frame_len) else {
            return Ok(None);
        };
        output.copy_from_slice(&self.segment.as_slice()[..self.frame_len]);
        // The validated depth-24 ZPixmap stores BGRX on little-endian X11.
        // Mark the unused byte opaque while preserving the no-swizzle path.
        for alpha in output[3..].iter_mut().step_by(4) {
            *alpha = u8::MAX;
        }

        Ok(Some(Frame::from_pooled(
            u32::from(self.width),
            u32::from(self.height),
            usize::from(self.width) * 4,
            output,
            FramePixelFormat::Bgra8,
            captured_at,
            captured_monotonic_at,
            CaptureBackendKind::LinuxXShm,
        )))
    }
}

impl Drop for XShmCapture {
    fn drop(&mut self) {
        if let Some(scaler) = self.scaler.as_ref() {
            scaler.free(&self.connection);
        }
        let cookie = self.connection.send_request_checked(&shm::Detach {
            shmseg: self.segment_id,
        });
        let _ = self.connection.check_request(cookie);
        let _ = self.connection.flush();
    }
}

fn check_xcb_request(connection: &Connection, cookie: xcb::VoidCookieChecked) -> XCapResult<()> {
    connection
        .check_request(cookie)
        .map_err(|error| XCapError::from(xcb::Error::from(error)))
}

fn fixed_16_16(value: f64) -> XCapResult<render::Fixed> {
    let scaled = value * f64::from(1 << 16);
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(XCapError::new("XRender transform is out of range"));
    }
    Ok(scaled.round() as i32)
}

fn free_xrender_resources(
    connection: &Connection,
    pixmap: Option<Pixmap>,
    source_picture: Option<Picture>,
    destination_picture: Option<Picture>,
) {
    if let Some(destination_picture) = destination_picture {
        connection.send_request(&render::FreePicture {
            picture: destination_picture,
        });
    }
    if let Some(source_picture) = source_picture {
        connection.send_request(&render::FreePicture {
            picture: source_picture,
        });
    }
    if let Some(pixmap) = pixmap {
        connection.send_request(&x::FreePixmap { pixmap });
    }
    let _ = connection.flush();
}

struct RecorderInner {
    command_sender: Sender<WorkerCommand>,
    worker: Mutex<Option<JoinHandle<()>>>,
    buffer_pool: Arc<FrameBufferPool>,
    latest_dropped: Arc<AtomicUsize>,
}

impl Drop for RecorderInner {
    fn drop(&mut self) {
        let _ = self.command_sender.send(WorkerCommand::Shutdown);
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

#[derive(Clone)]
pub struct XShmVideoRecorder {
    inner: Arc<RecorderInner>,
}

impl fmt::Debug for XShmVideoRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XShmVideoRecorder")
            .finish_non_exhaustive()
    }
}

impl XShmVideoRecorder {
    pub fn new(
        monitor: ImplMonitor,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        let capture = XShmCapture::new(&monitor, config)?;
        let (frame_sender, frame_receiver) = latest_frame_channel();
        let latest_dropped = frame_sender.dropped_counter();
        let (command_sender, command_receiver) = channel();
        let buffer_pool = FrameBufferPool::new(2);
        let worker_buffer_pool = buffer_pool.clone();
        let interval = frame_interval(config.fps);
        let worker = thread::Builder::new()
            .name("xcap-xshm-capture".to_string())
            .spawn(move || {
                set_current_thread_utility_priority();
                run_worker(
                    capture,
                    frame_sender,
                    command_receiver,
                    worker_buffer_pool,
                    interval,
                );
            })
            .map_err(XCapError::from)?;

        Ok((
            Self {
                inner: Arc::new(RecorderInner {
                    command_sender,
                    worker: Mutex::new(Some(worker)),
                    buffer_pool,
                    latest_dropped,
                }),
            },
            frame_receiver,
        ))
    }

    pub fn start(&self) -> XCapResult<()> {
        self.inner
            .command_sender
            .send(WorkerCommand::Start)
            .map_err(XCapError::new)
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.inner
            .command_sender
            .send(WorkerCommand::Stop)
            .map_err(XCapError::new)
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.inner
            .buffer_pool
            .dropped_frames()
            .saturating_add(self.inner.latest_dropped.load(Ordering::Relaxed))
    }
}

fn run_worker(
    capture: XShmCapture,
    sender: LatestFrameSender,
    command_receiver: Receiver<WorkerCommand>,
    buffer_pool: Arc<FrameBufferPool>,
    interval: Duration,
) {
    let mut active = false;
    loop {
        if !active {
            match command_receiver.recv() {
                Ok(WorkerCommand::Start) => active = true,
                Ok(WorkerCommand::Stop) => {}
                Ok(WorkerCommand::Shutdown) | Err(_) => break,
            }
            continue;
        }

        let attempt_started = Instant::now();
        match capture.capture(&buffer_pool) {
            Ok(Some(frame)) => {
                if sender.send_latest(frame).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::error!(
                    "Persistent XShm capture failed; closing this recorder so the caller can rebuild it with backoff: {error}"
                );
                break;
            }
        }

        let wait = interval.saturating_sub(attempt_started.elapsed());
        match command_receiver.recv_timeout(wait) {
            Ok(WorkerCommand::Start) => {}
            Ok(WorkerCommand::Stop) => active = false,
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn validate_geometry(width: u16, height: u16) -> XCapResult<()> {
    if width == 0 || height == 0 {
        return Err(XCapError::new("XShm capture dimensions must be non-zero"));
    }
    Ok(())
}

fn validate_pixel_format(
    depth: u8,
    bits_per_pixel: u8,
    scanline_pad: u8,
    byte_order: ImageOrder,
) -> XCapResult<()> {
    if depth != 24
        || bits_per_pixel != 32
        || scanline_pad != 32
        || byte_order != ImageOrder::LsbFirst
    {
        return Err(XCapError::new(format!(
            "XShm direct carrier requires depth=24, bpp=32, pad=32, LSB first; got depth={depth}, bpp={bits_per_pixel}, pad={scanline_pad}, order={byte_order:?}"
        )));
    }
    Ok(())
}

fn scanline_stride(width: usize, bits_per_pixel: usize, scanline_pad: usize) -> XCapResult<usize> {
    if scanline_pad == 0 || !scanline_pad.is_power_of_two() {
        return Err(XCapError::new("XShm invalid scanline padding"));
    }
    let row_bits = width
        .checked_mul(bits_per_pixel)
        .ok_or_else(|| XCapError::new("XShm row size overflow"))?;
    let padded_bits = row_bits
        .checked_add(scanline_pad - 1)
        .ok_or_else(|| XCapError::new("XShm padded row size overflow"))?
        & !(scanline_pad - 1);
    Ok(padded_bits / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_only_direct_bgra_x11_layout() {
        assert!(validate_pixel_format(24, 32, 32, ImageOrder::LsbFirst).is_ok());
        assert!(validate_pixel_format(24, 24, 32, ImageOrder::LsbFirst).is_err());
        assert!(validate_pixel_format(24, 32, 32, ImageOrder::MsbFirst).is_err());
    }

    #[test]
    fn scanline_stride_is_checked_and_padded() {
        assert_eq!(scanline_stride(1920, 32, 32).unwrap(), 7680);
        assert_eq!(scanline_stride(3, 24, 32).unwrap(), 12);
        assert!(scanline_stride(usize::MAX, 32, 32).is_err());
        assert!(scanline_stride(10, 32, 0).is_err());
    }

    #[test]
    fn xrender_transform_values_use_signed_16_16_fixed_point() {
        assert_eq!(fixed_16_16(1.0).unwrap(), 65_536);
        assert_eq!(fixed_16_16(2.0).unwrap(), 131_072);
        assert_eq!(fixed_16_16(0.5).unwrap(), 32_768);
        assert_eq!(fixed_16_16(-1920.0).unwrap(), -125_829_120);
        assert!(fixed_16_16(f64::INFINITY).is_err());
    }
}
