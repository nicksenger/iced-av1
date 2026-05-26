use glam::Vec2;

use crate::program::ScaleMode;
use crate::yuv::Size;

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Uniforms {
    sampling_factor: Vec2,
    size: Vec2,
    scale: Vec2,
    opacity: f32,
    cover_mode: f32,
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
        let (scale, cover_mode) = uv_scale(size, image_dimensions, target_size, scale_mode);

        Self {
            sampling_factor: (sampling_factor, sampling_factor).into(),
            size: (size.width, size.height).into(),
            scale: scale.into(),
            opacity,
            cover_mode,
        }
    }
}

fn uv_scale(
    container: Size,
    image: Size,
    target_size: Size,
    scale_mode: ScaleMode,
) -> ((f32, f32), f32) {
    if container.width <= 0.0
        || container.height <= 0.0
        || image.width <= 0.0
        || image.height <= 0.0
    {
        return ((1.0, 1.0), 0.0);
    }

    match scale_mode {
        ScaleMode::Stretch => {
            let image_dimensions: Vec2 = (image.width, image.height).into();
            (
                (
                    target_size.width / image_dimensions.x / 2.0 * image_dimensions.x,
                    target_size.height / image_dimensions.y / 2.0 * image_dimensions.y,
                ),
                0.0,
            )
        }
        ScaleMode::Cover => {
            let container_aspect = container.width / container.height;
            let image_aspect = image.width / image.height;

            if image_aspect > container_aspect {
                ((container_aspect / image_aspect, 1.0), 1.0)
            } else {
                ((1.0, image_aspect / container_aspect), 1.0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::uv_scale;
    use crate::program::ScaleMode;
    use crate::yuv::Size;

    const EPSILON: f32 = 1e-6;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn stretch_preserves_existing_scale_formula() {
        let scale = uv_scale(
            Size {
                width: 123.0,
                height: 88.0,
            },
            Size {
                width: 320.0,
                height: 240.0,
            },
            Size {
                width: 123.0,
                height: 88.0,
            },
            ScaleMode::Stretch,
        );

        assert_close(scale.0.0, 61.5);
        assert_close(scale.0.1, 44.0);
        assert_close(scale.1, 0.0);
    }

    #[test]
    fn stretch_uses_full_uv_when_viewport_matches_image() {
        let scale = uv_scale(
            Size {
                width: 123.0,
                height: 88.0,
            },
            Size {
                width: 320.0,
                height: 240.0,
            },
            Size {
                width: 320.0,
                height: 240.0,
            },
            ScaleMode::Stretch,
        );

        assert_close(scale.0.0, 160.0);
        assert_close(scale.0.1, 120.0);
        assert_close(scale.1, 0.0);
    }

    #[test]
    fn cover_crops_width_for_wider_images() {
        let scale = uv_scale(
            Size {
                width: 100.0,
                height: 100.0,
            },
            Size {
                width: 200.0,
                height: 100.0,
            },
            Size {
                width: 100.0,
                height: 100.0,
            },
            ScaleMode::Cover,
        );

        assert_close(scale.0.0, 0.5);
        assert_close(scale.0.1, 1.0);
        assert_close(scale.1, 1.0);
    }

    #[test]
    fn cover_crops_height_for_taller_images() {
        let scale = uv_scale(
            Size {
                width: 100.0,
                height: 100.0,
            },
            Size {
                width: 100.0,
                height: 200.0,
            },
            Size {
                width: 100.0,
                height: 100.0,
            },
            ScaleMode::Cover,
        );

        assert_close(scale.0.0, 1.0);
        assert_close(scale.0.1, 0.5);
        assert_close(scale.1, 1.0);
    }
}
