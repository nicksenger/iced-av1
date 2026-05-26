mod pipeline;

use std::cell::RefCell;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::mouse;
use iced::widget::shader;
use iced::{Rectangle, wgpu, window};

use pipeline::Pipeline;

use self::pipeline::Uniforms;
use crate::yuv::Renderable;
use crate::yuv::{Size, Yuv};

const DEFAULT_OPACITY: f32 = 0.5;
const DEFAULT_OPACITY_TWEEN_DURATION: Duration = Duration::from_millis(180);
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(16);
const OPACITY_EPSILON: f32 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleMode {
    #[default]
    Stretch,
    Cover,
}

#[derive(Debug, Clone, Copy)]
pub struct OpacityTween {
    pub duration: Duration,
}

impl Default for OpacityTween {
    fn default() -> Self {
        Self {
            duration: DEFAULT_OPACITY_TWEEN_DURATION,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OpacityOptions {
    pub opacity: f32,
    pub tween: OpacityTween,
}

impl Default for OpacityOptions {
    fn default() -> Self {
        Self {
            opacity: DEFAULT_OPACITY,
            tween: OpacityTween::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpacityController {
    shared: Arc<Mutex<OpacityState>>,
}

impl OpacityController {
    pub fn new(options: OpacityOptions) -> Self {
        Self {
            shared: Arc::new(Mutex::new(OpacityState::new(options))),
        }
    }

    pub fn opacity(&self) -> f32 {
        let Ok(mut state) = self.shared.lock() else {
            return DEFAULT_OPACITY;
        };

        state.opacity_at(Instant::now())
    }

    pub fn set_opacity(&self, opacity: f32) {
        if let Ok(mut state) = self.shared.lock() {
            state.set_immediate(opacity);
        }
    }

    pub fn tween_to_opacity(&self, opacity: f32) {
        if let Ok(mut state) = self.shared.lock() {
            let tween = state.default_tween;
            state.tween_to(opacity, tween, Instant::now());
        }
    }

    pub fn tween_to_opacity_with(&self, opacity: f32, tween: OpacityTween) {
        if let Ok(mut state) = self.shared.lock() {
            state.tween_to(opacity, tween, Instant::now());
        }
    }

    fn tick(&self, now: Instant) -> OpacityTick {
        let Ok(mut state) = self.shared.lock() else {
            return OpacityTick {
                opacity: DEFAULT_OPACITY,
                animating: false,
            };
        };

        let opacity = state.opacity_at(now);

        OpacityTick {
            opacity,
            animating: state.is_animating(),
        }
    }
}

#[derive(Debug)]
struct OpacityTick {
    opacity: f32,
    animating: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpacityAnimation {
    start_opacity: f32,
    target_opacity: f32,
    start_at: Instant,
    duration: Duration,
}

#[derive(Debug)]
struct OpacityState {
    current: f32,
    target: f32,
    animation: Option<OpacityAnimation>,
    default_tween: OpacityTween,
}

impl OpacityState {
    fn new(options: OpacityOptions) -> Self {
        let opacity = clamp_opacity(options.opacity);

        Self {
            current: opacity,
            target: opacity,
            animation: None,
            default_tween: options.tween,
        }
    }

    fn set_immediate(&mut self, opacity: f32) {
        let opacity = clamp_opacity(opacity);
        self.current = opacity;
        self.target = opacity;
        self.animation = None;
    }

    fn tween_to(&mut self, opacity: f32, tween: OpacityTween, now: Instant) {
        let opacity = clamp_opacity(opacity);
        let current = self.opacity_at(now);

        if (current - opacity).abs() <= OPACITY_EPSILON {
            self.current = opacity;
            self.target = opacity;
            self.animation = None;
            return;
        }

        if tween.duration.is_zero() {
            self.set_immediate(opacity);
            return;
        }

        self.current = current;
        self.target = opacity;
        self.animation = Some(OpacityAnimation {
            start_opacity: current,
            target_opacity: opacity,
            start_at: now,
            duration: tween.duration,
        });
    }

    fn opacity_at(&mut self, now: Instant) -> f32 {
        let Some(animation) = self.animation else {
            return self.current;
        };

        let elapsed = now.saturating_duration_since(animation.start_at);
        if elapsed >= animation.duration {
            self.current = animation.target_opacity;
            self.target = animation.target_opacity;
            self.animation = None;
            return self.current;
        }

        let progress = elapsed.as_secs_f32() / animation.duration.as_secs_f32();
        let blended = animation.start_opacity
            + (animation.target_opacity - animation.start_opacity) * ease_out_cubic(progress);

        self.current = clamp_opacity(blended);
        self.current
    }

    fn is_animating(&self) -> bool {
        self.animation.is_some()
    }
}

fn clamp_opacity(opacity: f32) -> f32 {
    opacity.clamp(0.0, 1.0)
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[derive(Clone)]
pub struct Program {
    dimensions: Size<u32>,
    sampling_factor: f32,
    scale_mode: ScaleMode,
    data: RefCell<Option<Renderable>>,
    opacity: OpacityController,
}

impl Program {
    pub fn new(yuv: Yuv) -> Self {
        Self::new_with_opacity_and_scale_mode(
            yuv,
            OpacityController::new(OpacityOptions::default()),
            ScaleMode::default(),
        )
    }

    pub fn new_with_opacity(yuv: Yuv, opacity: OpacityController) -> Self {
        Self::new_with_opacity_and_scale_mode(yuv, opacity, ScaleMode::default())
    }

    pub fn new_with_opacity_and_scale_mode(
        yuv: Yuv,
        opacity: OpacityController,
        scale_mode: ScaleMode,
    ) -> Self {
        let renderable = Renderable::from(yuv);

        Self {
            dimensions: renderable.dimensions(),
            sampling_factor: renderable.downsampling_factor(),
            scale_mode,
            data: RefCell::new(Some(renderable)),
            opacity,
        }
    }

    pub fn update_frame(&mut self, yuv: Yuv) {
        *self.data.borrow_mut() = Some(yuv.into());
    }

    pub fn set_opacity(&self, opacity: f32) {
        self.opacity.set_opacity(opacity);
    }

    pub fn tween_to_opacity(&self, opacity: f32) {
        self.opacity.tween_to_opacity(opacity);
    }

    pub fn tween_to_opacity_with(&self, opacity: f32, tween: OpacityTween) {
        self.opacity.tween_to_opacity_with(opacity, tween);
    }

    pub fn opacity_controller(&self) -> OpacityController {
        self.opacity.clone()
    }

    pub fn scale_mode(&self) -> ScaleMode {
        self.scale_mode
    }

    pub fn set_scale_mode(&mut self, scale_mode: ScaleMode) {
        self.scale_mode = scale_mode;
    }
}

impl<Message> shader::Program<Message> for Program {
    type State = ();
    type Primitive = Primitive;

    fn update(
        &self,
        _state: &mut Self::State,
        event: &iced::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<shader::Action<Message>> {
        let now = match event {
            iced::Event::Window(window::Event::RedrawRequested(now)) => *now,
            _ => Instant::now(),
        };

        let tick = self.opacity.tick(now);
        if tick.animating {
            Some(shader::Action::request_redraw_at(now + MIN_REDRAW_INTERVAL))
        } else {
            None
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        bounds: Rectangle,
    ) -> Self::Primitive {
        let tick = self.opacity.tick(Instant::now());

        Primitive {
            state: Mutex::new(match self.data.borrow_mut().take() {
                Some(yuv) => State::Pending { yuv, bounds },
                None => State::Prepared {
                    bounds,
                    image_dimensions: self.dimensions,
                    sampling_factor: self.sampling_factor,
                },
            }),
            opacity: tick.opacity,
            scale_mode: self.scale_mode,
        }
    }
}

#[derive(Debug)]
pub struct Primitive {
    state: Mutex<State>,
    opacity: f32,
    scale_mode: ScaleMode,
}

#[derive(Debug)]
enum State {
    Pending {
        yuv: Renderable,
        bounds: Rectangle,
    },
    Prepared {
        image_dimensions: Size<u32>,
        sampling_factor: f32,
        bounds: Rectangle,
    },
}

impl State {
    fn bounds(&self) -> Rectangle {
        match self {
            Self::Pending { bounds, .. } | Self::Prepared { bounds, .. } => *bounds,
        }
    }

    fn image_dimensions(&self) -> Size<u32> {
        match self {
            Self::Prepared {
                image_dimensions, ..
            } => *image_dimensions,
            Self::Pending { yuv, .. } => yuv.dimensions(),
        }
    }

    fn sampling_factor(&self) -> f32 {
        match self {
            Self::Prepared {
                sampling_factor, ..
            } => *sampling_factor,
            Self::Pending { yuv, .. } => yuv.downsampling_factor(),
        }
    }

    fn draw_bounds(&self, scale_mode: ScaleMode) -> Rectangle {
        match scale_mode {
            ScaleMode::Stretch => self.bounds(),
            ScaleMode::Cover => {
                let bounds = self.bounds();
                let image = self.image_dimensions();

                if bounds.width <= 0.0
                    || bounds.height <= 0.0
                    || image.width == 0
                    || image.height == 0
                {
                    return bounds;
                }

                let container_aspect = bounds.width / bounds.height;
                let image_aspect = image.width as f32 / image.height as f32;

                if image_aspect > container_aspect {
                    let width = bounds.height * image_aspect;
                    let x = bounds.x - (width - bounds.width) * 0.5;
                    Rectangle {
                        x,
                        y: bounds.y,
                        width,
                        height: bounds.height,
                    }
                } else {
                    let height = bounds.width / image_aspect;
                    let y = bounds.y - (height - bounds.height) * 0.5;
                    Rectangle {
                        x: bounds.x,
                        y,
                        width: bounds.width,
                        height,
                    }
                }
            }
        }
    }
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        let original_bounds = state.bounds();
        let draw_bounds = state.draw_bounds(self.scale_mode);
        let size = Size::from(original_bounds.size());
        let target_size = Size::from(viewport.logical_size());
        let scale_factor = viewport.scale_factor();

        match state.deref() {
            State::Pending { yuv, .. } => {
                if !pipeline.ensure_texture(yuv.dimensions()) {
                    return;
                }
                pipeline.prepare_texture(device, yuv.dimensions());

                pipeline.update_uniforms(
                    queue,
                    &Uniforms::new(
                        size,
                        yuv.dimensions().into(),
                        yuv.downsampling_factor(),
                        target_size,
                        self.opacity,
                    ),
                );
                pipeline.update_frame(queue, yuv);
                pipeline.update_vertices(queue, draw_bounds, target_size, scale_factor);
            }
            State::Prepared {
                image_dimensions,
                sampling_factor,
                ..
            } => {
                pipeline.update_uniforms(
                    queue,
                    &Uniforms::new(
                        size,
                        (*image_dimensions).into(),
                        *sampling_factor,
                        target_size,
                        self.opacity,
                    ),
                );
                pipeline.update_vertices(queue, draw_bounds, target_size, scale_factor);
            }
        }

        *state = State::Prepared {
            bounds: state.bounds(),
            image_dimensions: state.image_dimensions(),
            sampling_factor: state.sampling_factor(),
        };
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Ok(state) = self.state.lock() {
            pipeline.draw(render_pass, state.bounds());
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ScaleMode, State};
    use crate::yuv::{Renderable, Size};
    use iced::Rectangle;

    #[test]
    fn cover_expands_width_for_wide_video() {
        let state = State::Prepared {
            image_dimensions: Size {
                width: 200,
                height: 100,
            },
            sampling_factor: 1.0,
            bounds: Rectangle {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 100.0,
            },
        };

        let draw = state.draw_bounds(ScaleMode::Cover);
        assert_eq!(draw.width, 200.0);
        assert_eq!(draw.height, 100.0);
        assert_eq!(draw.x, -40.0);
        assert_eq!(draw.y, 20.0);
    }

    #[test]
    fn stretch_uses_original_bounds() {
        let state = State::Pending {
            yuv: Renderable::Y444 {
                data: vec![0; 3],
                dimensions: Size {
                    width: 1,
                    height: 1,
                },
            },
            bounds: Rectangle {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            },
        };

        let draw = state.draw_bounds(ScaleMode::Stretch);
        assert_eq!(draw.x, 1.0);
        assert_eq!(draw.y, 2.0);
        assert_eq!(draw.width, 3.0);
        assert_eq!(draw.height, 4.0);
    }
}
