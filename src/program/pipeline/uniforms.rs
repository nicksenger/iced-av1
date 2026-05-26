use crate::program::ScaleMode;
use crate::yuv::Size;

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Uniforms {
    sampling_factor: [f32; 2],
    size: [f32; 2],
    uv_scale: [f32; 2],
    uv_offset: [f32; 2],
    opacity: f32,
    _padding0: f32,
    _padding1: f32,
    _padding2: f32,
}

impl Uniforms {
    pub fn new(
        size: Size,
        image_dimensions: Size,
        sampling_factor: f32,
        opacity: f32,
        scale_mode: ScaleMode,
    ) -> Self {
        let (uv_scale, uv_offset) = match scale_mode {
            ScaleMode::Stretch => ((1.0, 1.0), (0.0, 0.0)),
            ScaleMode::Cover => cover_uv(size, image_dimensions),
        };

        Self {
            sampling_factor: [sampling_factor, sampling_factor],
            size: [size.width, size.height],
            uv_scale: [uv_scale.0, uv_scale.1],
            uv_offset: [uv_offset.0, uv_offset.1],
            opacity,
            _padding0: 0.0,
            _padding1: 0.0,
            _padding2: 0.0,
        }
    }
}

fn cover_uv(container: Size, image: Size) -> ((f32, f32), (f32, f32)) {
    if container.width <= 0.0
        || container.height <= 0.0
        || image.width <= 0.0
        || image.height <= 0.0
    {
        return ((1.0, 1.0), (0.0, 0.0));
    }

    let container_aspect = container.width / container.height;
    let image_aspect = image.width / image.height;

    if image_aspect > container_aspect {
        let visible_width = container_aspect / image_aspect;
        ((visible_width, 1.0), ((1.0 - visible_width) * 0.5, 0.0))
    } else {
        let visible_height = image_aspect / container_aspect;
        ((1.0, visible_height), (0.0, (1.0 - visible_height) * 0.5))
    }
}

#[cfg(test)]
mod tests {
    use super::{Uniforms, cover_uv};
    use crate::yuv::Size;

    const EPSILON: f32 = 1e-6;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn cover_crops_width_for_wider_images() {
        let (scale, offset) = cover_uv(
            Size {
                width: 100.0,
                height: 100.0,
            },
            Size {
                width: 200.0,
                height: 100.0,
            },
        );

        assert_close(scale.0, 0.5);
        assert_close(scale.1, 1.0);
        assert_close(offset.0, 0.25);
        assert_close(offset.1, 0.0);
    }

    #[test]
    fn cover_crops_height_for_taller_images() {
        let (scale, offset) = cover_uv(
            Size {
                width: 100.0,
                height: 100.0,
            },
            Size {
                width: 100.0,
                height: 200.0,
            },
        );

        assert_close(scale.0, 1.0);
        assert_close(scale.1, 0.5);
        assert_close(offset.0, 0.0);
        assert_close(offset.1, 0.25);
    }

    #[test]
    fn uniforms_layout_matches_wgsl() {
        use std::mem::{align_of, size_of};

        assert_eq!(size_of::<Uniforms>(), 48);
        assert_eq!(align_of::<Uniforms>(), 4);
    }
}
