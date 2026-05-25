use futures_lite::stream;
use iced::widget::shader;
use iced::{Element, Fill, Subscription};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use std::sync::{Arc, OnceLock};

use crate::{
    AvError, AvEvent, PlaybackOptions, Program, Yuv, default_media_path, spawn_with_options,
};

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
    pub fn new(options: PlaybackOptions) -> Result<Self, AvError> {
        if AV_EVENT_RX.get().is_none() {
            let media_path = default_media_path()?;

            if media_path.exists() {
                let receiver = spawn_with_options(media_path, options);
                let _ = AV_EVENT_RX.set(Arc::new(tokio::sync::Mutex::new(receiver)));
            } else {
                debug!("AV media file not found; overlay playback disabled");
            }
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
