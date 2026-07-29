# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Extracted from the game it was written for into a standalone repository. No API
changes were made during the move: the code, tests and examples are the same
ones that were passing in-tree.

## [0.1.0] - unreleased

First shape of the crate. Not published to crates.io yet; see the maturity note
in `README.md` for why.

### Added

- `KittyPlugin` and `KittyConfig`: render a Bevy 2D app into a kitty-protocol
  terminal. `KittyCamera` on your camera is the whole required integration.
- `KittyMode::Sprite`: upload each distinct bitmap once, then send cheap
  per-frame placements. Measured at ~9 KB/s ongoing after 5.7 MB of one-time
  uploads, which is what makes a 2D game playable over SSH.
- `KittyMode::Frame`: GPU readback of the whole composited plate, behind the
  `frame` feature. Faithful but ~307 KB per changed frame, and the reference
  oracle to diff sprite mode against.
- `Text2d` glyph pass, reusing Bevy's own font atlas so a repeated letter at the
  same size and colour is a single upload.
- `bevy_ui` pass behind the `ui` feature: node backgrounds, borders, outlines and
  images. UI text comes through the existing glyph pass at no extra cost.
- Terminal mouse and keyboard behind the `input` feature, surfaced as `KittyClick`
  and `KittyKey` messages, ordered by `KittySet::Input`.
- `PixelSource` trait with `DiskPixels` and `AssetPixels`, because Bevy frees a
  sprite image's CPU-side copy once it reaches the GPU.
- `proto` module: pure functions appending kitty escape bytes to a `Vec<u8>`, with
  no Bevy dependency at all.
- Examples: `bouncing_sprite`, `hello_text`, `chat_ui`.
- `docs/bevy-to-kitty.md`: the full path from Bevy components to bytes on the
  wire, with real captured escape sequences and 11 generated figures.
