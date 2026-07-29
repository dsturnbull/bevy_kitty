//! Where sprite pixels come from.
//!
//! This is the one abstraction the crate adds over the code it was extracted
//! from, and it exists because of a Bevy fact that is easy to trip over:
//!
//! > Bevy frees a sprite image's CPU-side copy once it is uploaded to the GPU.
//!
//! So `Assets<Image>` is usually **empty** for ordinary sprites, and a renderer
//! that only reads the ECS would draw nothing. The font atlas is the exception,
//! because Bevy keeps that CPU-persistent on purpose, which is why the glyph pass
//! can read it directly and the sprite pass cannot.
//!
//! [`DiskPixels`] therefore reads the original file back off disk, reproducing
//! Bevy's default asset layout. That is right for a game shipping loose asset
//! files and wrong for anyone using a custom `AssetSource`, packed assets or a
//! virtual filesystem, hence the trait.

use bevy::asset::{AssetId, AssetServer};
use bevy::image::Image;
use bevy::log::{error, warn};
use bevy::prelude::*;

/// Decoded RGBA8 pixels for one distinct bitmap: a whole image, or one atlas
/// cell.
#[derive(Clone, Debug)]
pub struct Bitmap {
    /// Tightly packed RGBA8, `w * h * 4` bytes, top row first.
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl Bitmap {
    /// A bitmap from raw parts, checking the length matches the dimensions.
    ///
    /// Returns `None` on a mismatch rather than panicking: a short buffer from a
    /// custom [`PixelSource`] should skip one sprite, not take down the app.
    pub fn new(rgba: Vec<u8>, w: u32, h: u32) -> Option<Self> {
        let want = (w as usize) * (h as usize) * 4;
        if rgba.len() != want {
            error!(
                "[kitty] bitmap is {} bytes but {w}x{h} RGBA needs {want} - skipping",
                rgba.len()
            );
            return None;
        }
        Some(Self { rgba, w, h })
    }
}

/// What the renderer wants pixels for.
///
/// `atlas_cell` is `Some` when the sprite draws one cell of a texture atlas, in
/// which case the source should return just that cell's pixels.
pub struct PixelRequest<'a> {
    /// The image asset the sprite points at.
    pub image: AssetId<Image>,
    /// The atlas cell rect to crop, if the sprite is atlased.
    pub atlas_cell: Option<URect>,
    /// CPU-side image assets. Usually empty for sprites, see the module docs.
    pub images: &'a Assets<Image>,
    /// Used to resolve an asset id back to its virtual path.
    pub assets: &'a AssetServer,
}

/// Resolves a sprite's asset id to actual pixels.
///
/// Implement this to serve pixels from somewhere other than loose files on disk:
/// a pack file, an embedded blob, a network cache.
///
/// Returning `None` skips drawing that sprite for this frame. That is the correct
/// answer while an asset is still loading, and it must not be treated as an
/// error, because the renderer will ask again next frame.
pub trait PixelSource: Send + Sync + 'static {
    /// RGBA8 pixels for a sprite, or `None` to skip drawing it.
    fn pixels(&self, req: &PixelRequest) -> Option<Bitmap>;

    /// A short name for logs, so a wrong source is diagnosable.
    fn name(&self) -> &'static str {
        "custom"
    }
}

impl std::fmt::Debug for dyn PixelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PixelSource({})", self.name())
    }
}

/// Reads pixels from the original files on disk, reproducing Bevy's default asset
/// layout: `<root>/assets/<virtual path>`.
///
/// This is the default, and it is what the renderer this crate came from always
/// did. It works whenever the app ships loose asset files that the process can
/// still read at runtime.
#[derive(Clone, Debug)]
pub struct DiskPixels {
    /// The directory containing `assets/`.
    pub root: std::path::PathBuf,
}

impl DiskPixels {
    /// Resolve the asset root the way Bevy's own default file source does:
    /// `BEVY_ASSET_ROOT`, else `CARGO_MANIFEST_DIR` (set by `cargo run`), else the
    /// working directory.
    ///
    /// Note the last fallback is a real trap for a shipped binary: run from
    /// anywhere but the game directory and every sprite silently fails to decode.
    /// It logs when it lands there.
    pub fn from_env() -> Self {
        if let Ok(root) = std::env::var("BEVY_ASSET_ROOT") {
            return Self { root: root.into() };
        }
        if let Ok(root) = std::env::var("CARGO_MANIFEST_DIR") {
            return Self { root: root.into() };
        }
        warn!(
            "[kitty] neither BEVY_ASSET_ROOT nor CARGO_MANIFEST_DIR is set, so sprite \
             pixels will be read relative to the working directory. Set BEVY_ASSET_ROOT \
             to the directory containing `assets/` if nothing draws."
        );
        Self { root: ".".into() }
    }

    /// A source rooted at an explicit directory (the one containing `assets/`).
    pub fn at(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The on-disk path for an asset id, or `None` for a generated/pathless
    /// image.
    fn disk_path(&self, req: &PixelRequest) -> Option<std::path::PathBuf> {
        let asset_path = req.assets.get_path(req.image)?;
        Some(self.root.join("assets").join(asset_path.path()))
    }
}

impl PixelSource for DiskPixels {
    fn pixels(&self, req: &PixelRequest) -> Option<Bitmap> {
        // Prefer the file. Fall back to CPU pixels for images that have no path,
        // which is how generated images (decoded at runtime, e.g. an animated
        // sticker) are drawn.
        match self.disk_path(req) {
            Some(path) => load_file_rgba(&path, req.atlas_cell),
            None => AssetPixels.pixels(req),
        }
    }

    fn name(&self) -> &'static str {
        "disk"
    }
}

/// Reads pixels only from CPU-side `Assets<Image>`.
///
/// Correct for images generated at runtime, which keep their CPU data. For
/// ordinary file-loaded sprites this returns `None` once Bevy has uploaded them
/// to the GPU and freed the CPU copy, so it is not a general substitute for
/// [`DiskPixels`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AssetPixels;

impl PixelSource for AssetPixels {
    fn pixels(&self, req: &PixelRequest) -> Option<Bitmap> {
        let img = req.images.get(req.image)?;
        crop_image_rgba(img, req.atlas_cell)
    }

    fn name(&self) -> &'static str {
        "assets"
    }
}

/// Decode an image file, optionally cropping to an atlas cell.
pub fn load_file_rgba(path: &std::path::Path, cell: Option<URect>) -> Option<Bitmap> {
    let img = match image::open(path) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            error!("[kitty] failed to decode {}: {e}", path.display());
            return None;
        }
    };
    let (iw, ih) = (img.width(), img.height());
    match cell {
        Some(rect) => {
            let (cx, cy) = (rect.min.x, rect.min.y);
            let (cw, ch) = (rect.width(), rect.height());
            if cx + cw > iw || cy + ch > ih {
                error!(
                    "[kitty] atlas cell {cx},{cy} {cw}x{ch} out of bounds for {iw}x{ih} in {}",
                    path.display()
                );
                return None;
            }
            let mut out = Vec::with_capacity((cw * ch * 4) as usize);
            for row in 0..ch {
                for col in 0..cw {
                    let p = img.get_pixel(cx + col, cy + row);
                    out.extend_from_slice(&p.0);
                }
            }
            Bitmap::new(out, cw, ch)
        }
        None => Bitmap::new(img.into_raw(), iw, ih),
    }
}

/// Copy CPU pixels out of an in-memory [`Image`], optionally cropping an atlas
/// cell, converting to RGBA8.
///
/// Returns `None` if the asset kept no CPU data, which is the normal state for a
/// file-loaded sprite and for anything created `RENDER_WORLD`-only.
pub fn crop_image_rgba(img: &Image, cell: Option<URect>) -> Option<Bitmap> {
    use bevy::render::render_resource::TextureFormat;

    let data = img.data.as_ref()?;
    let (iw, ih) = (img.width(), img.height());
    let fmt = img.texture_descriptor.format;
    // Only the 8-bit RGBA formats. Anything else would need a colour-space
    // conversion this crate deliberately does not attempt.
    if !matches!(
        fmt,
        TextureFormat::Rgba8UnormSrgb | TextureFormat::Rgba8Unorm
    ) {
        warn!("[kitty] image has unsupported format {fmt:?} - skipping");
        return None;
    }
    let stride = (iw as usize) * 4;
    let rect = cell.unwrap_or(URect::new(0, 0, iw, ih));
    let (cx, cy) = (rect.min.x, rect.min.y);
    let (cw, ch) = (rect.width(), rect.height());
    if cx + cw > iw || cy + ch > ih {
        error!("[kitty] image cell {cx},{cy} {cw}x{ch} out of bounds for {iw}x{ih}");
        return None;
    }
    if data.len() < stride * (ih as usize) {
        error!(
            "[kitty] image data short: {} bytes for {iw}x{ih} (want {})",
            data.len(),
            stride * (ih as usize)
        );
        return None;
    }
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in 0..ch as usize {
        let start = (cy as usize + row) * stride + (cx as usize) * 4;
        out.extend_from_slice(&data[start..start + (cw as usize) * 4]);
    }
    Bitmap::new(out, cw, ch)
}

// ------------------------------------------------------------------ transforms --

/// Is this tint effectively "no tint" (opaque white)? Then the untinted bitmap
/// can be shared and the per-pixel multiply skipped.
///
/// The transforms below are gated on the passes that call them. Without a gate
/// they are dead code in a build that excludes those passes, and `dead_code` is
/// an error under `-D warnings`.
#[cfg(feature = "sprite")]
pub(crate) fn is_white(c: &bevy::color::Srgba) -> bool {
    c.red >= 0.999 && c.green >= 0.999 && c.blue >= 0.999 && c.alpha >= 0.999
}

/// Build a `w`x`h` RGBA tile filled with `c`, for pathless solid-colour sprites
/// (UI panels, buttons, dividers, and flat-colour effect overlays).
#[cfg(feature = "sprite")]
pub(crate) fn solid_rgba(c: &bevy::color::Srgba, w: u32, h: u32) -> Vec<u8> {
    let px = [
        (c.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
        (c.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    ];
    px.iter()
        .copied()
        .cycle()
        .take((w as usize) * (h as usize) * 4)
        .collect()
}

/// Multiply RGBA pixels in place by a tint.
///
/// Kitty has no per-placement tint, so a sprite's colour has to be baked into the
/// uploaded pixels. That is also why the tint must be part of the cache key.
#[cfg(feature = "sprite")]
pub(crate) fn apply_tint(rgba: &mut [u8], c: &bevy::color::Srgba) {
    let (r, g, b, a) = (
        c.red.clamp(0.0, 1.0),
        c.green.clamp(0.0, 1.0),
        c.blue.clamp(0.0, 1.0),
        c.alpha.clamp(0.0, 1.0),
    );
    for px in rgba.chunks_exact_mut(4) {
        px[0] = (px[0] as f32 * r).round() as u8;
        px[1] = (px[1] as f32 * g).round() as u8;
        px[2] = (px[2] as f32 * b).round() as u8;
        px[3] = (px[3] as f32 * a).round() as u8;
    }
}

/// Mirror an RGBA buffer horizontally and/or vertically.
///
/// Kitty cannot mirror a placement, so a sprite's flip has to be baked into the
/// uploaded pixels, and therefore into its cache key.
#[cfg(feature = "sprite")]
pub(crate) fn flip_rgba(rgba: &[u8], w: u32, h: u32, fx: bool, fy: bool) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let src_y = if fy { h - 1 - y } else { y };
        for x in 0..w {
            let src_x = if fx { w - 1 - x } else { x };
            let s = (src_y * w + src_x) * 4;
            let d = (y * w + x) * 4;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    out
}

/// Resample an RGBA buffer to an exact pixel size.
///
/// Needed because kitty's `c`/`r` placement keys size an image in whole CELLS,
/// and cells are not square (e.g. 9.06 x 21.18 px), so the rounding error differs
/// per axis. For a 5x7 glyph that came out +20.8% wide but +0.8% tall, i.e.
/// visibly squashed text. Large images barely notice (a 384px sprite is off by
/// ~1.5%), but glyphs are wrecked. So glyphs are scaled here to their exact target
/// pixels and placed with `c`/`r` omitted, which kitty draws 1:1.
///
/// Downscaling averages; upscaling samples. Downscaling with nearest-neighbour
/// would throw away most pixels and undo the extra detail a hi-dpi render target
/// was asked to rasterise. Upscaling stays nearest, to keep pixel art crisp.
#[cfg(feature = "text")]
pub(crate) fn scale_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw_us, sh_us, dw_us, dh_us) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let mut out = vec![0u8; dw_us * dh_us * 4];
    if sw == 0 || sh == 0 {
        return out;
    }
    let downscaling = dw < sw || dh < sh;
    for dy in 0..dh_us {
        for dx in 0..dw_us {
            let d = (dy * dw_us + dx) * 4;
            if downscaling {
                // Box filter over the source rect this destination pixel covers.
                let x0 = (dx as f32 * sw as f32 / dw as f32).floor() as usize;
                let x1 = (((dx + 1) as f32 * sw as f32 / dw as f32).ceil() as usize).min(sw_us);
                let y0 = (dy as f32 * sh as f32 / dh as f32).floor() as usize;
                let y1 = (((dy + 1) as f32 * sh as f32 / dh as f32).ceil() as usize).min(sh_us);
                let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
                for sy in y0..y1.max(y0 + 1) {
                    for sx in x0..x1.max(x0 + 1) {
                        let s = (sy.min(sh_us - 1) * sw_us + sx.min(sw_us - 1)) * 4;
                        r += src[s] as u32;
                        g += src[s + 1] as u32;
                        b += src[s + 2] as u32;
                        a += src[s + 3] as u32;
                        n += 1;
                    }
                }
                let n = n.max(1);
                out[d] = (r / n) as u8;
                out[d + 1] = (g / n) as u8;
                out[d + 2] = (b / n) as u8;
                out[d + 3] = (a / n) as u8;
            } else {
                // +0.5 samples the centre of the destination pixel.
                let sy = (((dy as f32 + 0.5) * sh as f32 / dh as f32) as u32).min(sh - 1) as usize;
                let sx = (((dx as f32 + 0.5) * sw as f32 / dw as f32) as u32).min(sw - 1) as usize;
                let s = (sy * sw_us + sx) * 4;
                out[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "sprite")]
    use bevy::color::Srgba;

    #[test]
    fn bitmap_rejects_a_length_mismatch() {
        // A short buffer must skip one sprite, not panic mid-frame.
        assert!(Bitmap::new(vec![0; 16], 2, 2).is_some());
        assert!(Bitmap::new(vec![0; 15], 2, 2).is_none());
        assert!(Bitmap::new(vec![0; 17], 2, 2).is_none());
    }

    #[test]
    #[cfg(feature = "sprite")]
    fn solid_tile_is_exactly_the_requested_colour() {
        let c = Srgba::new(1.0, 0.0, 0.5, 1.0);
        let px = solid_rgba(&c, 3, 2);
        assert_eq!(px.len(), 3 * 2 * 4);
        for chunk in px.chunks_exact(4) {
            assert_eq!(chunk, &[255, 0, 128, 255]);
        }
    }

    #[test]
    #[cfg(feature = "sprite")]
    fn white_tint_is_recognised_so_the_multiply_is_skipped() {
        assert!(is_white(&Srgba::WHITE));
        assert!(!is_white(&Srgba::new(1.0, 1.0, 1.0, 0.5)));
        assert!(!is_white(&Srgba::new(0.5, 1.0, 1.0, 1.0)));
    }

    #[test]
    #[cfg(feature = "sprite")]
    fn tint_multiplies_every_channel_including_alpha() {
        let mut px = vec![200, 100, 50, 255];
        apply_tint(&mut px, &Srgba::new(0.5, 0.5, 0.5, 0.5));
        assert_eq!(px, vec![100, 50, 25, 128]);
    }

    #[test]
    #[cfg(feature = "sprite")]
    fn flip_x_mirrors_columns_not_rows() {
        // 2x1: red then green.
        let src = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let out = flip_rgba(&src, 2, 1, true, false);
        assert_eq!(&out[0..4], &[0, 255, 0, 255], "first pixel should be green");
        assert_eq!(&out[4..8], &[255, 0, 0, 255], "second pixel should be red");
        // Flipping the other axis on a single row changes nothing.
        assert_eq!(flip_rgba(&src, 2, 1, false, true), src);
    }

    #[test]
    #[cfg(feature = "sprite")]
    fn flip_both_axes_is_a_180_rotation() {
        // 2x2, distinct pixels in each corner.
        let src: Vec<u8> = vec![
            1, 1, 1, 255, 2, 2, 2, 255, //
            3, 3, 3, 255, 4, 4, 4, 255,
        ];
        let out = flip_rgba(&src, 2, 2, true, true);
        assert_eq!(&out[0..4], &[4, 4, 4, 255]);
        assert_eq!(&out[12..16], &[1, 1, 1, 255]);
    }

    #[test]
    #[cfg(feature = "text")]
    fn upscale_preserves_exact_pixel_values() {
        // Nearest-neighbour on the way up: no new colours may appear, or pixel
        // art turns to mush.
        let src = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let out = scale_rgba(&src, 2, 1, 4, 2);
        assert_eq!(out.len(), 4 * 2 * 4);
        for chunk in out.chunks_exact(4) {
            assert!(
                chunk == [255, 0, 0, 255] || chunk == [0, 0, 255, 255],
                "unexpected interpolated pixel {chunk:?}"
            );
        }
    }

    #[test]
    #[cfg(feature = "text")]
    fn downscale_averages_rather_than_dropping_pixels() {
        // 2x1 black+white averaged to 1x1 must be grey. Nearest would give one
        // of the two, silently discarding the hi-dpi detail we paid to rasterise.
        let src = vec![0, 0, 0, 255, 255, 255, 255, 255];
        let out = scale_rgba(&src, 2, 1, 1, 1);
        assert_eq!(out.len(), 4);
        assert!(
            (120..=135).contains(&out[0]),
            "expected a mid grey, got {}",
            out[0]
        );
    }

    #[test]
    #[cfg(feature = "text")]
    fn scaling_a_degenerate_source_returns_blank_not_a_panic() {
        let out = scale_rgba(&[], 0, 0, 4, 4);
        assert_eq!(out.len(), 4 * 4 * 4);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn disk_pixels_reports_a_missing_file_and_returns_none() {
        // The error path must not panic; a missing asset skips one sprite.
        let dir = std::env::temp_dir().join("bevy_kitty_no_such_dir_12345");
        let src = DiskPixels::at(&dir);
        assert_eq!(src.name(), "disk");
        assert!(load_file_rgba(&dir.join("nope.png"), None).is_none());
    }
}
