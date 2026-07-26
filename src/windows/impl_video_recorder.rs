use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::Receiver,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED,
                D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
                IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, LatestFrameSender, RecorderWorkerControl,
        VideoRecorderConfig, frame_interval, latest_frame_channel,
        set_current_thread_utility_priority,
    },
};

use super::d3d11_readback::{D3d11ReadbackState, texture_to_frame as readback_texture_to_frame};
#[cfg(feature = "windows-wgc")]
use super::wgc_video_recorder::WgcVideoRecorder;

pub fn texture_to_frame(
    d3d_device: &ID3D11Device,
    d3d_context: &ID3D11DeviceContext,
    source_texture: ID3D11Texture2D,
) -> XCapResult<Frame> {
    let readback_state = Mutex::new(D3d11ReadbackState::default());
    readback_texture_to_frame(
        d3d_device,
        d3d_context,
        source_texture,
        &readback_state,
        None,
        None,
        VideoRecorderConfig::default(),
        CaptureBackendKind::WindowsDxgi,
    )?
    .ok_or_else(|| XCapError::new("frame buffer unavailable"))
}

fn texture_to_frame_inner(
    d3d_device: &ID3D11Device,
    d3d_context: &ID3D11DeviceContext,
    source_texture: ID3D11Texture2D,
    readback_state: &Mutex<D3d11ReadbackState>,
    buffer_pool: Option<&Arc<FrameBufferPool>>,
    captured: Option<(SystemTime, Instant)>,
    config: VideoRecorderConfig,
) -> XCapResult<Option<Frame>> {
    readback_texture_to_frame(
        d3d_device,
        d3d_context,
        source_texture,
        readback_state,
        buffer_pool,
        captured,
        config,
        CaptureBackendKind::WindowsDxgi,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct DxgiVideoRecorder {
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    worker_control: Arc<RecorderWorkerControl>,
    frame_interval: Duration,
    buffer_pool: Arc<FrameBufferPool>,
    latest_dropped: Arc<AtomicUsize>,
    readback_state: Arc<Mutex<D3d11ReadbackState>>,
    config: VideoRecorderConfig,
}

impl DxgiVideoRecorder {
    pub fn new(
        h_monitor: HMONITOR,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        unsafe {
            let mut d3d_device = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d_device),
                None,
                None,
            )?;

            let d3d_device = d3d_device.ok_or(XCapError::new("Call D3D11CreateDevice failed"))?;
            let dxgi_device = d3d_device.cast::<IDXGIDevice>()?;
            let d3d_context = d3d_device.GetImmediateContext()?;

            let adapter = dxgi_device.GetAdapter()?;

            let mut output_index = 0;
            loop {
                let output = adapter.EnumOutputs(output_index)?;
                output_index += 1;
                let output_desc = output.GetDesc()?;

                let output1 = output.cast::<IDXGIOutput1>()?;
                let duplication = output1.DuplicateOutput(&dxgi_device)?;

                if output_desc.Monitor == h_monitor {
                    let (tx, sx) = latest_frame_channel();
                    let latest_dropped = tx.dropped_counter();
                    let buffer_pool = FrameBufferPool::new(2);
                    let worker_control = RecorderWorkerControl::new();
                    let s = Self {
                        d3d_device,
                        d3d_context,
                        duplication,
                        worker_control,
                        frame_interval: frame_interval(config.fps),
                        buffer_pool,
                        latest_dropped,
                        readback_state: Arc::new(Mutex::new(D3d11ReadbackState::default())),
                        config,
                    };
                    s.on_frame(tx)?;
                    return Ok((s, sx));
                }
            }
        }
    }

    fn on_frame(&self, tx: LatestFrameSender) -> XCapResult<()> {
        let duplication = self.duplication.clone();
        let d3d_device = self.d3d_device.clone();
        let d3d_context = self.d3d_context.clone();
        let recorder_waker = self.worker_control.waker();
        let shutdown = self.worker_control.shutdown_flag();
        let frame_interval = self.frame_interval;
        let buffer_pool = self.buffer_pool.clone();
        let readback_state = self.readback_state.clone();
        let config = self.config;

        let worker = thread::spawn(move || {
            set_current_thread_utility_priority();
            let result = (|| -> XCapResult<()> {
                let mut last_capture_attempt: Option<Instant> = None;
                loop {
                    recorder_waker.wait()?;
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(last_capture_attempt) = last_capture_attempt {
                        let elapsed = last_capture_attempt.elapsed();
                        if elapsed < frame_interval {
                            if !recorder_waker
                                .wait_timeout_while_running(frame_interval - elapsed)?
                            {
                                continue;
                            }
                            if shutdown.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                    last_capture_attempt = Some(Instant::now());

                    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                    let mut resource: Option<IDXGIResource> = None;
                    unsafe {
                        match duplication.AcquireNextFrame(200, &mut frame_info, &mut resource) {
                            Err(err) => {
                                // 尝试释放当前帧，不然不能获取到下一帧数据
                                let _ = duplication.ReleaseFrame();
                                if err.code() != DXGI_ERROR_WAIT_TIMEOUT {
                                    return Err(XCapError::new("DXGI_ERROR_UNSUPPORTED"));
                                }
                            }
                            _ => {
                                // 如何确定 AcquireNextFrame 执行成功
                                if frame_info.LastPresentTime != 0 {
                                    let resource = resource
                                        .ok_or(XCapError::new("AcquireNextFrame failed"))?;
                                    let source_texture = resource.cast::<ID3D11Texture2D>()?;
                                    let captured_at = SystemTime::now();
                                    let captured_monotonic_at = Instant::now();
                                    if let Some(frame) = texture_to_frame_inner(
                                        &d3d_device,
                                        &d3d_context,
                                        source_texture,
                                        &readback_state,
                                        Some(&buffer_pool),
                                        Some((captured_at, captured_monotonic_at)),
                                        config,
                                    )? {
                                        let _ = tx.send_latest(frame);
                                    }
                                }

                                // 最后释放帧，不然获取不到当前帧的数据
                                duplication.ReleaseFrame()?;
                            }
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                log::error!("DXGI recorder worker failed: {error:?}");
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

    fn dropped_frames(&self) -> usize {
        self.buffer_pool
            .dropped_frames()
            .saturating_add(self.latest_dropped.load(Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
pub enum ImplVideoRecorder {
    Dxgi(DxgiVideoRecorder),
    #[cfg(feature = "windows-wgc")]
    Wgc(WgcVideoRecorder),
}

impl ImplVideoRecorder {
    pub fn new(
        h_monitor: HMONITOR,
        config: VideoRecorderConfig,
    ) -> XCapResult<(Self, Receiver<Frame>)> {
        #[cfg(feature = "windows-wgc")]
        if config.prefer_windows_wgc {
            match WgcVideoRecorder::new(h_monitor, config) {
                Ok((recorder, receiver)) => {
                    log::info!("Windows capture backend: WGC");
                    return Ok((Self::Wgc(recorder), receiver));
                }
                Err(error) => {
                    log::warn!("WGC initialization failed, falling back to DXGI: {error}");
                }
            }
        }

        #[cfg(not(feature = "windows-wgc"))]
        if config.prefer_windows_wgc {
            log::warn!("WGC runtime gate requested without windows-wgc build feature; using DXGI");
        }

        let (recorder, receiver) = DxgiVideoRecorder::new(h_monitor, config)?;
        log::info!("Windows capture backend: DXGI");
        Ok((Self::Dxgi(recorder), receiver))
    }

    pub fn start(&self) -> XCapResult<()> {
        match self {
            Self::Dxgi(recorder) => recorder.start(),
            #[cfg(feature = "windows-wgc")]
            Self::Wgc(recorder) => recorder.start(),
        }
    }

    pub fn stop(&self) -> XCapResult<()> {
        match self {
            Self::Dxgi(recorder) => recorder.stop(),
            #[cfg(feature = "windows-wgc")]
            Self::Wgc(recorder) => recorder.stop(),
        }
    }

    pub(crate) fn dropped_frames(&self) -> usize {
        match self {
            Self::Dxgi(recorder) => recorder.dropped_frames(),
            #[cfg(feature = "windows-wgc")]
            Self::Wgc(recorder) => recorder.dropped_frames(),
        }
    }

    pub(crate) fn terminal_error(&self) -> Option<String> {
        match self {
            Self::Dxgi(_) => None,
            #[cfg(feature = "windows-wgc")]
            Self::Wgc(recorder) => recorder.terminal_error(),
        }
    }
}
