use futures_lite::stream;
use iced::widget::shader;
use iced::{Element, Fill, Subscription};
use iced_futures::subscription::{EventStream, Hasher, Recipe, from_recipe, into_recipes};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use std::any::TypeId;
use std::collections::HashMap;
use std::hash::Hash as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::{
    AvError, AvEvent, MediaSource, PlaybackControlError, PlaybackController, PlaybackOptions,
    Program, Yuv, default_media_source, spawn_session_with_source_and_options,
};
use crate::{OpacityController, OpacityOptions, OpacityTween};

static NEXT_SUBSCRIPTION_ID: AtomicU64 = AtomicU64::new(1);
static AV_EVENT_RXS: OnceLock<
    Mutex<HashMap<u64, Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<AvEvent>>>>>,
> = OnceLock::new();

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Yuv),
    Ended,
    Error(String),
}

pub struct State {
    program: Option<Program>,
    controller: PlaybackController,
    opacity: OpacityController,
    subscription_id: u64,
}

impl State {
    pub fn new() -> Result<Self, AvError> {
        Self::new_with_media_source(default_media_source()?)
    }

    pub fn new_with_media_directory(media_directory: impl Into<PathBuf>) -> Result<Self, AvError> {
        Self::new_with_media_source(MediaSource::Directory(media_directory.into()))
    }

    pub fn new_with_media_bytes(media_bytes: Vec<u8>) -> Result<Self, AvError> {
        Self::new_with_media_source(MediaSource::from_bytes(media_bytes))
    }

    pub fn new_with_media_source(media_source: MediaSource) -> Result<Self, AvError> {
        Self::new_with_media_source_and_options(media_source, PlaybackOptions::default())
    }

    pub fn new_with_media_source_and_options(
        media_source: MediaSource,
        playback_options: PlaybackOptions,
    ) -> Result<Self, AvError> {
        Self::new_with_media_source_and_opacity_options(
            media_source,
            playback_options,
            OpacityOptions::default(),
        )
    }

    pub fn new_with_media_source_and_opacity_options(
        media_source: MediaSource,
        playback_options: PlaybackOptions,
        opacity_options: OpacityOptions,
    ) -> Result<Self, AvError> {
        let session = spawn_session_with_source_and_options(media_source, playback_options);
        let receiver = Arc::new(tokio::sync::Mutex::new(session.events));
        let subscription_id = NEXT_SUBSCRIPTION_ID.fetch_add(1, Ordering::Relaxed);
        let mut receivers = receiver_map().lock().expect("AV receiver map poisoned");
        receivers.insert(subscription_id, receiver.clone());
        let opacity = OpacityController::new(opacity_options);

        Ok(Self {
            program: None,
            controller: session.controller,
            opacity,
            subscription_id,
        })
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::Frame(frame) => {
                if let Some(program) = self.program.as_mut() {
                    program.update_frame(frame);
                } else {
                    self.program = Some(Program::new_with_opacity(frame, self.opacity.clone()));
                }
            }
            Message::Ended => {
                debug!("AV playback finished");
            }
            Message::Error(error) => {
                warn!(%error, "AV playback error");
            }
        }
    }

    pub fn overlay_view<'a, MessageT: 'a>(
        &'a self,
        map: fn(Message) -> MessageT,
    ) -> Option<Element<'a, MessageT>> {
        let program = self.program.as_ref()?;
        let overlay: Element<'_, Message> = shader(program).width(Fill).height(Fill).into();
        Some(overlay.map(map))
    }

    pub fn overlay_view_owned<MessageT: 'static>(
        &self,
        map: fn(Message) -> MessageT,
    ) -> Option<Element<'static, MessageT>> {
        let program = self.program.as_ref()?.clone();
        let overlay: Element<'static, Message> = shader(program).width(Fill).height(Fill).into();
        Some(overlay.map(map))
    }

    pub fn subscription<MessageT: 'static, Mapper>(&self, map: Mapper) -> Subscription<MessageT>
    where
        Mapper: Fn(Message) -> MessageT + Clone + Send + 'static,
    {
        map_subscription(Subscription::run_with(self.subscription_id, av_event_stream), map)
    }

    pub fn seek(&self, timestamp_ns: u64) -> Result<(), PlaybackControlError> {
        self.controller.seek(timestamp_ns)
    }

    pub fn set_start_offset(&self, timestamp_ns: u64) -> Result<(), PlaybackControlError> {
        self.controller.set_start_offset(timestamp_ns)
    }

    pub fn controller(&self) -> PlaybackController {
        self.controller.clone()
    }

    pub fn opacity(&self) -> f32 {
        self.opacity.opacity()
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
}

impl Drop for State {
    fn drop(&mut self) {
        if let Ok(mut receivers) = receiver_map().lock() {
            receivers.remove(&self.subscription_id);
        }
    }
}

fn map_subscription<T: 'static, U: 'static, Mapper>(
    subscription: Subscription<T>,
    map: Mapper,
) -> Subscription<U>
where
    Mapper: Fn(T) -> U + Clone + Send + 'static,
{
    struct MapRecipe<T, U, Mapper>
    where
        T: 'static,
        U: 'static,
        Mapper: Fn(T) -> U + Send + 'static,
    {
        recipe: Box<dyn Recipe<Output = T>>,
        mapper: Mapper,
    }

    impl<T, U, Mapper> Recipe for MapRecipe<T, U, Mapper>
    where
        T: 'static,
        U: 'static,
        Mapper: Fn(T) -> U + Send + 'static,
    {
        type Output = U;

        fn hash(&self, state: &mut Hasher) {
            TypeId::of::<Mapper>().hash(state);
            self.recipe.hash(state);
        }

        fn stream(self: Box<Self>, input: EventStream) -> iced_futures::BoxStream<Self::Output> {
            iced_futures::futures::StreamExt::boxed(iced_futures::futures::StreamExt::map(
                self.recipe.stream(input),
                self.mapper,
            ))
        }
    }

    Subscription::batch(
        into_recipes(subscription)
            .into_iter()
            .map(|recipe| from_recipe(MapRecipe { recipe, mapper: map.clone() })),
    )
}

fn receiver_map()
-> &'static Mutex<HashMap<u64, Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<AvEvent>>>>> {
    AV_EVENT_RXS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn av_event_stream(subscription_id: &u64) -> futures_lite::stream::Boxed<Message> {
    let subscription_id = *subscription_id;
    futures_lite::StreamExt::boxed(stream::unfold((), move |_| async move {
        let receiver = {
            let receivers = receiver_map().lock().ok()?;
            receivers.get(&subscription_id).cloned()
        }?;

        let mut guard = receiver.lock().await;
        let event = guard.recv().await?;

        let message = match event {
            AvEvent::VideoFrame(frame) => Message::Frame(frame),
            AvEvent::PlaybackEnded => Message::Ended,
            AvEvent::PlaybackError(error) => Message::Error(error),
        };

        Some((message, ()))
    }))
}
