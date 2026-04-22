use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};

use image::RgbaImage;
use lazy_static::lazy_static;
use pipewire::{
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
    stream::{StreamFlags, StreamRc},
};
use zbus::zvariant::Value;

use crate::{XCapError, XCapResult};

use super::{
    utils::get_zbus_connection,
    wayland_video_recorder::ScreenCast,
};

struct StreamInfo {
    source_x: i32,
    source_y: i32,
    source_w: i32,
    source_h: i32,
    latest_frame: Arc<(Mutex<Option<RgbaImage>>, Condvar)>,
}

struct ScreenCastCaptureInner {
    streams: Vec<StreamInfo>,
}

// 0 = not initialized, 1 = active, 2 = permanently failed
static SCREENCAST_STATE: AtomicU8 = AtomicU8::new(0);

lazy_static! {
    static ref SCREENCAST_INSTANCE: Mutex<Option<ScreenCastCaptureInner>> = Mutex::new(None);
}

fn init_screencast() -> XCapResult<ScreenCastCaptureInner> {
    let screen_cast = ScreenCast::new()?;
    let session = screen_cast.create_session()?;

    let conn = get_zbus_connection()?;
    let proxy = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )?;

    let handle_token = rand::random::<u32>().to_string();
    let portal_request = super::utils::get_zbus_portal_request(conn, &handle_token)?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(&handle_token));
    options.insert("types", Value::from(1_u32));
    options.insert("multiple", Value::from(true));
    options.insert("persist_mode", Value::from(2_u32));

    if let Some(token) = load_restore_token() {
        options.insert("restore_token", Value::from(token));
    }

    proxy.call_method("SelectSources", &(&session, options))?;

    // Validate SelectSources response code
    let mut response_iter = portal_request.receive_signal("Response")?;
    if let Some(msg) = response_iter.next() {
        let body = msg.body();
        let result: Result<(u32, zbus::zvariant::Value), _> = body.deserialize();
        if let Ok((code, _)) = result {
            if code == 1 {
                return Err(XCapError::new("ScreenCast: user cancelled source selection"));
            }
            if code != 0 {
                return Err(XCapError::new(format!("ScreenCast: SelectSources failed with code {code}")));
            }
        }
    }

    let response = screen_cast.start(&session)?;

    if let Some(ref token) = response.restore_token {
        save_restore_token(token);
    }

    let raw_streams = response
        .streams
        .ok_or(XCapError::new("ScreenCast: no streams in response"))?;

    if raw_streams.is_empty() {
        return Err(XCapError::new("ScreenCast: empty streams list"));
    }

    let mut streams = Vec::new();

    for (stream_id, stream_meta) in &raw_streams {
        let (src_x, src_y) = stream_meta.position.unwrap_or((0, 0));
        let (src_w, src_h) = stream_meta.size.unwrap_or((0, 0));

        let latest_frame: Arc<(Mutex<Option<RgbaImage>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let initialized = Arc::new(AtomicBool::new(false));

        let frame_ref = latest_frame.clone();
        let init_ref = initialized.clone();
        let sid = *stream_id;

        thread::spawn(move || {
            if let Err(e) = run_pipewire_capture(sid, frame_ref, init_ref) {
                log::error!("ScreenCast PipeWire thread for stream {sid} failed: {e}");
            }
        });

        streams.push(StreamInfo {
            source_x: src_x,
            source_y: src_y,
            source_w: src_w,
            source_h: src_h,
            latest_frame,
        });
    }

    Ok(ScreenCastCaptureInner { streams })
}

fn run_pipewire_capture(
    stream_id: u32,
    latest_frame: Arc<(Mutex<Option<RgbaImage>>, Condvar)>,
    initialized: Arc<AtomicBool>,
) -> XCapResult<()> {
    pipewire::init();

    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let user_data = VideoInfoRaw::default();

    let stream = StreamRc::new(
        core,
        "XCap-Screenshot",
        properties::properties! {
            *MEDIA_TYPE => "Video",
            *MEDIA_CATEGORY => "Capture",
            *MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(err) => {
                    log::error!("ScreenCast: failed to parse format: {err:?}");
                    return;
                }
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            if let Err(err) = user_data.parse(param) {
                log::error!("ScreenCast: failed to parse video format: {err:?}");
            }
        })
        .process(move |stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let size = user_data.size();
            if size.width == 0 || size.height == 0 {
                return;
            }
            if let Some(frame_data) = datas[0].data() {
                let expected_pixels = (size.width * size.height) as usize;
                let rgba_data = match user_data.format() {
                    VideoFormat::RGB => {
                        if frame_data.len() < expected_pixels * 3 {
                            return;
                        }
                        let mut buf = vec![0u8; expected_pixels * 4];
                        for (src, dst) in frame_data.chunks_exact(3).take(expected_pixels).zip(buf.chunks_exact_mut(4)) {
                            dst[0] = src[0];
                            dst[1] = src[1];
                            dst[2] = src[2];
                            dst[3] = 255;
                        }
                        buf
                    }
                    VideoFormat::RGBA | VideoFormat::RGBx => {
                        if frame_data.len() < expected_pixels * 4 {
                            return;
                        }
                        frame_data[..expected_pixels * 4].to_vec()
                    }
                    VideoFormat::BGRx => {
                        if frame_data.len() < expected_pixels * 4 {
                            return;
                        }
                        let mut buf = frame_data[..expected_pixels * 4].to_vec();
                        for chunk in buf.chunks_exact_mut(4) {
                            chunk.swap(0, 2);
                        }
                        buf
                    }
                    _ => {
                        log::error!("ScreenCast: unsupported format: {:?}", user_data.format());
                        return;
                    }
                };

                if let Some(image) = RgbaImage::from_raw(size.width, size.height, rgba_data) {
                    let (lock, cvar) = &*latest_frame;
                    if let Ok(mut guard) = lock.lock() {
                        *guard = Some(image);
                        initialized.store(true, Ordering::Release);
                        cvar.notify_all();
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
            VideoFormat::RGB,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 128,
                height: 128
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 10, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let values = PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
        .map_err(XCapError::new)?
        .0
        .into_inner();

    let mut params = [Pod::from_bytes(&values).ok_or(XCapError::new("Failed to create Pod"))?];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    main_loop.run();

    Ok(())
}

fn find_matching_stream(streams: &[StreamInfo], x: i32, y: i32, width: i32, height: i32) -> Option<usize> {
    let req_cx = x + width / 2;
    let req_cy = y + height / 2;

    for (i, s) in streams.iter().enumerate() {
        if s.source_w == 0 && s.source_h == 0 {
            return Some(i);
        }
        if req_cx >= s.source_x
            && req_cx < s.source_x + s.source_w
            && req_cy >= s.source_y
            && req_cy < s.source_y + s.source_h
        {
            return Some(i);
        }
    }
    if streams.len() == 1 {
        return Some(0);
    }
    None
}

pub fn screencast_capture(x: i32, y: i32, width: i32, height: i32) -> XCapResult<RgbaImage> {
    // Fast path: permanently failed, don't retry
    if SCREENCAST_STATE.load(Ordering::Relaxed) == 2 {
        return Err(XCapError::new("ScreenCast: previously failed, not retrying"));
    }

    // Get the matching stream's frame Arc, releasing the instance lock ASAP
    let (frame_arc, source_x, source_y) = {
        let mut instance_guard = SCREENCAST_INSTANCE.lock()?;

        if instance_guard.is_none() {
            // Re-check state under lock to avoid retrying after another thread's failure
            if SCREENCAST_STATE.load(Ordering::Relaxed) == 2 {
                return Err(XCapError::new("ScreenCast: previously failed, not retrying"));
            }
            log::info!("Initializing ScreenCast capture session (one-time permission prompt)");
            match init_screencast() {
                Ok(inner) => {
                    *instance_guard = Some(inner);
                    SCREENCAST_STATE.store(1, Ordering::Relaxed);
                }
                Err(e) => {
                    SCREENCAST_STATE.store(2, Ordering::Relaxed);
                    return Err(e);
                }
            }
        }

        let inner = instance_guard.as_ref().unwrap();

        let stream_idx = find_matching_stream(&inner.streams, x, y, width, height)
            .ok_or(XCapError::new("ScreenCast: no stream covers the requested region"))?;

        let stream = &inner.streams[stream_idx];
        (stream.latest_frame.clone(), stream.source_x, stream.source_y)
        // instance_guard drops here — other capture threads can proceed
    };

    // Wait for a frame on the matching stream (without holding instance lock)
    let full_image = {
        let (lock, cvar) = &*frame_arc;

        let guard = lock.lock()?;
        let result = cvar
            .wait_timeout_while(guard, Duration::from_secs(5), |frame| frame.is_none())
            .map_err(|e| XCapError::new(format!("ScreenCast: condvar wait failed: {e}")))?;

        if result.1.timed_out() {
            return Err(XCapError::new("ScreenCast: timed out waiting for first frame"));
        }

        // Clone the image and release the frame lock immediately so PipeWire can update
        result
            .0
            .as_ref()
            .ok_or(XCapError::new("ScreenCast: no frame available"))?
            .clone()
        // MutexGuard drops here — PipeWire process callback unblocked
    };

    // Crop using stream-relative coordinates (no locks held)
    let rel_x = x - source_x;
    let rel_y = y - source_y;

    let img_w = full_image.width() as i32;
    let img_h = full_image.height() as i32;

    let crop_x = rel_x.max(0).min(img_w) as u32;
    let crop_y = rel_y.max(0).min(img_h) as u32;
    let crop_w = width.min(img_w - crop_x as i32).max(0) as u32;
    let crop_h = height.min(img_h - crop_y as i32).max(0) as u32;

    if crop_w == 0 || crop_h == 0 {
        return Err(XCapError::new(format!(
            "ScreenCast: requested region ({x},{y} {width}x{height}) outside stream bounds (source at {source_x},{source_y}, frame {img_w}x{img_h})"
        )));
    }

    if crop_x == 0 && crop_y == 0 && crop_w == full_image.width() && crop_h == full_image.height() {
        return Ok(full_image);
    }

    let cropped = image::imageops::crop_imm(&full_image, crop_x, crop_y, crop_w, crop_h).to_image();
    Ok(cropped)
}

fn restore_token_path() -> Option<std::path::PathBuf> {
    let xdg_data = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })?;
    Some(xdg_data.join("xcap").join("screencast_restore_token"))
}

fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    std::fs::read_to_string(&path).ok().filter(|s| !s.is_empty())
}

fn save_restore_token(token: &str) {
    if let Some(path) = restore_token_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, token);
    }
}
