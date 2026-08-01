//! Render a Bevy 2D app into a terminal, using the kitty graphics protocol.
//!
//! The protocol uploads a rectangle of RGBA pixels, then places it at a character
//! cell. This crate turns what Bevy would have drawn into a set of uploads and
//! placements. It does that in few enough bytes that it works over SSH.
//!
//! # The two modes
//!
//! | | [`KittyMode::Sprite`] | [`KittyMode::Frame`] |
//! |---|---|---|
//! | What it sends | one upload per distinct bitmap, then placements | the whole composited plate |
//! | Fidelity | flat images only, no per-pixel shaders | everything the GPU composites |
//! | Cost | ~10 KB/s ongoing | ~307 KB per changed frame |
//! | Use | the real renderer, works over SSH | localhost, and the reference oracle |
//!
//! Frame mode needs the `frame` feature. It is the ground truth to diff against
//! when sprite mode looks wrong. Several real bugs were found that way.
//!
//! # Usage
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_kitty::prelude::*;
//!
//! # fn spawn_camera(mut commands: Commands) {
//! // Mark the camera whose view gets painted into the terminal.
//! commands.spawn((Camera2d, KittyCamera));
//! # }
//! ```
//!
//! Then add [`KittyPlugin`] with a [`KittyConfig`] naming your virtual
//! resolution. Input arrives as [`KittyClick`] and [`KittyKey`] messages. Nothing
//! in this crate needs to know what your game does with a click.
//!
//! # What this crate does not do
//!
//! Limits of the approach:
//!
//! - **No `Mesh2d`, no custom materials.** A placement renderer cannot draw
//!   arbitrary geometry. Those need `frame` mode.
//! - **No rounded corners or gradients on UI nodes.** A placement is a rectangle,
//!   so `border_radius` is ignored. `BackgroundGradient` needs `frame` mode.
//! - **No per-pixel full-screen shaders in sprite mode.** A flat alpha tile
//!   approximates a colour grade acceptably. CRT curvature and lightning do not
//!   survive.
//! - **Text cannot be sharper than the cell grid.** Legible is achievable.
//!
//! # Traps
//!
//! Each of these cost real debugging time. They all fail silently.
//!
//! - **`ViewVisibility` lies about text when headless.** The render pipeline sets
//!   it by frustum culling. `Text2d`'s `Aabb` is computed after layout, so culling
//!   never marks it visible and every string vanishes with no error. The text pass
//!   reads `InheritedVisibility`. The sprite pass keeps `ViewVisibility`, because
//!   for sprites it genuinely works.
//! - **Terminal cells are not square.** At 1920x1080 over 212x51 they are
//!   9.06 x 21.18 px. Sizing an image with kitty's `c`/`r` keys rounds to whole
//!   cells, so the error differs per axis. On a 5x7 glyph that is +20.8% wide but
//!   +0.8% tall. The result is visibly squashed text. The glyph pass pre-scales
//!   glyphs to exact pixels and places them with `c`/`r` omitted. The sprite pass
//!   keeps cell sizing.
//! - **One image id per bitmap, one placement id per entity.** Sharing a placement
//!   id between image ids makes kitty stack images rather than replace them. That
//!   looks like ghosting.
//! - **The text pass must run after `update_text2d_layout`.** That system fills
//!   `TextLayoutInfo.glyphs`. Query earlier and you get an empty vector and blank
//!   text, with no error.
//! - **An SSH pty reports the cell grid but not the pixel size.** [`term`] asks the
//!   terminal for it with a `CSI 14 t` query. Guessing a cell size gets the aspect
//!   wrong.
//! - **Do not measure bandwidth with a sliding window.** It double-counts the
//!   one-time uploads, so the figure reads about five times too high. Count
//!   whole-run, and split uploads from placements.
//!
//! # Where the output goes
//!
//! Escape sequences go to stdout. `bevy_log` writes to stderr, so verbose logging
//! never corrupts the graphics stream. Never `println!` from a game using this
//! crate.

#![doc = include_str!("../docs/bevy-to-kitty.md")]

use bevy::prelude::*;

pub mod proto;
pub mod term;

pub mod pixels;

#[cfg(feature = "sprite")]
pub mod sprite;

#[cfg(feature = "text")]
pub mod text;

#[cfg(feature = "ui")]
pub mod ui;

#[cfg(feature = "frame")]
pub mod frame;

#[cfg(feature = "input")]
pub mod input;

pub use term::{FitBox, TermSize};

/// Everything you need to add the renderer to an app.
pub mod prelude {
    #[cfg(feature = "input")]
    pub use crate::input::{KittyClick, KittyKey};
    pub use crate::pixels::{AssetPixels, Bitmap, DiskPixels, PixelRequest, PixelSource};
    pub use crate::term::TermSize;
    pub use crate::{KittyCamera, KittyConfig, KittyMode, KittyPlugin, KittySet};
}

/// Marker for the camera whose view is painted into the terminal.
///
/// Exactly one entity should carry this. The camera is used two ways: its render
/// target defines the coordinate space that `Camera::world_to_viewport` projects
/// into, and its transform is the view matrix every placement is positioned
/// through.
///
/// This is why the plugin needs a marker rather than just taking any camera: an
/// app may have several (a UI camera, a render-to-texture camera), and only one
/// of them is the terminal's view.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct KittyCamera;

/// Which terminal-render strategy to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KittyMode {
    /// Per-sprite placement deltas over uploaded bitmaps. Thin enough for SSH.
    #[default]
    Sprite,
    /// Full framebuffer plate every changed frame: the reference oracle.
    /// Requires the `frame` feature.
    Frame,
}

/// How the renderer is configured. Inserted as a resource by [`KittyPlugin`], so
/// systems can read it.
#[derive(Resource, Clone, Debug)]
pub struct KittyConfig {
    /// The virtual resolution your game draws at, e.g. 320x180. Everything is
    /// scaled and letterboxed from this, preserving aspect.
    ///
    /// This is the crate's replacement for a game's own resolution constants.
    pub virtual_size: UVec2,

    /// Which render strategy to use.
    pub mode: KittyMode,

    /// Rasterise text at this multiple of the virtual resolution, so small fonts
    /// are not upscaled blobs.
    ///
    /// A game drawing at 320x180 authors fonts at 4 to 10 px. At `1.0` a letter
    /// is a roughly 5x7 pixel blob, and upscaling cannot add detail. Raising this
    /// sets the render target's `scale_factor`, which makes Bevy rasterise glyphs
    /// larger in the first place.
    ///
    /// The subtlety, handled for you: `logical_size = physical_size /
    /// scale_factor`, so raising the scale factor alone would shrink the logical
    /// viewport and break every coordinate. The target's pixel size is scaled by
    /// the same factor, keeping the logical viewport at exactly `virtual_size`.
    ///
    /// Clamped to 1.0..=16.0. Ignored in frame mode, which reads the target back
    /// expecting exactly `virtual_size`.
    pub text_scale: f32,

    /// `None` asks the terminal how big it is. Set it when stdout is a pipe (no
    /// tty to query) or to pin a size for testing.
    pub terminal_size: Option<TermSize>,

    /// Render `Text2d` as reusable glyph images. Needs the `text` feature.
    ///
    /// This also covers `bevy_ui`'s `Text` widget, which populates the same
    /// `TextLayoutInfo` and the same font atlas, so UI labels come out of the same
    /// pass at no extra cost.
    pub text: bool,

    /// Render `bevy_ui` nodes: backgrounds, borders, outlines and images. Needs the
    /// `ui` feature.
    ///
    /// Turn it off for a game that draws entirely in world space, which saves the
    /// pass walking a node tree that is always empty.
    pub ui: bool,

    /// Read mouse and keyboard from the terminal. Needs the `input` feature.
    ///
    /// Turn this off when stdout is redirected to a file: raw mode on a
    /// non-terminal is meaningless, and the reader thread would compete for stdin.
    pub input: bool,

    /// Where sprite pixels come from. Defaults to [`pixels::DiskPixels`], which
    /// reproduces Bevy's default asset layout.
    ///
    /// The indirection exists because pixels cannot simply come from the ECS:
    /// Bevy frees a sprite image's CPU-side copy once it is uploaded to the GPU,
    /// so `Assets<Image>` is usually empty for ordinary sprites. The font atlas is
    /// the exception, because Bevy keeps that CPU-persistent on purpose.
    pub pixel_source: std::sync::Arc<dyn pixels::PixelSource>,
}

impl Default for KittyConfig {
    fn default() -> Self {
        Self {
            virtual_size: UVec2::new(320, 180),
            mode: KittyMode::default(),
            text_scale: 4.0,
            terminal_size: None,
            text: true,
            ui: true,
            input: true,
            pixel_source: std::sync::Arc::new(pixels::DiskPixels::from_env()),
        }
    }
}

impl KittyConfig {
    /// `text_scale`, clamped, warning if it was out of range.
    pub(crate) fn clamped_text_scale(&self) -> f32 {
        if !(1.0..=16.0).contains(&self.text_scale) {
            warn!(
                "[kitty] text_scale {} out of range 1..16 - clamping",
                self.text_scale
            );
        }
        self.text_scale.clamp(1.0, 16.0)
    }

    /// The render target's scale factor for the configured mode.
    ///
    /// Frame mode stays at 1.0 because it reads the target back expecting exactly
    /// `virtual_size` pixels. Sprite mode uses the text scale.
    pub(crate) fn target_scale_factor(&self) -> f32 {
        match self.mode {
            KittyMode::Sprite => self.clamped_text_scale(),
            KittyMode::Frame => 1.0,
        }
    }
}

/// System sets this crate adds, so an app can order its own systems against
/// them.
///
/// [`KittySet::Input`] is the useful one: order your input handling `.after()` it
/// and an injected click or keystroke is seen the SAME frame it arrives, rather
/// than one frame late.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittySet {
    /// Terminal input is drained into messages here, in `Update`.
    Input,
    /// Placements are emitted here, in `PostUpdate`.
    Render,
}

/// The renderer.
///
/// Add this after your own camera setup, so the [`KittyCamera`] entity exists for
/// it to retarget.
#[derive(Default)]
pub struct KittyPlugin {
    pub config: KittyConfig,
}

impl KittyPlugin {
    /// The renderer with default configuration for a given virtual resolution.
    pub fn new(virtual_size: UVec2) -> Self {
        Self {
            config: KittyConfig {
                virtual_size,
                ..default()
            },
        }
    }
}

impl Plugin for KittyPlugin {
    fn build(&self, app: &mut App) {
        let config = self.config.clone();
        info!(
            "[kitty] mode {:?}, virtual {}x{}, text {}, input {}",
            config.mode, config.virtual_size.x, config.virtual_size.y, config.text, config.input
        );

        // Refuse silently-broken combinations loudly. Asking for a mode whose
        // feature is off would otherwise render nothing at all, with no clue why.
        #[cfg(not(feature = "frame"))]
        if config.mode == KittyMode::Frame {
            error!(
                "[kitty] mode Frame requested but the `frame` cargo feature is off - \
                 nothing will be drawn. Enable it, or use KittyMode::Sprite."
            );
        }
        #[cfg(not(feature = "sprite"))]
        if config.mode == KittyMode::Sprite {
            error!(
                "[kitty] mode Sprite requested but the `sprite` cargo feature is off - \
                 nothing will be drawn."
            );
        }
        #[cfg(not(feature = "text"))]
        if config.text {
            warn!(
                "[kitty] config.text is on but the `text` cargo feature is off - \
                 no text will be rendered."
            );
        }
        #[cfg(not(feature = "ui"))]
        if config.ui {
            warn!(
                "[kitty] config.ui is on but the `ui` cargo feature is off - \
                 bevy_ui nodes will not be drawn."
            );
        }
        #[cfg(not(feature = "input"))]
        if config.input {
            warn!(
                "[kitty] config.input is on but the `input` cargo feature is off - \
                 the terminal will not be readable."
            );
        }

        app.insert_resource(config.clone());
        app.configure_sets(Update, KittySet::Input);
        app.configure_sets(PostUpdate, KittySet::Render);
        app.add_systems(Startup, enter_terminal);
        // Show the cursor again on a clean shutdown. The panic hook in
        // `enter_terminal` covers an unwinding exit; this covers `AppExit`.
        app.add_systems(Last, restore_cursor_on_exit);

        // Both modes need the camera pointed at an offscreen image. Frame mode
        // reads the pixels back; sprite mode never does, but
        // `Camera::world_to_viewport` derives its coordinate space from the render
        // target, and with no window there would otherwise be no space to project
        // into.
        //
        // Registered in BOTH `Startup` and `Update`, and the `Startup` copy is
        // load-bearing for `bevy_ui`. `bevy_render`'s `camera_system` runs in
        // `PostStartup` (as well as `PostUpdate`), and it is what fills
        // `Camera::computed.target_info`. `bevy_ui` reads that, once, via
        // `camera.physical_viewport_size()`, to decide what `Val::Percent(100)`
        // resolves against. Attach the target only in `Update` and that first
        // `PostStartup` pass sees no target at all, so the whole interface lays out
        // against a zero viewport, collapses to its content size, and never
        // recovers. It renders, which is what makes it confusing: a complete UI in
        // the top-left eighth of the terminal.
        //
        // The `Update` copy stays as the fallback for an app that spawns its camera
        // later than `Startup`, which the game does.
        app.init_resource::<TargetState>();
        app.add_systems(Startup, setup_render_target_once);
        app.add_systems(Update, setup_render_target_once);

        #[cfg(feature = "input")]
        {
            // Register the message types unconditionally, even when input is off:
            // a reader of an unregistered message type PANICS, so an app that
            // routes KittyClick would crash the moment input was disabled or raw
            // mode failed. See input::register_messages.
            input::register_messages(app);
            if config.input {
                input::build(app);
            }
        }

        #[cfg(feature = "frame")]
        if config.mode == KittyMode::Frame {
            frame::build(app);
        }

        #[cfg(feature = "sprite")]
        if config.mode == KittyMode::Sprite {
            sprite::build(app);

            #[cfg(feature = "text")]
            if config.text {
                text::build(app);
            }

            #[cfg(feature = "ui")]
            if config.ui {
                ui::build(app);
            }
        }
    }
}

/// Tracks the offscreen render target, shared by both modes.
#[derive(Resource, Default)]
pub(crate) struct TargetState {
    /// The offscreen image the camera renders into.
    pub(crate) image: Option<Handle<Image>>,
    /// Whether the camera has already been retargeted.
    pub(crate) retargeted: bool,
}

/// Once, when the [`KittyCamera`] exists: create an offscreen RGBA image and
/// point the camera at it.
fn setup_render_target_once(
    mut commands: Commands,
    mut state: ResMut<TargetState>,
    mut images: ResMut<Assets<Image>>,
    config: Res<KittyConfig>,
    camera_q: Query<Entity, With<KittyCamera>>,
    mut cameras: Query<&mut Camera>,
) {
    use bevy::asset::RenderAssetUsages;
    use bevy::camera::RenderTarget;
    use bevy::image::Image;
    use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};

    if state.retargeted {
        return;
    }
    let Ok(camera) = camera_q.single() else {
        // Camera not spawned yet this frame; try again next Update.
        return;
    };

    let sf = config.target_scale_factor();
    let size = Extent3d {
        width: (config.virtual_size.x as f32 * sf).round().max(1.0) as u32,
        height: (config.virtual_size.y as f32 * sf).round().max(1.0) as u32,
        depth_or_array_layers: 1,
    };
    // Rgba8UnormSrgb matches a standard sRGB pipeline; the bytes come back
    // sRGB-encoded, which is exactly what kitty's f=32 wants.
    let mut image = Image::new_fill(
        size,
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    // The camera renders INTO this (RENDER_ATTACHMENT); frame mode's readback
    // copies OUT of it (COPY_SRC); TEXTURE_BINDING keeps it samplable.
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC | TextureUsages::RENDER_ATTACHMENT;
    let handle = images.add(image);

    // In Bevy 0.19 RenderTarget is a *component* the Camera requires, so
    // inserting our own overrides the default Window target.
    //
    // `scale_factor` is Bevy's hi-dpi lever: `update_text2d_layout` reads it via
    // `Camera::target_scaling_factor()` and rasterises glyphs at `font_size *
    // scale_factor`, then records it in `TextLayoutInfo.scale_factor`. Because the
    // glyph transform divides by that same value, raising it makes text SHARPER
    // without moving anything.
    let mut target: bevy::camera::ImageRenderTarget = handle.clone().into();
    target.scale_factor = sf;
    commands.entity(camera).insert(RenderTarget::Image(target));

    // Also set the camera's `viewport` explicitly, which matters for `bevy_ui` and
    // is worth explaining because the failure is so indirect.
    //
    // `bevy_ui` resolves `Val::Percent` against `camera.physical_viewport_size()`.
    // That reads `camera.computed.target_info`, which `bevy_render::camera_system`
    // fills, but only when the camera was just added OR its render target changed
    // according to asset-event change detection. We attach the target to a camera
    // that already existed, in the same tick we create its image, so neither
    // condition reliably fires and `target_info` stays `None`. `bevy_ui` then lays
    // the whole interface out against a zero viewport.
    //
    // The symptom is not a crash or a blank screen, which is what makes it expensive:
    // every `Percent` collapses to its content size, so a complete and correct-looking
    // interface renders in a fraction of the terminal and never grows. `viewport` is
    // read before that change-detection path, so setting it sidesteps the whole
    // question. It is a FIELD on `Camera`, not a component, so this is a mutation of
    // the existing camera rather than an insert.
    if let Ok(mut cam) = cameras.get_mut(camera) {
        cam.viewport = Some(bevy::camera::Viewport {
            physical_position: UVec2::ZERO,
            physical_size: UVec2::new(size.width, size.height),
            ..default()
        });
    }

    // Claim the camera as `bevy_ui`'s default, which is REQUIRED here rather than
    // merely tidy.
    //
    // `DefaultUiCamera::get` picks a camera two ways: a single entity marked
    // `IsDefaultUiCamera`, or else the highest-ordered camera whose `RenderTarget` is
    // a WINDOW. Read that second clause carefully: it filters on
    // `RenderTarget::Window` and discards everything else, so a camera rendering to
    // an image can never be chosen implicitly. Headless there is no window either, so
    // without this marker `bevy_ui` finds no camera, never propagates a
    // `ComputedUiRenderTargetInfo`, and lays every root out against a zero viewport.
    //
    // The `#[cfg]` matters: `IsDefaultUiCamera` only exists when bevy_ui is compiled
    // in, which the `ui` feature controls.
    #[cfg(feature = "ui")]
    commands.entity(camera).insert(bevy::ui::IsDefaultUiCamera);

    state.image = Some(handle);
    state.retargeted = true;
    info!(
        "[kitty] render target armed: {}x{} physical at {sf:.1}x, logical stays {}x{}",
        size.width, size.height, config.virtual_size.x, config.virtual_size.y
    );
}

/// Clear the screen and hide the cursor, once.
fn enter_terminal() {
    let mut buf = Vec::new();
    proto::enter_screen(&mut buf);
    if !write_stdout(&buf, "terminal setup") {
        return;
    }
    // Hiding the cursor is an edit to the USER's terminal, so it has to be undone
    // however the process ends. The input module restores it on Ctrl-C and on
    // reader shutdown, but input is optional (and raw mode fails outright when
    // stdout is a pipe), so a normal exit with input off would otherwise leave the
    // user with an invisible cursor and no idea why.
    install_cursor_restore();
    info!("[kitty] terminal prepared (cleared, cursor hidden)");
}

/// Ensure the cursor is shown again when the process exits.
///
/// Registered once. `atexit`-style cleanup via a guard whose `Drop` runs on normal
/// exit; a hard `SIGKILL` obviously cannot be covered, which is why the escape is
/// also emitted by the input module's own teardown.
fn install_cursor_restore() {
    use std::sync::OnceLock;
    static GUARD: OnceLock<()> = OnceLock::new();
    if GUARD.set(()).is_err() {
        return; // already installed
    }
    // A leaked thread-local guard would not run, so use a real hook: Rust has no
    // atexit, but a panic hook plus an explicit call covers the two realistic
    // paths. The panic hook matters most: an unwinding app would otherwise leave
    // the terminal in a state the user has to `reset` out of.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        show_cursor();
        prev(info);
    }));
}

/// On `AppExit`, put the cursor back.
///
/// Runs in `Last` so it fires on the frame the app decides to quit, before the
/// runner returns. Without it, a `cargo run` that ends normally leaves the user
/// staring at a terminal with no cursor.
fn restore_cursor_on_exit(mut exits: MessageReader<AppExit>) {
    if exits.read().next().is_some() {
        info!("[kitty] app exiting, restoring cursor");
        show_cursor();
    }
}

/// Emit the show-cursor escape. Safe to call more than once.
pub fn show_cursor() {
    use std::io::Write as _;
    let mut buf = Vec::new();
    proto::leave_screen(&mut buf);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&buf);
    let _ = out.flush();
}

/// Write a buffer of escape bytes to stdout, logging failures with `what` for
/// context. Shared by every pass so no pass invents its own error handling.
pub(crate) fn write_stdout(buf: &[u8], what: &str) -> bool {
    use std::io::Write as _;
    if buf.is_empty() {
        return true;
    }
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(buf) {
        error!("[kitty] {what} stdout write failed: {e}");
        return false;
    }
    if let Err(e) = out.flush() {
        error!("[kitty] {what} stdout flush failed: {e}");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_scale_is_clamped_both_ways() {
        let too_small = KittyConfig {
            text_scale: 0.1,
            ..default()
        };
        assert_eq!(too_small.clamped_text_scale(), 1.0);
        let too_big = KittyConfig {
            text_scale: 99.0,
            ..default()
        };
        assert_eq!(too_big.clamped_text_scale(), 16.0);
        let ok = KittyConfig {
            text_scale: 4.0,
            ..default()
        };
        assert_eq!(ok.clamped_text_scale(), 4.0);
    }

    #[test]
    fn frame_mode_never_scales_its_target() {
        // Frame mode reads the target back expecting exactly virtual_size, so a
        // scale factor other than 1.0 would silently corrupt every decoded frame.
        let cfg = KittyConfig {
            mode: KittyMode::Frame,
            text_scale: 4.0,
            ..default()
        };
        assert_eq!(cfg.target_scale_factor(), 1.0);
        let cfg = KittyConfig {
            mode: KittyMode::Sprite,
            text_scale: 4.0,
            ..default()
        };
        assert_eq!(cfg.target_scale_factor(), 4.0);
    }

    #[test]
    fn default_is_sprite_mode_at_320x180() {
        let cfg = KittyConfig::default();
        assert_eq!(cfg.mode, KittyMode::Sprite);
        assert_eq!(cfg.virtual_size, UVec2::new(320, 180));
    }
}
