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
    _padding: [f32; 1],
}

impl Uniforms {
    pub fn new(
        size: Size,
        image_dimensions: Size,
        sampling_factor: f32,
        target_size: Size,
        opacity: f32,
        scale_mode: ScaleMode,
    ) -> Self {
        let (uv_scale, uv_offset) = uv_mapping(size, image_dimensions, scale_mode);

        Self {
            sampling_factor: [sampling_factor, sampling_factor],
            size: [
                (size.width / target_size.width) * 2.0,
                (size.height / target_size.height) * 2.0,
            ],
            uv_scale: [uv_scale.0, uv_scale.1],
            uv_offset: [uv_offset.0, uv_offset.1],
            opacity,
            _padding: [0.0; 1],
        }
    }
}

fn uv_mapping(container: Size, image: Size, scale_mode: ScaleMode) -> ((f32, f32), (f32, f32)) {
    if container.width <= 0.0
        || container.height <= 0.0
        || image.width <= 0.0
        || image.height <= 0.0
    {
        return ((1.0, 1.0), (0.0, 0.0));
    }

    match scale_mode {
        ScaleMode::Stretch => ((1.0, 1.0), (0.0, 0.0)),
        ScaleMode::Cover => {
            let container_aspect = container.width / container.height;
            let image_aspect = image.width / image.height;

            if image_aspect > container_aspect {
                let visible = container_aspect / image_aspect;
                ((visible, 1.0), ((1.0 - visible) * 0.5, 0.0))
            } else {
                let visible = image_aspect / container_aspect;
                ((1.0, visible), (0.0, (1.0 - visible) * 0.5))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Uniforms, uv_mapping};
    use crate::program::ScaleMode;
    use crate::yuv::Size;
    use std::mem::size_of;

    #[test]
    fn uniforms_still_match_pipeline_binding_size() {
        assert_eq!(size_of::<Uniforms>(), 40);
    }

    #[test]
    fn cover_crops_width_for_wide_video() {
        let (scale, offset) = uv_mapping(
            Size {
                width: 100.0,
                height: 100.0,
            },
            Size {
                width: 200.0,
                height: 100.0,
            },
            ScaleMode::Cover,
        );
        assert_eq!(scale.0, 0.5);
        assert_eq!(scale.1, 1.0);
        assert_eq!(offset.0, 0.25);
        assert_eq!(offset.1, 0.0);
    }
}
