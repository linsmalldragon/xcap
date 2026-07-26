use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        mpsc::Receiver,
    },
    time::{Instant, SystemTime},
};

use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
            Gdi::HMONITOR,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    core::{AgileReference, IInspectable, Interface, factory},
};

use super::d3d11_readback::{D3d11ReadbackState, texture_to_frame as readback_texture_to_frame};

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, LatestFrameSender, VideoRecorderConfig,
        frame_interval, latest_frame_channel,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum WgcTerminalReason {
    ItemClosed = 1,
}

impl WgcTerminalReason {
    fn from_raw(value: u8) -> Option<Self> {
        match value {
            value if value == Self::ItemClosed as u8 => Some(Self::ItemClosed),
            _ => None,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::ItemClosed => "Windows Graphics Capture item closed",
        }
    }
}

struct WgcResources {
    item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived_token: i64,
    item_closed_token: i64,
}

impl Drop for WgcResources {
    fn drop(&mut self) {
        let _ = self.item.RemoveClosed(self.item_closed_token);
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

struct WgcInner {
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
    _winrt_device: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    _resources: WgcResources,
    active: Arc<AtomicBool>,
    buffer_pool: Arc<FrameBufferPool>,
    latest_dropped: Arc<AtomicUsize>,
    callback_errors: Arc<AtomicUsize>,
    terminal_reason: Arc<AtomicU8>,
}

impl fmt::Debug for WgcInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WgcInner")
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct WgcVideoRecorder {
    inner: Arc<WgcInner>,
}

impl fmt::Debug for WgcVideoRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WgcVideoRecorder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl WgcVideoRecorder {
    pub fn new(
        h_monitor: HMONITOR,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(XCapError::new("Windows Graphics Capture is not supported"));
        }

        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(h_monitor)? };
        let size = validate_size(item.Size()?)?;

        let (device, context, winrt_device) = create_direct3d_device()?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);

        let (sender, receiver) = latest_frame_channel();
        let latest_dropped = sender.dropped_counter();
        let callback_errors = Arc::new(AtomicUsize::new(0));
        let terminal_reason = Arc::new(AtomicU8::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let buffer_pool = FrameBufferPool::new(2);
        let readback_state = Arc::new(Mutex::new(D3d11ReadbackState::default()));
        let last_frame_at = Arc::new(Mutex::new(None::<Instant>));
        let current_size = Arc::new(Mutex::new(size));
        let interval = frame_interval(config.fps);

        let callback_active = active.clone();
        let callback_device = device.clone();
        let callback_context = context.clone();
        let callback_winrt_device = AgileReference::new(&winrt_device)?;
        let callback_buffer_pool = buffer_pool.clone();
        let callback_readback_state = readback_state.clone();
        let callback_last_frame_at = last_frame_at.clone();
        let callback_current_size = current_size.clone();
        let callback_sender: LatestFrameSender = sender.clone();
        let callback_errors_counter = callback_errors.clone();
        let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |frame_pool, _| {
                let Some(frame_pool) = frame_pool.as_ref() else {
                    return Ok(());
                };
                let frame = match frame_pool.TryGetNextFrame() {
                    Ok(frame) => frame,
                    Err(_) => {
                        callback_errors_counter.fetch_add(1, Ordering::Relaxed);
                        return Ok(());
                    }
                };
                if !callback_active.load(Ordering::Acquire) {
                    let _ = frame.Close();
                    return Ok(());
                }

                let now = Instant::now();
                {
                    let mut last_frame = callback_last_frame_at
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if last_frame.is_some_and(|last| now.duration_since(last) < interval) {
                        let _ = frame.Close();
                        return Ok(());
                    }
                    *last_frame = Some(now);
                }

                let captured_at = SystemTime::now();
                let captured_monotonic_at = Instant::now();
                let content_size = frame.ContentSize().ok();
                let result = frame
                    .Surface()
                    .and_then(|surface| {
                        let access = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                        unsafe { access.GetInterface::<ID3D11Texture2D>() }
                    })
                    .map_err(XCapError::from)
                    .and_then(|texture| {
                        readback_texture_to_frame(
                            &callback_device,
                            &callback_context,
                            texture,
                            &callback_readback_state,
                            Some(&callback_buffer_pool),
                            Some((captured_at, captured_monotonic_at)),
                            config,
                            CaptureBackendKind::WindowsGraphicsCapture,
                        )
                    });

                match result {
                    Ok(Some(frame)) => {
                        let _ = callback_sender.send_latest(frame);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        callback_errors_counter.fetch_add(1, Ordering::Relaxed);
                    }
                }

                if let Some(content_size) = content_size {
                    let mut previous = callback_current_size
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if *previous != content_size
                        && content_size.Width > 0
                        && content_size.Height > 0
                    {
                        if callback_winrt_device.resolve().is_ok_and(|winrt_device| {
                            frame_pool
                                .Recreate(
                                    &winrt_device,
                                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                    2,
                                    content_size,
                                )
                                .is_ok()
                        }) {
                            *previous = content_size;
                        }
                    }
                }
                let _ = frame.Close();
                Ok(())
            },
        );
        let frame_arrived_token = frame_pool.FrameArrived(&handler)?;
        let closed_active = active.clone();
        let closed_terminal_reason = terminal_reason.clone();
        let closed_handler =
            TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                closed_active.store(false, Ordering::Release);
                let _ = closed_terminal_reason.compare_exchange(
                    0,
                    WgcTerminalReason::ItemClosed as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                Ok(())
            });
        let item_closed_token = item.Closed(&closed_handler)?;
        let resources = WgcResources {
            item: item.clone(),
            frame_pool,
            session,
            frame_arrived_token,
            item_closed_token,
        };
        resources.session.StartCapture()?;

        Ok((
            Self {
                inner: Arc::new(WgcInner {
                    _device: device,
                    _context: context,
                    _winrt_device: winrt_device,
                    _item: item,
                    _resources: resources,
                    active,
                    buffer_pool,
                    latest_dropped,
                    callback_errors,
                    terminal_reason,
                }),
            },
            receiver,
        ))
    }

    pub fn start(&self) -> XCapResult<()> {
        if let Some(reason) = self.terminal_reason() {
            return Err(XCapError::new(reason.message()));
        }
        self.inner.active.store(true, Ordering::Release);
        Ok(())
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.inner.active.store(false, Ordering::Release);
        Ok(())
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        self.inner
            .buffer_pool
            .dropped_frames()
            .saturating_add(self.inner.latest_dropped.load(Ordering::Relaxed))
            .saturating_add(self.inner.callback_errors.load(Ordering::Relaxed))
    }

    pub(crate) fn terminal_error(&self) -> Option<String> {
        self.terminal_reason()
            .map(|reason| reason.message().to_string())
    }

    fn terminal_reason(&self) -> Option<WgcTerminalReason> {
        WgcTerminalReason::from_raw(self.inner.terminal_reason.load(Ordering::Acquire))
    }
}

fn validate_size(size: SizeInt32) -> XCapResult<SizeInt32> {
    if size.Width <= 0 || size.Height <= 0 {
        return Err(XCapError::new(format!(
            "WGC returned invalid size {}x{}",
            size.Width, size.Height
        )));
    }
    Ok(size)
}

fn create_direct3d_device() -> XCapResult<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.ok_or_else(|| XCapError::new("D3D11CreateDevice failed"))?;
        let context = device.GetImmediateContext()?;
        let dxgi_device = device.cast::<IDXGIDevice>()?;
        let inspectable: IInspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        let winrt_device = inspectable.cast::<IDirect3DDevice>()?;
        Ok((device, context, winrt_device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_wgc_size_is_rejected_before_session_creation() {
        assert!(
            validate_size(SizeInt32 {
                Width: 0,
                Height: 1080
            })
            .is_err()
        );
        assert!(
            validate_size(SizeInt32 {
                Width: 1920,
                Height: -1
            })
            .is_err()
        );
        assert!(
            validate_size(SizeInt32 {
                Width: 1920,
                Height: 1080
            })
            .is_ok()
        );
    }

    #[test]
    fn terminal_reason_decoder_rejects_unknown_states() {
        assert_eq!(WgcTerminalReason::from_raw(0), None);
        assert_eq!(
            WgcTerminalReason::from_raw(WgcTerminalReason::ItemClosed as u8),
            Some(WgcTerminalReason::ItemClosed)
        );
        assert_eq!(WgcTerminalReason::from_raw(u8::MAX), None);
    }
}
