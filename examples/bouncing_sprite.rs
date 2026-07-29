//! The smallest thing that proves the renderer works: a coloured square bouncing
//! around a 320x180 world, drawn in your terminal.
//!
//! Run it from a kitty-capable terminal (kitty, Ghostty, WezTerm, Konsole):
//!
//! ```sh
//! cargo run -p bevy_kitty --example bouncing_sprite
//! ```
//!
//! Ctrl-C to quit. Logs go to stderr, so redirect them if they get in the way:
//! `cargo run ... 2>/dev/null`.
//!
//! There is deliberately no image file here. The squares are pathless
//! solid-colour sprites, which the renderer synthesises, so the example needs no
//! assets directory and proves the crate needs no game.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use bevy::window::{ExitCondition, WindowPlugin};

use bevy_kitty::prelude::*;

/// The virtual resolution the "game" draws at.
const VIRTUAL: UVec2 = UVec2::new(320, 180);

/// Velocity in virtual pixels per second.
#[derive(Component)]
struct Velocity(Vec2);

fn main() {
    let tick = Duration::from_secs_f64(1.0 / 20.0);

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    // No OS window: the camera renders into an offscreen image and
                    // bevy_kitty paints the terminal itself.
                    primary_window: None,
                    // With no windows the default `OnAllClosed` would exit
                    // immediately.
                    exit_condition: ExitCondition::DontExit,
                    close_when_requested: false,
                    ..default()
                })
                .disable::<bevy::winit::WinitPlugin>(),
        )
        // Disabling winit removes the app runner too, so supply a headless one or
        // `run()` returns after a single tick.
        .add_plugins(ScheduleRunnerPlugin::run_loop(tick))
        .add_plugins(KittyPlugin {
            config: KittyConfig {
                virtual_size: VIRTUAL,
                ..default()
            },
        })
        .add_systems(Startup, setup)
        .add_systems(Update, bounce)
        .run();
}

fn setup(mut commands: Commands) {
    // The camera has to carry KittyCamera: that is what tells the renderer whose
    // view to paint, and its render target is the coordinate space every placement
    // is projected through.
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

    // A backdrop, so the bouncing squares have something to move against. Drawn
    // behind everything via a negative z.
    commands.spawn((
        Sprite {
            color: Color::srgb(0.08, 0.09, 0.15),
            custom_size: Some(Vec2::new(VIRTUAL.x as f32, VIRTUAL.y as f32)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, -1.0),
    ));

    // Three squares at different speeds, so animation is obviously live rather
    // than a still frame.
    let squares = [
        (Color::srgb(1.0, 0.4, 0.5), 24.0, Vec2::new(70.0, 45.0)),
        (Color::srgb(0.4, 0.9, 1.0), 16.0, Vec2::new(-95.0, 62.0)),
        (Color::srgb(1.0, 0.85, 0.3), 10.0, Vec2::new(55.0, -80.0)),
    ];
    for (i, (color, size, vel)) in squares.into_iter().enumerate() {
        commands.spawn((
            Sprite {
                color,
                custom_size: Some(Vec2::splat(size)),
                ..default()
            },
            Transform::from_xyz(0.0, 0.0, i as f32),
            Velocity(vel),
        ));
    }
}

/// Move each square and reflect it off the edges of the virtual world.
fn bounce(time: Res<Time>, mut q: Query<(&mut Transform, &mut Velocity, &Sprite)>) {
    let dt = time.delta_secs();
    let half = Vec2::new(VIRTUAL.x as f32, VIRTUAL.y as f32) * 0.5;
    for (mut xf, mut vel, sprite) in q.iter_mut() {
        let r = sprite.custom_size.unwrap_or(Vec2::ONE) * 0.5;
        xf.translation.x += vel.0.x * dt;
        xf.translation.y += vel.0.y * dt;
        if xf.translation.x.abs() + r.x > half.x {
            vel.0.x = -vel.0.x;
            xf.translation.x = xf.translation.x.clamp(-(half.x - r.x), half.x - r.x);
        }
        if xf.translation.y.abs() + r.y > half.y {
            vel.0.y = -vel.0.y;
            xf.translation.y = xf.translation.y.clamp(-(half.y - r.y), half.y - r.y);
        }
    }
}
