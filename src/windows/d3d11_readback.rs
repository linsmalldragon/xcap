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
                D3D11_VIDEO_PROCESSOR_CAPS, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_ROTATION,
                D3D11_VIDEO_PROCESSOR_ROTATION_90, D3D11_VIDEO_PROCESSOR_ROTATION_180,
                D3D11_VIDEO_PROCESSOR_ROTATION_270, D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
                ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
                ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorOutputView,
            },
            Dxgi::Common::{
                DXGI_FORMAT, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
                DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
                DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_RATIONAL,
                DXGI_SAMPLE_DESC,
            },
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

/// Rotation required to turn a DXGI desktop-duplication surface into the
/// monitor's logical, upright orientation.
///
/// Desktop Duplication always returns an unrotated surface. On a portrait
/// display the desktop pixels are rotated *inside* that landscape surface, so
/// output geometry and FitWithin sizing must be based on this logical
/// orientation rather than the texture descriptor alone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum FrameRotation {
    #[default]
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl FrameRotation {
    pub(crate) fn from_dxgi(rotation: DXGI_MODE_ROTATION) -> XCapResult<Self> {
        match rotation {
            DXGI_MODE_ROTATION_UNSPECIFIED | DXGI_MODE_ROTATION_IDENTITY => Ok(Self::Identity),
            DXGI_MODE_ROTATION_ROTATE90 => Ok(Self::Rotate90),
            DXGI_MODE_ROTATION_ROTATE180 => Ok(Self::Rotate180),
            DXGI_MODE_ROTATION_ROTATE270 => Ok(Self::Rotate270),
            _ => Err(XCapError::new(format!(
                "unsupported DXGI output rotation value {}",
                rotation.0
            ))),
        }
    }

    fn output_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Self::Identity | Self::Rotate180 => (width, height),
            Self::Rotate90 | Self::Rotate270 => (height, width),
        }
    }

    fn as_d3d11(self) -> D3D11_VIDEO_PROCESSOR_ROTATION {
        match self {
            Self::Identity => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
            Self::Rotate90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
            Self::Rotate180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
            Self::Rotate270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
        }
    }
}

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
    rotation: FrameRotation,
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
        rotation: FrameRotation,
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
            if rotation != FrameRotation::Identity {
                let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
                enumerator.GetVideoProcessorCaps(&mut caps)?;
                if caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 == 0 {
                    return Err(XCapError::new(
                        "D3D11 video processor does not support stream rotation",
                    ));
                }
            }
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
                rotation,
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
        rotation: FrameRotation,
    ) -> bool {
        self.input_width == source_desc.Width
            && self.input_height == source_desc.Height
            && self.output_width == output_width
            && self.output_height == output_height
            && self.format == source_desc.Format
            && self.rotation == rotation
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
            self.video_context.VideoProcessorSetStreamRotation(
                &self.processor,
                0,
                self.rotation != FrameRotation::Identity,
                self.rotation.as_d3d11(),
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
    gpu_transform_unavailable: bool,
}

impl fmt::Debug for D3d11ReadbackState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3d11ReadbackState")
            .field("has_staging", &self.staging.is_some())
            .field("has_scaler", &self.scaler.is_some())
            .field("gpu_transform_unavailable", &self.gpu_transform_unavailable)
            .finish()
    }
}

fn requested_output_dimensions(
    config: VideoRecorderConfig,
    source_width: u32,
    source_height: u32,
    rotation: FrameRotation,
) -> (u32, u32) {
    let (logical_width, logical_height) = rotation.output_dimensions(source_width, source_height);
    config.output_dimensions(logical_width, logical_height, true)
}

fn copy_mapped_bgra(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_stride: usize,
    rotation: FrameRotation,
    destination: &mut [u8],
) -> XCapResult<()> {
    let source_row_bytes = (source_width as usize)
        .checked_mul(4)
        .ok_or_else(|| XCapError::new("D3D11 source row size overflow"))?;
    let required_source_len = if source_height == 0 {
        0
    } else {
        source_stride
            .checked_mul(source_height as usize - 1)
            .and_then(|offset| offset.checked_add(source_row_bytes))
            .ok_or_else(|| XCapError::new("D3D11 mapped source size overflow"))?
    };
    let (output_width, output_height) = rotation.output_dimensions(source_width, source_height);
    let required_destination_len = (output_width as usize)
        .checked_mul(output_height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| XCapError::new("D3D11 rotated output size overflow"))?;
    if source_stride < source_row_bytes || source.len() < required_source_len {
        return Err(XCapError::new("D3D11 mapped source buffer is truncated"));
    }
    if destination.len() != required_destination_len {
        return Err(XCapError::new(format!(
            "D3D11 destination buffer has {} bytes, expected {required_destination_len}",
            destination.len()
        )));
    }

    if rotation == FrameRotation::Identity {
        if source_stride == source_row_bytes {
            destination.copy_from_slice(&source[..required_destination_len]);
        } else {
            for (source_row, destination_row) in source
                .chunks_exact(source_stride)
                .take(source_height as usize)
                .zip(destination.chunks_exact_mut(source_row_bytes))
            {
                destination_row.copy_from_slice(&source_row[..source_row_bytes]);
            }
        }
        return Ok(());
    }

    let output_width = output_width as usize;
    for source_y in 0..source_height as usize {
        let source_row = source_y * source_stride;
        for source_x in 0..source_width as usize {
            let (destination_x, destination_y) = match rotation {
                FrameRotation::Identity => unreachable!("identity handled above"),
                FrameRotation::Rotate90 => (source_height as usize - 1 - source_y, source_x),
                FrameRotation::Rotate180 => (
                    source_width as usize - 1 - source_x,
                    source_height as usize - 1 - source_y,
                ),
                FrameRotation::Rotate270 => (source_y, source_width as usize - 1 - source_x),
            };
            let source_index = source_row + source_x * 4;
            let destination_index = (destination_y * output_width + destination_x) * 4;
            destination[destination_index..destination_index + 4]
                .copy_from_slice(&source[source_index..source_index + 4]);
        }
    }
    Ok(())
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
    rotation: FrameRotation,
) -> XCapResult<Option<Frame>> {
    unsafe {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        source.GetDesc(&mut source_desc);
        let (logical_source_width, logical_source_height) =
            rotation.output_dimensions(source_desc.Width, source_desc.Height);
        let (output_width, output_height) =
            requested_output_dimensions(config, source_desc.Width, source_desc.Height, rotation);
        let gpu_transform_requested = rotation != FrameRotation::Identity
            || output_width != logical_source_width
            || output_height != logical_source_height;

        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (readback_source, readback_rotation) = if gpu_transform_requested
            && !state.gpu_transform_unavailable
        {
            let recreate = state.scaler.as_ref().is_none_or(|scaler| {
                !scaler.matches(source_desc, output_width, output_height, rotation)
            });
            if recreate {
                match TextureScaler::new(
                    device,
                    context,
                    source_desc,
                    output_width,
                    output_height,
                    rotation,
                ) {
                    Ok(scaler) => state.scaler = Some(scaler),
                    Err(error) => {
                        state.gpu_transform_unavailable = true;
                        state.scaler = None;
                        log::warn!(
                            "D3D11 GPU transform unavailable for {backend_kind:?}; using CPU rotation at native logical {}x{} instead of requested {}x{}: {error}",
                            logical_source_width,
                            logical_source_height,
                            output_width,
                            output_height
                        );
                    }
                }
            }
            match state.scaler.as_ref() {
                Some(scaler) => match scaler.scale(&source) {
                    Ok(texture) => (texture, FrameRotation::Identity),
                    Err(error) => {
                        state.gpu_transform_unavailable = true;
                        state.scaler = None;
                        log::warn!(
                            "D3D11 GPU transform failed for {backend_kind:?}; falling back to CPU rotation at native logical {}x{}: {error}",
                            logical_source_width,
                            logical_source_height
                        );
                        (source.clone(), rotation)
                    }
                },
                None => (source.clone(), rotation),
            }
        } else {
            (source.clone(), rotation)
        };

        let mut readback_desc = D3D11_TEXTURE2D_DESC::default();
        readback_source.GetDesc(&mut readback_desc);
        let (frame_width, frame_height) =
            readback_rotation.output_dimensions(readback_desc.Width, readback_desc.Height);
        let frame_row_bytes = (frame_width as usize)
            .checked_mul(4)
            .ok_or_else(|| XCapError::new("D3D11 frame row size overflow"))?;
        let compact_len = frame_row_bytes
            .checked_mul(frame_height as usize)
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
        let copy_result = copy_mapped_bgra(
            mapped_bytes,
            readback_desc.Width,
            readback_desc.Height,
            mapped.RowPitch as usize,
            readback_rotation,
            destination,
        );
        context.Unmap(Some(&staging_resource), 0);
        copy_result?;

        let (captured_at, captured_monotonic_at) =
            captured.unwrap_or_else(|| (SystemTime::now(), Instant::now()));
        Ok(Some(match pooled {
            Some(buffer) => Frame::from_pooled(
                frame_width,
                frame_height,
                frame_row_bytes,
                buffer,
                FramePixelFormat::Bgra8,
                captured_at,
                captured_monotonic_at,
                backend_kind,
            ),
            None => Frame::new_bgra(
                frame_width,
                frame_height,
                owned.expect("owned D3D11 output exists"),
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::video_recorder::VideoRecorderOutputSize;

    fn padded_bgra(values: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
        assert_eq!(values.len(), width as usize * height as usize);
        let mut output = vec![0xee; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let value = values[y * width as usize + x];
                let index = y * stride + x * 4;
                output[index..index + 4].copy_from_slice(&[
                    value,
                    value.wrapping_add(10),
                    value.wrapping_add(20),
                    255,
                ]);
            }
        }
        output
    }

    fn first_channels(pixels: &[u8]) -> Vec<u8> {
        pixels.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    fn rotate_values(rotation: FrameRotation) -> Vec<u8> {
        let source = padded_bgra(&[1, 2, 3, 4, 5, 6], 3, 2, 16);
        let (width, height) = rotation.output_dimensions(3, 2);
        let mut destination = vec![0; width as usize * height as usize * 4];
        copy_mapped_bgra(&source, 3, 2, 16, rotation, &mut destination).unwrap();
        first_channels(&destination)
    }

    #[test]
    fn dxgi_rotation_values_map_without_guessing() {
        assert_eq!(
            FrameRotation::from_dxgi(DXGI_MODE_ROTATION_UNSPECIFIED).unwrap(),
            FrameRotation::Identity
        );
        assert_eq!(
            FrameRotation::from_dxgi(DXGI_MODE_ROTATION_IDENTITY).unwrap(),
            FrameRotation::Identity
        );
        assert_eq!(
            FrameRotation::from_dxgi(DXGI_MODE_ROTATION_ROTATE90).unwrap(),
            FrameRotation::Rotate90
        );
        assert_eq!(
            FrameRotation::from_dxgi(DXGI_MODE_ROTATION_ROTATE180).unwrap(),
            FrameRotation::Rotate180
        );
        assert_eq!(
            FrameRotation::from_dxgi(DXGI_MODE_ROTATION_ROTATE270).unwrap(),
            FrameRotation::Rotate270
        );
        assert!(FrameRotation::from_dxgi(DXGI_MODE_ROTATION(99)).is_err());
    }

    #[test]
    fn cpu_fallback_rotates_bgra_clockwise_and_honors_row_pitch() {
        assert_eq!(
            rotate_values(FrameRotation::Identity),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            rotate_values(FrameRotation::Rotate90),
            vec![4, 1, 5, 2, 6, 3]
        );
        assert_eq!(
            rotate_values(FrameRotation::Rotate180),
            vec![6, 5, 4, 3, 2, 1]
        );
        assert_eq!(
            rotate_values(FrameRotation::Rotate270),
            vec![3, 6, 2, 5, 1, 4]
        );
    }

    #[test]
    fn portrait_fit_within_uses_post_rotation_geometry() {
        let config = VideoRecorderConfig {
            output_size: Some(VideoRecorderOutputSize::FitWithin {
                max_long_edge: 1280,
                max_short_edge: 720,
            }),
            ..VideoRecorderConfig::default()
        };
        assert_eq!(
            requested_output_dimensions(config, 1920, 1080, FrameRotation::Rotate90),
            (720, 1280)
        );
        assert_eq!(
            requested_output_dimensions(config, 1920, 1080, FrameRotation::Rotate270),
            (720, 1280)
        );
        assert_eq!(
            requested_output_dimensions(config, 1920, 1080, FrameRotation::Identity),
            (1280, 720)
        );
    }

    #[test]
    fn malformed_mapped_buffers_are_rejected() {
        let mut destination = vec![0; 3 * 2 * 4];
        assert!(
            copy_mapped_bgra(
                &[0; 23],
                3,
                2,
                12,
                FrameRotation::Identity,
                &mut destination
            )
            .is_err()
        );
        assert!(
            copy_mapped_bgra(
                &[0; 24],
                3,
                2,
                12,
                FrameRotation::Rotate90,
                &mut destination[..20]
            )
            .is_err()
        );
    }
}
