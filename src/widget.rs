use futures_lite::stream;
use iced::widget::shader;
use iced::{Element, Fill, Subscription};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::{AvError, AvEvent, MediaSource, Program, Yuv, default_media_source, spawn_with_source};

static AV_EVENT_RX: OnceLock<Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<AvEvent>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Yuv),
    Ended,
    Error(String),
}

pub struct State {
    program: Option<Program>,
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
        if AV_EVENT_RX.get().is_none() {
            let receiver = spawn_with_source(media_source);
            let _ = AV_EVENT_RX.set(Arc::new(tokio::sync::Mutex::new(receiver)));
        }

        Ok(Self { program: None })
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::Frame(frame) => {
                if let Some(program) = self.program.as_mut() {
                    program.update_frame(frame);
                } else {
                    self.program = Some(Program::new(frame));
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

    pub fn subscription<MessageT: 'static>(
        &self,
        map: fn(Message) -> MessageT,
    ) -> Subscription<MessageT> {
        if AV_EVENT_RX.get().is_some() {
            Subscription::run(av_event_stream).map(map)
        } else {
            Subscription::none()
        }
    }
}

fn av_event_stream() -> impl futures_lite::Stream<Item = Message> {
    stream::unfold((), |_| async move {
        let receiver = AV_EVENT_RX.get()?.clone();
        let mut guard = receiver.lock().await;
        let event = guard.recv().await?;

        let message = match event {
            AvEvent::VideoFrame(frame) => Message::Frame(frame),
            AvEvent::PlaybackEnded => Message::Ended,
            AvEvent::PlaybackError(error) => Message::Error(error),
        };

        Some((message, ()))
    })
}
