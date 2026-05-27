use glam::Vec2;

use crate::yuv::Size;

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Uniforms {
    sampling_factor: Vec2,
    size: Vec2,
    scale: Vec2,
    opacity: f32,
    _padding: f32,
}

impl Uniforms {
    pub fn new(
        size: Size,
        image_dimensions: Size,
        sampling_factor: f32,
        target_size: Size,
        opacity: f32,
    ) -> Self {
        let image_dimensions: Vec2 = (image_dimensions.width, image_dimensions.height).into();
        let scale = (
            target_size.width / image_dimensions.x / 2.0 * image_dimensions.x,
            target_size.height / image_dimensions.y / 2.0 * image_dimensions.y,
        )
            .into();

        Self {
            sampling_factor: (sampling_factor, sampling_factor).into(),
            size: (size.width, size.height).into(),
            scale,
            opacity,
            _padding: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Uniforms;
    use crate::yuv::Size;
    use std::mem::size_of;

    #[test]
    fn uniforms_still_match_pipeline_binding_size() {
        assert_eq!(size_of::<Uniforms>(), 32);
    }

    #[test]
    fn scale_depends_on_drawn_size() {
        let target_size = Size {
            width: 1000.0,
            height: 500.0,
        };

        let large = Uniforms::new(
            Size {
                width: 500.0,
                height: 250.0,
            },
            Size {
                width: 1920.0,
                height: 1080.0,
            },
            1.0,
            target_size,
            1.0,
        );

        let small = Uniforms::new(
            Size {
                width: 250.0,
                height: 125.0,
            },
            Size {
                width: 1920.0,
                height: 1080.0,
            },
            1.0,
            target_size,
            1.0,
        );

        assert!(small.scale.x > large.scale.x);
        assert!(small.scale.y > large.scale.y);
    }
}
