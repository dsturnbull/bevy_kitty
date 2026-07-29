//! Integration tests that build a real `App` with the plugin.
//!
//! The unit tests cover pure functions. These cover the wiring, which is where the
//! failures are the kind you only see at runtime: a message type that was never
//! registered, a system set that does not exist, a resource nobody inserted.

use bevy::prelude::*;
use bevy_kitty::prelude::*;

/// A minimal app: the plugin under test, plus the bits of Bevy it needs to build
/// its schedules. No rendering, no window, no GPU.
///
/// The asset types are registered by hand rather than by adding `SpritePlugin`,
/// which would drag in the render pipeline. A real app gets them from its own
/// plugins; here they only need to exist so the passes' `Res<Assets<..>>`
/// parameters validate.
fn test_app(config: KittyConfig) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(AssetPlugin::default());
    app.init_asset::<Image>();
    app.init_asset::<bevy::image::TextureAtlasLayout>();
    app.init_resource::<ButtonInput<KeyCode>>();
    app.add_plugins(KittyPlugin { config });
    app
}

/// Input off (stdout is a pipe under `cargo test`, so raw mode would fail anyway).
fn headless_config() -> KittyConfig {
    KittyConfig {
        virtual_size: UVec2::new(320, 180),
        terminal_size: Some(TermSize {
            cols: 80,
            rows: 24,
            xpixel: 640,
            ypixel: 384,
        }),
        input: false,
        text: false,
        ..default()
    }
}

/// A system that reads clicks, exactly as a host app would.
///
/// Gated because `KittyClick` is: naming it unconditionally stopped this whole test
/// target compiling without the `input` feature.
#[cfg(feature = "input")]
fn read_clicks(mut clicks: MessageReader<KittyClick>, mut seen: Local<u32>) {
    for _ in clicks.read() {
        *seen += 1;
    }
}

#[test]
#[cfg(feature = "input")]
fn a_click_reader_works_even_when_input_is_disabled() {
    // The regression this exists for: message types were registered inside the
    // `if config.input` branch, so with input off a reader of KittyClick panicked
    // with "Message not initialized" on the first frame. That is the normal
    // configuration when stdout is redirected to a file, so it broke every
    // captured run.
    let mut app = test_app(headless_config());
    app.add_systems(Update, read_clicks);
    app.update();
    app.update();
}

#[test]
#[cfg(feature = "input")]
fn a_key_reader_works_even_when_input_is_disabled() {
    fn read_keys(mut keys: MessageReader<KittyKey>) {
        for _ in keys.read() {}
    }
    let mut app = test_app(headless_config());
    app.add_systems(Update, read_keys);
    app.update();
}

#[test]
fn the_config_is_available_as_a_resource() {
    let app = test_app(headless_config());
    let config = app.world().resource::<KittyConfig>();
    assert_eq!(config.virtual_size, UVec2::new(320, 180));
}

#[test]
fn a_host_system_can_order_against_the_input_set() {
    // KittySet::Input must exist as a configured set even with input off, or an
    // app that orders `.after(KittySet::Input)` fails to build its schedule.
    //
    // Ordered against a noop rather than the click reader on purpose: `KittySet` is
    // NOT feature-gated, so this claim has to hold without the `input` feature too,
    // and that is exactly the configuration where you would doubt the set exists.
    fn noop() {}
    let mut app = test_app(headless_config());
    app.add_systems(Update, noop.after(KittySet::Input));
    app.update();
}

#[test]
fn a_host_system_can_order_against_the_render_set() {
    fn noop() {}
    let mut app = test_app(headless_config());
    app.add_systems(PostUpdate, noop.after(KittySet::Render));
    app.update();
}

#[test]
fn ticking_without_a_camera_does_not_panic() {
    // A real app spawns its camera in Startup, and the plugin's own systems run
    // before it exists on the first frames. Warning and retrying is correct;
    // unwrapping would crash on boot.
    let mut app = test_app(headless_config());
    for _ in 0..5 {
        app.update();
    }
}

#[test]
fn a_camera_gets_a_render_target_attached() {
    // The offscreen target is what gives `world_to_viewport` a coordinate space
    // when there is no window. Without it every placement would fail silently.
    use bevy::camera::RenderTarget;

    let mut app = test_app(headless_config());
    let cam = app.world_mut().spawn((Camera2d, KittyCamera)).id();
    app.update();
    app.update();
    assert!(
        app.world().entity(cam).contains::<RenderTarget>(),
        "the KittyCamera should have been retargeted to an offscreen image"
    );
}

#[test]
fn frame_mode_boots_too() {
    let config = KittyConfig {
        mode: KittyMode::Frame,
        ..headless_config()
    };
    let mut app = test_app(config);
    app.world_mut().spawn((Camera2d, KittyCamera));
    app.update();
}

#[cfg(feature = "ui")]
#[test]
fn the_kitty_camera_claims_bevy_uis_default_camera_slot() {
    // The trap this guards, which cost the most time of anything in this crate.
    //
    // `DefaultUiCamera::get` picks either a single entity marked `IsDefaultUiCamera`,
    // or the highest-ordered camera whose `RenderTarget` is a WINDOW. A camera
    // rendering to an image can therefore never be chosen implicitly, and headless
    // there is no window at all. Without the marker, `bevy_ui` finds no camera, never
    // propagates a `ComputedUiRenderTargetInfo`, and lays every root out against a
    // zero viewport.
    //
    // The failure is silent and looks like a scaling bug: a complete, correct
    // interface renders in a fraction of the terminal and never grows.
    let mut app = test_app(headless_config());
    let cam = app.world_mut().spawn((Camera2d, KittyCamera)).id();
    app.update();
    app.update();
    assert!(
        app.world()
            .entity(cam)
            .contains::<bevy::ui::IsDefaultUiCamera>(),
        "the KittyCamera must claim bevy_ui's default camera slot, or every \
         Val::Percent resolves against a zero viewport"
    );
}

#[test]
fn bevy_really_does_put_a_white_image_behind_the_default_handle() {
    // This test exists to pin the FACT the sprite pass's dispatch depends on, so
    // that if a future Bevy stops inserting it (or changes its size) we find out
    // here rather than through every UI panel rendering as one white pixel.
    //
    // The trap: a colour-only sprite (`Sprite { color, custom_size, .. }`) leaves
    // `image` at `Handle::default()`, and `ImagePlugin` inserts a real 1x1 white
    // `Image` there, CPU data and all (bevy_image 0.19 image.rs:222). So a naive
    // "does this handle resolve in Assets<Image>?" test says YES for every panel,
    // and the pass would upload that single white pixel instead of synthesising a
    // custom_size tile in the sprite's colour. sprite.rs excludes the default
    // handle explicitly for this reason.
    let mut app = App::new();
    app.add_plugins(AssetPlugin::default());
    app.add_plugins(bevy::image::ImagePlugin::default());

    let images = app.world().resource::<Assets<Image>>();
    let default_img = images
        .get(&Handle::<Image>::default())
        .expect("Bevy should insert an Image at Handle::default()");

    assert_eq!(
        (default_img.width(), default_img.height()),
        (1, 1),
        "the default image is a 1x1 placeholder; if this grew, re-check the \
         sprite pass's solid-tile dispatch"
    );
    assert!(
        default_img.data.is_some(),
        "the default image keeps CPU data, which is exactly why an \
         Assets<Image> lookup cannot distinguish it from a real sprite"
    );
}
