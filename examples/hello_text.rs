//! Text rendering on its own: a few `Text2d` strings drawn in your terminal as
//! reusable glyph images.
//!
//! ```sh
//! cargo run -p bevy_kitty --example hello_text
//! ```
//!
//! Watch the glyph log lines on stderr. The interesting number is "new glyph
//! uploads", which should fall to zero within a second or two even though the
//! ticking counter keeps changing: every distinct letter is uploaded once and then
//! only re-placed. That reuse is the whole reason text is affordable over a thin
//! link.
//!
//! Uses Bevy's built-in font (the `default_font` feature), so there are no assets.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::window::{ExitCondition, WindowPlugin};

use bevy_kitty::prelude::*;

const VIRTUAL: UVec2 = UVec2::new(320, 180);

/// Marks the string that shows a live counter.
#[derive(Component)]
struct Ticker;

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
            1.0 / 10.0,
        )))
        .add_plugins(KittyPlugin {
            config: KittyConfig {
                virtual_size: VIRTUAL,
                // Text authored for a 320x180 screen is 4 to 10 px tall, so
                // rasterising at 1x gives an unreadable blob no amount of upscaling
                // can fix. 4x is the default for exactly this reason.
                text_scale: 4.0,
                ..default()
            },
        })
        .add_systems(Startup, setup)
        .add_systems(Update, tick)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: bevy::camera::ScalingMode::FixedVertical {
                viewport_height: VIRTUAL.y as f32,
            },
            ..OrthographicProjection::default_2d()
        }),
        KittyCamera,
    ));

    // A dark backdrop, so light text has contrast to read against.
    commands.spawn((
        Sprite {
            color: Color::srgb(0.05, 0.05, 0.09),
            custom_size: Some(Vec2::new(VIRTUAL.x as f32, VIRTUAL.y as f32)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, -1.0),
    ));

    // `font_size` is a `FontSize` enum in Bevy 0.19, not a bare f32: Px is an
    // absolute size, the Vw/Vh variants are relative to the viewport.
    let line = |text: &str, y: f32, size: f32, color: Color| {
        (
            Text2d::new(text),
            TextFont {
                font_size: FontSize::Px(size),
                ..default()
            },
            TextColor(color),
            Anchor::CENTER,
            Transform::from_xyz(0.0, y, 1.0),
        )
    };

    commands.spawn(line("bevy_kitty", 60.0, 24.0, Color::WHITE));
    commands.spawn(line(
        "Text2d, rendered as kitty glyphs",
        36.0,
        10.0,
        Color::srgb(0.7, 0.8, 1.0),
    ));
    commands.spawn(line(
        "every letter uploaded once, placed many",
        20.0,
        10.0,
        Color::srgb(0.6, 0.65, 0.75),
    ));

    // The same characters in a different colour are genuinely different images:
    // kitty cannot tint a placement, so the colour is baked into the pixels and
    // therefore into the cache key.
    commands.spawn(line(
        "a different colour costs new uploads",
        -8.0,
        10.0,
        Color::srgb(1.0, 0.5, 0.4),
    ));

    commands.spawn((
        Text2d::new("tick 0"),
        TextFont {
            font_size: FontSize::Px(14.0),
            ..default()
        },
        TextColor(Color::srgb(1.0, 0.85, 0.3)),
        Anchor::CENTER,
        Transform::from_xyz(0.0, -40.0, 1.0),
        Ticker,
    ));

    commands.spawn(line(
        "Ctrl-C to quit",
        -66.0,
        8.0,
        Color::srgb(0.45, 0.45, 0.5),
    ));
}

/// Change one string every tick, so glyph reuse is observable: the digits change
/// but the upload count stays at zero once each digit has been seen.
fn tick(mut q: Query<&mut Text2d, With<Ticker>>, mut n: Local<u32>) {
    *n += 1;
    for mut text in q.iter_mut() {
        text.0 = format!("tick {}", *n);
    }
}
