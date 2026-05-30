use iced::widget::{container, row, text};
use iced::{Element, Fill, Subscription, Task};
use iced_av1::{widget, MediaSource, ScaleMode};

const VIDEO_BYTES: &[u8] = include_bytes!("../chimp.mkv");
const SEEK_OFFSET_NS: u64 = 2_000_000_000;

#[derive(Debug, Clone)]
enum Message {
    Player(usize, widget::Message),
}

struct App {
    players: Vec<widget::State>,
    init_error: Option<String>,
}

fn main() -> iced::Result {
    iced::application(init, update, view)
        .title("iced-av1: seek")
        .subscription(subscription)
        .run()
}

fn init() -> (App, Task<Message>) {
    let source = MediaSource::Bytes(VIDEO_BYTES.to_vec().into());
    let mut first = match widget::State::new_with_media_source(source.clone()) {
        Ok(state) => state,
        Err(error) => {
            return (
                App {
                    players: Vec::new(),
                    init_error: Some(error.to_string()),
                },
                Task::none(),
            );
        }
    };

    let mut second = match widget::State::new_with_media_source(source) {
        Ok(state) => state,
        Err(error) => {
            return (
                App {
                    players: Vec::new(),
                    init_error: Some(error.to_string()),
                },
                Task::none(),
            );
        }
    };

    first.set_scale_mode(ScaleMode::Cover { offset: 0.0 });
    second.set_scale_mode(ScaleMode::Cover { offset: 0.0 });

    if let Err(error) = second.seek(SEEK_OFFSET_NS) {
        return (
            App {
                players: Vec::new(),
                init_error: Some(error.to_string()),
            },
            Task::none(),
        );
    }

    (
        App {
            players: vec![first, second],
            init_error: None,
        },
        Task::none(),
    )
}

fn update(app: &mut App, message: Message) {
    match message {
        Message::Player(index, player_message) => {
            if let Some(player) = app.players.get_mut(index) {
                player.update(player_message);
            }
        }
    }
}

fn subscription(app: &App) -> Subscription<Message> {
    Subscription::batch(
        app.players
            .iter()
            .enumerate()
            .map(|(index, player)| player.subscription(move |msg| Message::Player(index, msg))),
    )
}

fn view(app: &App) -> Element<'_, Message> {
    if let Some(error) = &app.init_error {
        return container(text(error.clone())).center(Fill).into();
    }

    row![
        player_column(app.players.first(), map_player_0),
        player_column(app.players.get(1), map_player_1),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill)
    .into()
}

fn player_column<'a>(
    player: Option<&'a widget::State>,
    mapper: fn(widget::Message) -> Message,
) -> Element<'a, Message> {
    let content = match player.and_then(|state| state.overlay_view(mapper)) {
        Some(video) => video,
        None => container(text("Loading video...")).center(Fill).into(),
    };

    container(content)
        .width(iced::Length::FillPortion(1))
        .height(Fill)
        .into()
}

fn map_player_0(message: widget::Message) -> Message {
    Message::Player(0, message)
}

fn map_player_1(message: widget::Message) -> Message {
    Message::Player(1, message)
}
