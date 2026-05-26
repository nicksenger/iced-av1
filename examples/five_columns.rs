use iced::widget::{container, row, text};
use iced::{Element, Fill, Subscription, Task};
use iced_av1::{ScaleMode, widget};

const COLUMN_COUNT: usize = 5;
const JUNGLE_MKV: &[u8] = include_bytes!("../jungle.mkv");

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
        .title("iced-av1: 5 columns (Cover)")
        .subscription(subscription)
        .run()
}

fn init() -> (App, Task<Message>) {
    let mut players = Vec::with_capacity(COLUMN_COUNT);

    for _ in 0..COLUMN_COUNT {
        let mut state = match widget::State::new_with_media_bytes(JUNGLE_MKV.to_vec()) {
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
        state.set_scale_mode(ScaleMode::Cover);
        players.push(state);
    }

    (
        App {
            players,
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
        player_column(app.players.get(2), map_player_2),
        player_column(app.players.get(3), map_player_3),
        player_column(app.players.get(4), map_player_4),
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

fn map_player_2(message: widget::Message) -> Message {
    Message::Player(2, message)
}

fn map_player_3(message: widget::Message) -> Message {
    Message::Player(3, message)
}

fn map_player_4(message: widget::Message) -> Message {
    Message::Player(4, message)
}
