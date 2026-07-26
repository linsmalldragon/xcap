use std::{
    fmt,
    mem::ManuallyDrop,
    slice,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime},
};

use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorOutputView,
            },
            Dxgi::Common::{DXGI_FORMAT, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
        },
    },
    core::Interface,
};

use crate::{
    XCapError, XCapResult,
    video_recorder::{
        CaptureBackendKind, Frame, FrameBufferPool, FramePixelFormat, VideoRecorderConfig,
    },
};

struct StagingTexture {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    texture: ID3D11Texture2D,
}

struct TextureScaler {
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
    format: DXGI_FORMAT,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

impl TextureScaler {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source_desc: D3D11_TEXTURE2D_DESC,
        output_width: u32,
        output_height: u32,
    ) -> XCapResult<Self> {
        unsafe {
            let video_device = device.cast::<ID3D11VideoDevice>()?;
            let video_context = context.cast::<ID3D11VideoContext>()?;
            let frame_rate = DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            };
            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: frame_rate,
                InputWidth: source_desc.Width,
                InputHeight: source_desc.Height,
                OutputFrameRate: frame_rate,
                OutputWidth: output_width,
                OutputHeight: output_height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content_desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            let output_desc = D3D11_TEXTURE2D_DESC {
                Width: output_width,
                Height: output_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: source_desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut output_texture = None;
            device.CreateTexture2D(&output_desc, None, Some(&mut output_texture))?;
            let output_texture = output_texture
                .ok_or_else(|| XCapError::new("D3D11 scaler output texture missing"))?;
            let output_resource = output_texture.cast::<ID3D11Resource>()?;
            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view = None;
            video_device.CreateVideoProcessorOutputView(
                &output_resource,
                &enumerator,
                &output_view_desc,
                Some(&mut output_view),
            )?;

            Ok(Self {
                input_width: source_desc.Width,
                input_height: source_desc.Height,
                output_width,
                output_height,
                format: source_desc.Format,
                video_device,
                video_context,
                enumerator,
                processor,
                output_texture,
                output_view: output_view
                    .ok_or_else(|| XCapError::new("D3D11 scaler output view missing"))?,
            })
        }
    }

    fn matches(
        &self,
        source_desc: D3D11_TEXTURE2D_DESC,
        output_width: u32,
        output_height: u32,
    ) -> bool {
        self.input_width == source_desc.Width
            && self.input_height == source_desc.Height
            && self.output_width == output_width
            && self.output_height == output_height
            && self.format == source_desc.Format
    }

    fn scale(&self, source: &ID3D11Texture2D) -> XCapResult<ID3D11Texture2D> {
        unsafe {
            let input_resource = source.cast::<ID3D11Resource>()?;
            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view = None;
            self.video_device.CreateVideoProcessorInputView(
                &input_resource,
                &self.enumerator,
                &input_view_desc,
                Some(&mut input_view),
            )?;
            let input_view =
                input_view.ok_or_else(|| XCapError::new("D3D11 scaler input view missing"))?;
            let source_rect = RECT {
                left: 0,
                top: 0,
                right: self.input_width as i32,
                bottom: self.input_height as i32,
            };
            let output_rect = RECT {
                left: 0,
                top: 0,
                right: self.output_width as i32,
                bottom: self.output_height as i32,
            };
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&source_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&output_rect),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&output_rect),
            );
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input_view)),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            Ok(self.output_texture.clone())
        }
    }
}

#[derive(Default)]
pub(crate) struct D3d11ReadbackState {
    staging: Option<StagingTexture>,
    scaler: Option<TextureScaler>,
    scaling_unavailable: bool,
}

impl fmt::Debug for D3d11ReadbackState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3d11ReadbackState")
            .field("has_staging", &self.staging.is_some())
            .field("has_scaler", &self.scaler.is_some())
            .field("scaling_unavailable", &self.scaling_unavailable)
            .finish()
    }
}

pub(crate) fn texture_to_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: ID3D11Texture2D,
    state: &Mutex<D3d11ReadbackState>,
    buffer_pool: Option<&Arc<FrameBufferPool>>,
    captured: Option<(SystemTime, Instant)>,
    config: VideoRecorderConfig,
    backend_kind: CaptureBackendKind,
) -> XCapResult<Option<Frame>> {
    unsafe {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut source_desc);
        let (output_width, output_height) =
            config.output_dimensions(source_desc.Width, source_desc.Height, true);
        let scaling_requested =
            output_width != source_desc.Width || output_height != source_desc.Height;

        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let readback_source = if scaling_requested && !state.scaling_unavailable {
            let recreate = state
                .scaler
                .as_ref()
                .is_none_or(|scaler| !scaler.matches(source_desc, output_width, output_height));
            if recreate {
                match TextureScaler::new(device, context, source_desc, output_width, output_height)
                {
                    Ok(scaler) => state.scaler = Some(scaler),
                    Err(error) => {
                        state.scaling_unavailable = true;
                        state.scaler = None;
                        log::warn!(
                            "D3D11 GPU scaling unavailable for {backend_kind:?}; using native {}x{} frames instead of requested {}x{}: {error}",
                            source_desc.Width,
                            source_desc.Height,
                            output_width,
                            output_height
                        );
                    }
                }
            }
            match state.scaler.as_ref() {
                Some(scaler) => match scaler.scale(&source) {
                    Ok(texture) => texture,
                    Err(error) => {
                        state.scaling_unavailable = true;
                        state.scaler = None;
                        log::warn!(
                            "D3D11 GPU scaling failed for {backend_kind:?}; falling back to native {}x{} frames: {error}",
                            source_desc.Width,
                            source_desc.Height
                        );
                        source.clone()
                    }
                },
                None => source.clone(),
            }
        } else {
            source.clone()
        };

        let mut readback_desc = D3D11_TEXTURE2D_DESC::default();
        readback_source.GetDesc(&mut readback_desc);
        let row_bytes = readback_desc.Width as usize * 4;
        let compact_len = row_bytes
            .checked_mul(readback_desc.Height as usize)
            .ok_or_else(|| XCapError::new("D3D11 readback size overflow"))?;
        let mut pooled = match buffer_pool {
            Some(pool) => match pool.try_acquire(compact_len) {
                Some(buffer) => Some(buffer),
                None => return Ok(None),
            },
            None => None,
        };
        let mut owned = pooled.is_none().then(|| vec![0; compact_len]);
        let destination = pooled
            .as_mut()
            .map(|buffer| buffer.as_mut_slice())
            .or_else(|| owned.as_deref_mut())
            .expect("one D3D11 output buffer is always available");

        let recreate_staging = state.staging.as_ref().is_none_or(|staging| {
            staging.width != readback_desc.Width
                || staging.height != readback_desc.Height
                || staging.format != readback_desc.Format
        });
        if recreate_staging {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                BindFlags: 0,
                MiscFlags: 0,
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..readback_desc
            };
            let mut staging_texture = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging_texture))?;
            state.staging = Some(StagingTexture {
                width: readback_desc.Width,
                height: readback_desc.Height,
                format: readback_desc.Format,
                texture: staging_texture
                    .ok_or_else(|| XCapError::new("D3D11 staging texture missing"))?,
            });
        }
        let staging_texture = &state
            .staging
            .as_ref()
            .expect("D3D11 staging texture initialized")
            .texture;
        context.CopyResource(
            Some(&staging_texture.cast()?),
            Some(&readback_source.cast()?),
        );
        let staging_resource = staging_texture.cast::<ID3D11Resource>()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(
            Some(&staging_resource),
            0,
            D3D11_MAP_READ,
            0,
            Some(&mut mapped),
        )?;
        let mapped_bytes = slice::from_raw_parts(
            mapped.pData.cast::<u8>(),
            mapped.RowPitch as usize * readback_desc.Height as usize,
        );
        if mapped.RowPitch as usize == row_bytes {
            destination.copy_from_slice(&mapped_bytes[..compact_len]);
        } else {
            for (source_row, destination_row) in mapped_bytes
                .chunks_exact(mapped.RowPitch as usize)
                .zip(destination.chunks_exact_mut(row_bytes))
            {
                destination_row.copy_from_slice(&source_row[..row_bytes]);
            }
        }
        context.Unmap(Some(&staging_resource), 0);

        let (captured_at, captured_monotonic_at) =
            captured.unwrap_or_else(|| (SystemTime::now(), Instant::now()));
        Ok(Some(match pooled {
            Some(buffer) => Frame::from_pooled(
                readback_desc.Width,
                readback_desc.Height,
                row_bytes,
                buffer,
                FramePixelFormat::Bgra8,
                captured_at,
                captured_monotonic_at,
                backend_kind,
            ),
            None => Frame::new_bgra(
                readback_desc.Width,
                readback_desc.Height,
                owned.expect("owned D3D11 output exists"),
            ),
        }))
    }
}
