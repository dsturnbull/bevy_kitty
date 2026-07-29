//! A `bevy_ui` interface drawn in your terminal: panels, borders, a scrolling
//! message log and an input line.
//!
//! ```sh
//! cargo run -p bevy_kitty --example chat_ui --features ui
//! ```
//!
//! Ctrl-C to quit. Type to edit the input line, Enter to post a message, and 1/2/3
//! to pick a suggestion chip.
//!
//! This is the example that proves UI support works, because it is built the way a
//! real Bevy app builds an interface: flexbox `Node`s, `BackgroundColor`,
//! `BorderColor`, and `Text` widgets. None of it is world-space. The layout is
//! adapted from a chat-UI prototype so it exercises the awkward parts rather than a
//! single centred box: nested rows and columns, percentage and fixed widths, a
//! one-sided border used as a divider, a growing spacer, and text at four sizes.
//!
//! What to look for in the terminal:
//!
//! * the two panels meet at a one-pixel divider, which is a `BorderColor` with only
//!   its right edge set,
//! * the message log fills downward and older lines scroll off the top,
//! * the interest meter's border changes colour without its panel being redrawn,
//! * the log's text is clipped at the panel edge rather than painting over it.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::text::FontSize;
use bevy::window::{ExitCondition, WindowPlugin};

use bevy_kitty::prelude::*;

/// The virtual resolution. Bigger than a pixel-art game would use, because an
/// interface needs room for text: a terminal cell is about 9x21 physical pixels, so
/// 640x360 over a 212x51 grid gives roughly 3 virtual pixels per cell column.
const VIRTUAL: UVec2 = UVec2::new(640, 360);

const BG_DARK: Color = Color::srgb(0.07, 0.07, 0.10);
const PORTRAIT_BG: Color = Color::srgb(0.11, 0.11, 0.16);
const CHAT_BG: Color = Color::srgb(0.09, 0.09, 0.13);
const DIVIDER: Color = Color::srgb(0.25, 0.25, 0.34);
const TEXT_ACCENT: Color = Color::srgb(0.95, 0.85, 0.45);
const TEXT_MAIN: Color = Color::srgb(0.88, 0.90, 0.95);
const TEXT_DIM: Color = Color::srgb(0.50, 0.53, 0.62);
const CHIP_BG: Color = Color::srgb(0.16, 0.20, 0.28);

#[derive(Component)]
struct MessageLog;

#[derive(Component)]
struct InputLine;

#[derive(Component)]
struct MeterPanel;

#[derive(Component)]
struct MeterLabel;

/// What the user is typing, plus the posted history.
#[derive(Resource, Default)]
struct Chat {
    typing: String,
    posted: Vec<String>,
    interest: u32,
}

fn main() {
    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    close_when_requested: false,
                    ..default()
                })
                .disable::<bevy::winit::WinitPlugin>(),
        )
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 15.0,
        )))
        .add_plugins(KittyPlugin {
            config: KittyConfig {
                virtual_size: VIRTUAL,
                // UI text is authored at real point sizes, so it needs far less
                // help than a pixel-art game's 4 px font. 2x still sharpens it.
                text_scale: 2.0,
                // Honour KITTY_TERM_SIZE=colsxrows@wxh, so the output can be captured
                // to a file. Redirecting stdout means there is no tty to ask, and the
                // renderer then falls back to 80x24 (it says so, loudly). Without an
                // override, a captured run therefore draws a correct interface at a
                // third of the size, which is easy to mistake for a scaling bug.
                terminal_size: std::env::var("KITTY_TERM_SIZE")
                    .ok()
                    .and_then(|s| TermSize::parse_spec(&s)),
                ..default()
            },
        })
        .init_resource::<Chat>()
        .add_systems(Startup, setup)
        .add_systems(Update, (handle_keys, sync_log, sync_meter))
        .run();
}

fn setup(mut commands: Commands, mut chat: ResMut<Chat>) {
    // A UI camera still needs the KittyCamera marker: it is what tells the renderer
    // which camera's view to paint.
    commands.spawn((Camera2d, KittyCamera));

    chat.posted
        .push("Bex: Witless. But I watched those two.".into());
    chat.posted.push("You: More than the numbers?".into());

    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Row,
                ..default()
            },
            BackgroundColor(BG_DARK),
        ))
        .with_children(|root| {
            // Left: a fixed-width column, divided from the right by a border on one
            // edge only. A single outline rect could not express that.
            root.spawn((
                Node {
                    width: Val::Px(200.0),
                    height: Val::Percent(100.0),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::all(Val::Px(12.0)),
                    row_gap: Val::Px(6.0),
                    border: UiRect::right(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(PORTRAIT_BG),
                BorderColor {
                    right: DIVIDER,
                    ..BorderColor::DEFAULT
                },
            ))
            .with_children(|p| {
                p.spawn((
                    Text::new("BEX"),
                    TextFont {
                        font_size: FontSize::Px(28.0),
                        ..default()
                    },
                    TextColor(TEXT_ACCENT),
                ));
                p.spawn((
                    Text::new("Quartermaster, The Kettle"),
                    TextFont {
                        font_size: FontSize::Px(13.0),
                        ..default()
                    },
                    TextColor(TEXT_DIM),
                ));
                // A spacer that grows: proof the pass reads the computed box rather
                // than anything authored.
                p.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                // The meter: a bordered box whose border colour changes at runtime.
                p.spawn((
                    Node {
                        width: Val::Percent(100.0),
                        padding: UiRect::all(Val::Px(8.0)),
                        border: UiRect::all(Val::Px(2.0)),
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.13, 0.15, 0.13)),
                    BorderColor::all(Color::srgb(0.20, 0.45, 0.25)),
                    MeterPanel,
                ))
                .with_children(|m| {
                    m.spawn((
                        Text::new("Interest"),
                        TextFont {
                            font_size: FontSize::Px(13.0),
                            ..default()
                        },
                        TextColor(TEXT_DIM),
                    ));
                    m.spawn((
                        Text::new("0%"),
                        TextFont {
                            font_size: FontSize::Px(24.0),
                            ..default()
                        },
                        TextColor(TEXT_MAIN),
                        MeterLabel,
                    ));
                });
            });

            // Right: the chat column, filling the rest.
            root.spawn((
                Node {
                    flex_grow: 1.0,
                    height: Val::Percent(100.0),
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
                BackgroundColor(CHAT_BG),
            ))
            .with_children(|chat_col| {
                // The log. `Overflow::clip_y` is what produces a CalculatedClip, so
                // this is the node that exercises clipping.
                chat_col.spawn((
                    Node {
                        flex_grow: 1.0,
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(Val::Px(12.0)),
                        row_gap: Val::Px(4.0),
                        overflow: Overflow::clip_y(),
                        ..default()
                    },
                    MessageLog,
                ));

                // Suggestion chips: three small panels in a row.
                chat_col
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(6.0),
                        padding: UiRect::all(Val::Px(10.0)),
                        ..default()
                    })
                    .with_children(|row| {
                        for (i, label) in ["Ask about the ice", "Change subject", "Say nothing"]
                            .into_iter()
                            .enumerate()
                        {
                            row.spawn((
                                Node {
                                    padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
                                    border: UiRect::all(Val::Px(1.0)),
                                    ..default()
                                },
                                BackgroundColor(CHIP_BG),
                                BorderColor::all(DIVIDER),
                            ))
                            .with_children(|chip| {
                                chip.spawn((
                                    Text::new(format!("{}. {label}", i + 1)),
                                    TextFont {
                                        font_size: FontSize::Px(13.0),
                                        ..default()
                                    },
                                    TextColor(TEXT_MAIN),
                                ));
                            });
                        }
                    });

                // The input line, separated by a top border.
                chat_col
                    .spawn((
                        Node {
                            padding: UiRect::all(Val::Px(10.0)),
                            border: UiRect::top(Val::Px(2.0)),
                            ..default()
                        },
                        BorderColor {
                            top: DIVIDER,
                            ..BorderColor::DEFAULT
                        },
                    ))
                    .with_children(|line| {
                        line.spawn((
                            Text::new("> _"),
                            TextFont {
                                font_size: FontSize::Px(18.0),
                                ..default()
                            },
                            TextColor(TEXT_MAIN),
                            InputLine,
                        ));
                    });
            });
        });
}

/// Terminal keystrokes arrive as real `KeyboardInput` messages, so this is the same
/// code a windowed app would write.
fn handle_keys(
    mut keys: MessageReader<KeyboardInput>,
    mut chat: ResMut<Chat>,
    mut input: Query<&mut Text, With<InputLine>>,
) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        match &key.logical_key {
            Key::Enter if !chat.typing.is_empty() => {
                let line = std::mem::take(&mut chat.typing);
                chat.posted.push(format!("You: {line}"));
                chat.interest = (chat.interest + 12).min(100);
            }
            Key::Backspace => {
                chat.typing.pop();
            }
            Key::Character(s) => {
                let s = s.to_string();
                // Digits pick a chip rather than typing, matching the prototype.
                match s.as_str() {
                    "1" | "2" | "3" if chat.typing.is_empty() => {
                        let which = ["Ask about the ice", "Change subject", "Say nothing"]
                            [s.parse::<usize>().unwrap_or(1) - 1];
                        chat.posted.push(format!("You: {which}"));
                        chat.interest = (chat.interest + 20).min(100);
                    }
                    _ => chat.typing.push_str(&s),
                }
            }
            _ => {}
        }
    }
    for mut text in input.iter_mut() {
        text.0 = format!("> {}_", chat.typing);
    }
}

/// Rebuild the log's children when the history changes.
fn sync_log(
    mut commands: Commands,
    chat: Res<Chat>,
    log: Query<Entity, With<MessageLog>>,
    mut last: Local<usize>,
) {
    if chat.posted.len() == *last {
        return;
    }
    *last = chat.posted.len();
    let Ok(log) = log.single() else { return };
    commands.entity(log).despawn_related::<Children>();
    for line in &chat.posted {
        let colour = if line.starts_with("You:") {
            TEXT_MAIN
        } else {
            TEXT_ACCENT
        };
        commands.entity(log).with_children(|p| {
            p.spawn((
                Text::new(line.clone()),
                TextFont {
                    font_size: FontSize::Px(15.0),
                    ..default()
                },
                TextColor(colour),
            ));
        });
    }
}

/// Recolour the meter's border as interest rises, without touching its panel.
fn sync_meter(
    chat: Res<Chat>,
    mut panel: Query<&mut BorderColor, With<MeterPanel>>,
    mut label: Query<&mut Text, With<MeterLabel>>,
) {
    if !chat.is_changed() {
        return;
    }
    let t = chat.interest as f32 / 100.0;
    let colour = Color::srgb(0.20 + 0.6 * t, 0.45, 0.25 - 0.1 * t);
    for mut border in panel.iter_mut() {
        *border = BorderColor::all(colour);
    }
    for mut text in label.iter_mut() {
        text.0 = format!("{}%", chat.interest);
    }
}
