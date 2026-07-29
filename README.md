# bevy_kitty

Render a Bevy 2D app into a terminal, using the [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Real pixels, uploaded once each and then moved around with a few dozen bytes per
frame. A 2D game is playable over SSH.

## The two modes

kitty's image protocol works in two steps: upload a rectangle of RGBA pixels, then
place it at a character cell. The crate implements two strategies for turning a Bevy
frame into those uploads and placements.

| | `KittyMode::Sprite` | `KittyMode::Frame` |
|---|---|---|
| What it sends | one upload per distinct bitmap, then placements | the whole composited plate |
| Fidelity | flat images only, no per-pixel shaders | everything the GPU composites |
| Cost | ~9 KB/s ongoing (measured) | ~307 KB per changed frame |
| Use | the real renderer, works over SSH | localhost, and the reference oracle |

Measured over a 14 second run of a real game at 1920x1080, sprite mode sent 158 image
uploads and zero re-uploads. That is 5.7 MB of one-time pixels, then 9.1 KB/s covering
animation, movement and text.

Frame mode is the ground truth to diff against when sprite mode looks wrong. Several
real bugs were found that way.

[`docs/bevy-to-kitty.md`](docs/bevy-to-kitty.md) walks the whole path from Bevy
components to the bytes on the wire, with real captured escape sequences and
annotated figures.

## Usage

```rust,ignore
use bevy::prelude::*;
use bevy_kitty::prelude::*;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins
            .set(WindowPlugin { primary_window: None, ..default() })
            .disable::<bevy::winit::WinitPlugin>())
        // Disabling winit removes the app runner, so supply a headless one.
        .add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
            std::time::Duration::from_secs_f64(1.0 / 20.0)))
        .add_plugins(KittyPlugin {
            config: KittyConfig {
                virtual_size: UVec2::new(320, 180),
                ..default()
            },
        })
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands) {
    // The marker is the whole required integration: it says whose view to paint.
    commands.spawn((Camera2d, KittyCamera));
}
```

Input arrives as messages, so nothing in the crate knows what your game does with a
click:

```rust,ignore
fn route_input(
    mut clicks: MessageReader<KittyClick>,
    mut keys: MessageReader<KittyKey>,
) {
    for click in clicks.read() {
        // click.viewport is in virtual_size space, ready for
        // Camera::viewport_to_world_2d
    }
    for key in keys.read() { /* ... */ }
}
```

Order your handler `.after(KittySet::Input)` and a click or keystroke is seen on the
frame it arrives.

Run the examples from a kitty-capable terminal (kitty, Ghostty, WezTerm, Konsole):

```sh
cargo run -p bevy_kitty --example bouncing_sprite
cargo run -p bevy_kitty --example hello_text
cargo run -p bevy_kitty --example chat_ui        # a real bevy_ui interface
```

## Features

```toml
default = ["sprite", "text", "ui", "input"]
sprite  = []                    # the placement renderer, the SSH-viable one
text    = ["sprite"]            # the Text2d glyph pass, and bevy_ui's Text
ui      = ["sprite", "bevy/bevy_ui"]  # bevy_ui nodes: backgrounds, borders, images
input   = ["dep:crossterm"]     # mouse and keyboard from the terminal
frame   = []                    # GPU readback, faithful but ~307 KB/frame
```

Turn `ui` off for a game that draws entirely in world space. The build then does not
pay for `bevy_ui` at all.

## What gets drawn

| Bevy component | How it reaches the terminal |
|---|---|
| `Sprite` (disk image) | the file's pixels, cropped to its atlas cell |
| `Sprite` (`color`, no image) | a 1x1 tile of that colour, stretched |
| `Text2d` | one reusable glyph image per letter, colour and size |
| `bevy_ui` `Text` | the same glyph pass, at no extra cost |
| `BackgroundColor` | a 1x1 tile stretched to the node's box |
| `BorderColor` | four tiles, one per edge, at the resolved widths |
| `Outline` | four tiles outside the box |
| `ImageNode` | the image, honouring its atlas cell, `rect` and flips |

`bevy_ui`'s `Text` widget fills the same `TextLayoutInfo` and rasterises into the same
font atlas as `Text2d`, so one pass and one glyph cache serve both. An "A" in a menu
button and an "A" in a world-space label, at the same size and colour, are a single
upload.

Measured on the `chat_ui` example: a 21-node interface draws as 25 rectangles from 5
uploads, with 157 glyphs from 82 glyph images. After the first frame it costs zero
bytes until something changes.

## Where pixels come from

Bevy frees a sprite image's CPU-side copy once it is uploaded to the GPU, so
`Assets<Image>` is usually empty for ordinary sprites. A renderer that only read the
ECS would draw nothing.

The default `PixelSource` therefore reads the original file back off disk, reproducing
Bevy's default `<root>/assets/<path>` layout, with the root from `BEVY_ASSET_ROOT` or
`CARGO_MANIFEST_DIR`. If your assets are packed, embedded or virtual, implement
`PixelSource` yourself and set `KittyConfig::pixel_source`.

The font atlas is the exception. Bevy keeps that copy CPU-persistent, so the glyph
pass reads it directly.

## Text scaling

A game drawing at 320x180 authors fonts at 4 to 10 px. At `text_scale: 1.0` a letter
is a roughly 5x7 pixel blob. Upscaling cannot add detail, so the glyph has to be
rasterised larger in the first place. Setting the render target's `scale_factor` is
what makes Bevy do that.

`logical_size = physical_size / scale_factor`, so raising the scale factor alone would
shrink the logical viewport and break every coordinate in the app. The plugin scales
the target's pixel size by the same factor, keeping the logical viewport at exactly
`virtual_size`.

The default is 4.0. It is clamped to `1.0..=16.0`, and ignored in frame mode, which
reads the target back expecting exactly `virtual_size`.

## What it does not do

Limits of the approach.

- **No `Mesh2d`, no custom materials.** A placement renderer cannot draw arbitrary
  geometry. Those need `frame` mode.
- **No rounded corners or gradients on UI nodes.** A placement is a rectangle, so
  `border_radius` is ignored and `BackgroundGradient` needs `frame` mode. At terminal
  scale a 4 px radius is a fraction of a cell.
- **No per-pixel full-screen shaders in sprite mode.** A flat alpha tile
  approximates a colour grade acceptably. CRT curvature and lightning do not
  survive.
- **Text cannot be sharper than the cell grid.** Legible is achievable.
- **`RenderLayers` is not consulted.** The sprite pass draws every visible `Sprite`,
  so a sprite on a layer your camera does not watch is still drawn. That is equivalent
  for a single-camera, single-layer app. It is wrong if you use layers to hide things.
- **Text sorts above world sprites, not above screen-space overlays.** A sprite at a
  high world z draws over text, which is what a real window does too. See
  `TEXT_Z_BIAS`.

## Traps

Each of these cost real debugging time. They all fail silently.

**`ViewVisibility` lies about text when headless.** It is set by frustum culling in
the render pipeline. `Text2d`'s `Aabb` is computed after layout, so culling never
marks it visible and every string vanishes with no error. The text pass reads
`InheritedVisibility`, which is the propagated authored intent. The sprite pass keeps
`ViewVisibility`, because for sprites it genuinely works.

**Terminal cells are not square.** At 1920x1080 over 212x51 they are 9.06 x 21.18
px. Sizing an image with kitty's `c`/`r` keys rounds to whole cells, so the error
differs per axis: on a 5x7 glyph that was 20.8% too wide but 0.8% too tall, i.e.
visibly squashed text. Glyphs are therefore pre-scaled to exact pixels and placed
with `c`/`r` omitted. Sprites keep cell sizing. Their error is about 3%, and
depth-scaling varies their size continuously, so pre-scaling every size would destroy
the cache.

**Cell size is floored, so fit the cell-aligned area, not the raw pixel count.**
212 columns of 9 px covers 1908, not 1920. Scaling against 1920 put the far edge of
the world at column 213 of a 212-column grid, and off-grid placements are drawn in
the wrong place.

**One image id per bitmap, one placement id per entity.** Two separate bugs came
from getting this wrong. Sharing a placement id between different image ids makes
kitty *stack* the images rather than replace them, which looks like ghosting. Giving
each entity its own image id and re-transmitting on every animation frame re-sent
64x64 sheets about 48 times per 4 seconds.

**Quantise animated colours before using them as a cache key.** Fades sweep alpha
continuously. At full precision every tick produced a new key and a new upload,
measured at ~478 redundant uploads and ~2 MB in a 4 second window.

**Text needs `.after(update_text2d_layout)`.** That system fills
`TextLayoutInfo.glyphs`. Query before it and you get an empty vector and no text,
with no error.

**An SSH pty reports the cell grid but not the pixel size.** Ask the terminal with
`CSI 14 t`, and note the reply carries *height before width*. Guessing a cell size
gets the aspect wrong and misplaces everything. Query once and cache: the round trip
needs raw mode briefly and would otherwise race the input reader for keystrokes.

**Register message types even when input is off.** A reader of an unregistered
message type panics, so an app that routes `KittyClick` would crash the moment input
was disabled, which is the normal configuration when stdout is a pipe.

**`bevy_ui` will not pick an image-target camera by itself.** `DefaultUiCamera` takes
either a single entity marked `IsDefaultUiCamera` or the highest-ordered camera whose
`RenderTarget` is a window. An offscreen camera can therefore never be chosen
implicitly, and headless there is no window either. Without that marker `bevy_ui` finds
no camera, never propagates a `ComputedUiRenderTargetInfo`, and resolves every
`Val::Percent` against a zero viewport. The plugin inserts the marker for you. The
symptom without the marker: a complete and correct interface renders in a fraction of
the terminal and never grows.

**UI positions are physical pixels, `Text2d` positions are world units.** The UI pass
divides by `ComputedNode::inverse_scale_factor`. The text pass divides by
`TextLayoutInfo::scale_factor`. They are different numbers with a similar purpose, and
using the wrong one scales the whole interface by the text-scale factor.

**No tty means no terminal size.** Redirect stdout to a file and `TIOCGWINSZ` fails, so
the renderer falls back to 80x24 and says so. Set `KittyConfig::terminal_size` when
capturing output, or a correct render at a third of the expected size will look like a
layout bug. The log line is `TIOCGWINSZ failed (rc=-1); falling back to 80x24`.

**Measuring bandwidth with a sliding window double-counts the one-time uploads.** It
reads about five times too high. Count whole-run, and split uploads from placements.

## Maturity

This is an 0.1. It is tested against exactly one game, one terminal (kitty 0.47.3),
one platform (macOS client, Linux host) and one resolution.

`bevy_ui` support is the least proven part. It draws backgrounds, borders, outlines,
images and text. The `chat_ui` example exercises it, and no real application's
interface has. The host game this came out of draws entirely in world space and uses
none of it.

## Licence

Apache-2.0 OR MIT.
