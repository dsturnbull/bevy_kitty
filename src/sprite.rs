//! The sprite pass: upload each distinct bitmap once, then emit cheap per-tick
//! placement deltas.
//!
//! This is the SSH-friendly renderer. A moved sprite costs tens of bytes, not a
//! whole framebuffer. In a measured 14 second run it did 158 uploads and **zero**
//! re-uploads, with about 10 KB/s of ongoing traffic covering all animation,
//! movement and text.
//!
//! # Where the data is intercepted
//!
//! Worth being explicit, because the obvious guess is wrong: this does **not**
//! intercept Bevy's render pipeline. There is no custom `RenderGraph` node, no
//! extraction into the render world, no wgpu hook. It is an ordinary system doing
//! an ordinary ECS query, running beside the real renderer rather than downstream
//! of it, reading the same components Bevy's own renderer reads.
//!
//! Three things make that work: it runs in `PostUpdate` so transforms have
//! settled; pixels come from outside the ECS (see [`crate::pixels`]); and the
//! camera is borrowed purely as a coordinate oracle, via
//! `Camera::world_to_viewport`.

use std::collections::HashMap;

use bevy::image::{TextureAtlas, TextureAtlasLayout};
use bevy::prelude::*;
use bevy::sprite::{Anchor, Sprite};

use crate::pixels::{apply_tint, flip_rgba, is_white, solid_rgba, Bitmap, PixelRequest};
use crate::term::{FitBox, TermSize};
use crate::{proto, write_stdout, KittyCamera, KittyConfig, KittySet};

/// Multiplier turning a `Transform.translation.z` into a kitty `z` index.
///
/// Y-sorted 2D games often use a very narrow z band (one real case was only
/// `[1.0, 1.018]` wide), so a small factor collapses every actor onto one z and
/// front-to-back ordering is lost entirely. 100_000 spreads such a band over
/// ~1800 distinct values while staying far inside `i32`.
pub const Z_SPREAD: f32 = 100_000.0;

/// Bias added to text's kitty `z`, so a string is never hidden behind the sprite
/// it labels.
///
/// Sized to clear the y-sort band (world z around 1.0, i.e. `1.0 * Z_SPREAD`) by a
/// factor of ten. It does **not** clear everything: a sprite deliberately placed at
/// a high world z still draws over text, and that is correct. A full-screen colour
/// grade at world z 100 maps to `10_000_000` and tints the text along with the
/// scene, exactly as it would in a real window.
///
/// So the guarantee is narrow and worth stating precisely: text beats **world-space
/// sprites**, not **screen-space overlays**. An app that wants text above its own
/// overlays must give that text a higher world z, the same as it would on the GPU.
pub const TEXT_Z_BIAS: i32 = 1_000_000;

/// First kitty image id the sprite pass allocates. Below this is reserved:
/// frame mode uses id 1.
const SPRITE_IMG_ID_BASE: u32 = 1000;

/// The pixel size a flat-colour tile is synthesised at.
///
/// One pixel. That is not a compromise, it is the correct answer: a placement is
/// stretched to its cell span by kitty, and every pixel of a flat colour is the
/// same colour, so a 1x1 source scaled up is bit-identical to a full-size one.
///
/// It matters because the naive alternative is quadratic in the sprite's size, and
/// games routinely size full-screen effect overlays well beyond the virtual
/// resolution so camera shake cannot reveal an edge. One real case used `RES * 10`,
/// which is 3200x1800: a 23 MB buffer to allocate and base64-encode, per quantised
/// colour change, to say "slightly blue".
///
/// The other win is cache shape. Keyed at a fixed size, the cache holds one image
/// per COLOUR rather than one per (colour, size) pair, so a hundred differently
/// sized panels sharing a palette collapse to a handful of uploads.
const SOLID_TILE_PX: u32 = 1;

/// The placement geometry last emitted for an entity. Re-place only when this
/// changes, so a still sprite costs zero bytes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Geom {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) xoff: u32,
    pub(crate) yoff: u32,
    pub(crate) z: i32,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
}

/// A bitmap uploaded to the terminal, and the id it went up under.
struct CachedBitmap {
    w: u32,
    h: u32,
    /// The kitty image id these pixels were uploaded under. Assigned and
    /// transmitted exactly ONCE; every entity showing this bitmap just places
    /// this id.
    img_id: u32,
}

/// Per-entity render state.
///
/// The invariant that makes this cheap, and that broke twice before it was
/// understood: each entity owns ONE placement id for its whole life, and each
/// distinct bitmap owns ONE image id, uploaded once ever. A frame change
/// re-points the placement at a different image; a move re-places the same pair.
/// Two different image ids must never share a placement id, or kitty *stacks*
/// them instead of replacing, which looks like ghosting.
struct EntityRender {
    placement_id: u32,
    /// The SHARED bitmap image id this placement currently shows (0 = none).
    cur_img_id: u32,
    last_geom: Option<Geom>,
}

/// Persistent sprite-pass state across ticks.
#[derive(Resource, Default)]
pub struct SpriteScene {
    /// Uploaded bitmaps, keyed by everything baked into their pixels.
    bitmaps: HashMap<String, CachedBitmap>,
    /// ECS entity -> its placement id and last-drawn state.
    entities: HashMap<Entity, EntityRender>,
    next_img_id: u32,
    next_placement_id: u32,
    term: Option<TermSize>,
    tick: u64,
}

impl SpriteScene {
    fn alloc_img_id(&mut self) -> u32 {
        self.next_img_id = self.next_img_id.max(SPRITE_IMG_ID_BASE) + 1;
        self.next_img_id
    }

    fn alloc_placement_id(&mut self) -> u32 {
        self.next_placement_id += 1;
        self.next_placement_id
    }

    /// How many distinct bitmaps are uploaded. Exposed for tests and diagnostics.
    pub fn bitmap_count(&self) -> usize {
        self.bitmaps.len()
    }

    /// How many entities currently hold a placement.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Does every placement id map to exactly one image id?
    ///
    /// This is the anti-ghosting invariant, exposed so it can be asserted rather
    /// than assumed. A violation is not a crash, it is a visual artefact, which is
    /// exactly the kind of bug worth a test.
    pub fn placement_ids_are_unique(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        self.entities
            .values()
            .all(|er| seen.insert(er.placement_id))
    }
}

pub(crate) fn build(app: &mut App) {
    app.init_resource::<SpriteScene>();
    app.add_systems(PostUpdate, render_sprites.in_set(KittySet::Render));
}

/// Quantise a tint into coarse buckets before using it as a cache key.
///
/// Fading effects sweep alpha continuously. At full precision every tick produced
/// a NEW cache key and therefore a NEW pixel upload, measured at ~478 redundant
/// uploads and ~2 MB in a 4 second window, which defeats the whole point of
/// placement-based rendering. 16 buckets per channel is visually
/// indistinguishable at terminal scale.
fn quantise_tint(c: bevy::color::Srgba) -> (bevy::color::Srgba, String) {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 15.0).round() as u8;
    let (qr, qg, qb, qa) = (q(c.red), q(c.green), q(c.blue), q(c.alpha));
    // Use the QUANTISED colour for the pixels too, so a cached bitmap can never
    // disagree with the key that named it.
    let tint = bevy::color::Srgba::new(
        qr as f32 / 15.0,
        qg as f32 / 15.0,
        qb as f32 / 15.0,
        qa as f32 / 15.0,
    );
    let key = if is_white(&tint) {
        String::new()
    } else {
        format!("@{qr:x}{qg:x}{qb:x}{qa:x}")
    };
    (tint, key)
}

/// A readable name for an image asset, for cache keys and logs.
///
/// Prefers the asset's own path, because a key you cannot read is a key you cannot
/// debug: `artie_sheet.png#1@db8f` tells you which sheet, which animation frame and
/// which tint at a glance, where the raw `AssetId` debug form
/// (`AssetId<...>{ index: 68, generation: 0 }`) tells you nothing and is long enough
/// to bury the rest of the key.
///
/// Falls back to the id for generated images, which genuinely have no path. Only the
/// file name is used, not the full path: the directory is the same for every sprite
/// in a game and just makes the log wider.
pub(crate) fn image_label(assets: &AssetServer, id: bevy::asset::AssetId<Image>) -> String {
    match assets.get_path(id) {
        Some(p) => p
            .path()
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.path().display().to_string()),
        // Generated at runtime, so the id IS its only identity. Keep it short:
        // the index alone is unique within a run.
        None => format!("gen:{}", id_index(id)),
    }
}

/// The distinguishing part of an `AssetId`, short enough to read in a log.
///
/// `AssetId`'s Debug form is 60-odd characters of type name wrapped around the few
/// that matter. It comes in two shapes, and both have to be handled:
///
/// ```text
/// AssetId<bevy_image::image::Image>{ index: 68, generation: 0}
/// AssetId<bevy_image::image::Image>{uuid: 97128bb1-2588-480b-bdc6-87b4adbec477}
/// ```
///
/// Runtime-loaded assets get the first (an index), compile-time constants the second
/// (a uuid). Handling only the index form leaves the uuid case emitting all 77
/// characters into every cache key.
fn id_index(id: bevy::asset::AssetId<Image>) -> String {
    // No public accessor for either field, so parse the Debug form. A formatting
    // change upstream degrades this to a truncated string: ugly, but still unique
    // and still correct.
    let s = format!("{id:?}");
    let after = |marker: &str| {
        s.split(marker).nth(1).map(|rest| {
            rest.trim()
                .trim_end_matches('}')
                .split(&[',', ' ', '}'][..])
                .next()
                .unwrap_or(rest)
                .to_string()
        })
    };
    if let Some(index) = after("index: ") {
        return index;
    }
    if let Some(uuid) = after("uuid: ") {
        // First group of the uuid is plenty to tell two assets apart in a log.
        return uuid.split('-').next().unwrap_or(&uuid).to_string();
    }
    // Unrecognised shape: keep it bounded rather than pasting a type name into
    // every key.
    s.chars()
        .rev()
        .take(12)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// The DRAWN size of a solid-colour sprite, in virtual pixels.
///
/// This is what the placement's cell span is computed from, and it is the sprite's
/// real `custom_size`. The uploaded PIXELS are [`SOLID_TILE_PX`] square regardless;
/// see that constant for why the two differ.
fn solid_draw_size(custom_size: Vec2) -> Vec2 {
    Vec2::new(
        custom_size.x.abs().round().max(1.0),
        custom_size.y.abs().round().max(1.0),
    )
}

/// The per-tick render system. Query every visible sprite, ensure its bitmap is
/// uploaded, then emit a placement delta only when it moved or changed.
///
/// # Known limitation: no `RenderLayers` filter
///
/// The query takes every visible `Sprite` in the world, without consulting
/// `RenderLayers`. So a sprite on a layer the camera does not watch is still drawn.
/// For an app with one camera and one layer, which is the case this was built for,
/// that is equivalent. For an app using layers to hide things from a camera, it is
/// wrong, and the fix is a real one: read the camera's own `RenderLayers` and
/// intersect.
///
/// It matters more than it sounds, because full-screen effect overlays are commonly
/// parked on a separate layer AND sized far beyond the virtual resolution. Those are
/// handled two ways here: the fully-transparent early-out skips them while inactive,
/// and [`SOLID_TILE_PX`] keeps them cheap when they are active.
#[allow(clippy::type_complexity)]
pub fn render_sprites(
    mut scene: ResMut<SpriteScene>,
    config: Res<KittyConfig>,
    assets: Res<AssetServer>,
    atlas_layouts: Res<Assets<TextureAtlasLayout>>,
    images: Res<Assets<Image>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<KittyCamera>>,
    sprites: Query<
        (
            Entity,
            &Sprite,
            &GlobalTransform,
            &ViewVisibility,
            Option<&Anchor>,
        ),
        Without<KittyCamera>,
    >,
) {
    scene.tick += 1;

    // Terminal size: query once, then re-query occasionally in case of a resize
    // (a cheap ioctl, but no need every frame).
    if scene.term.is_none() || scene.tick.is_multiple_of(120) {
        scene.term = Some(TermSize::query(config.terminal_size));
    }
    let term = scene.term.unwrap();
    let fit = FitBox::compute(&term, config.virtual_size);

    let Ok((camera, cam_xf)) = camera_q.single() else {
        if scene.tick <= 3 {
            warn!(
                "[kitty] no KittyCamera found (tick {}), so nothing can be placed. \
                 Put the KittyCamera marker on your Camera2d.",
                scene.tick
            );
        }
        return;
    };

    let mut buf: Vec<u8> = Vec::new();
    let mut seen: Vec<Entity> = Vec::new();
    let mut emitted = 0u32;
    let mut uploaded = 0u32;

    for (entity, sprite, xf, vis, anchor) in sprites.iter() {
        if !vis.get() {
            continue;
        }
        // The tint and flip are baked into the uploaded pixels (kitty has no
        // per-placement tint or mirror), so they MUST be part of the cache key or
        // a flipped/tinted variant would silently reuse the wrong bitmap, and
        // never re-transmit when the flip changes.
        let raw_tint = sprite.color.to_srgba();
        // A fully transparent sprite draws nothing; uploading it would burn an
        // image id and bandwidth for an invisible tile. Fade effects pass through
        // alpha 0 regularly.
        if raw_tint.alpha <= 0.001 {
            continue;
        }
        let (tint, tint_key) = quantise_tint(raw_tint);
        if tint.alpha <= 0.001 {
            continue;
        }
        let flip_key = match (sprite.flip_x, sprite.flip_y) {
            (false, false) => "",
            (true, false) => "|fx",
            (false, true) => "|fy",
            (true, true) => "|fxy",
        };

        // Atlas cell rect, if this sprite draws one cell of a sheet.
        let (cell_rect, cell_suffix) = match &sprite.texture_atlas {
            Some(TextureAtlas { layout, index }) => {
                let Some(layout) = atlas_layouts.get(layout) else {
                    continue; // layout not loaded yet
                };
                let Some(rect) = layout.textures.get(*index) else {
                    continue;
                };
                (Some(*rect), format!("#{index}"))
            }
            None => (None, String::new()),
        };

        // Two kinds of sprite: one backed by a real image (from disk, or generated
        // at runtime), and one with no image at all whose `color` and `custom_size`
        // describe a solid tile (UI panels, buttons, flat-colour overlays).
        //
        // The trap: a colour-only sprite does NOT have a dangling handle. Bevy's
        // `ImagePlugin` inserts a 1x1 white `Image` at `Handle::default()`
        // (bevy_image 0.19 image.rs:222), CPU data and all. So an "is it in
        // Assets<Image>?" test says YES for every panel, and the pass would draw a
        // single white pixel scaled to the panel's span instead of the panel's
        // colour. The default handle has to be excluded explicitly.
        let is_default_handle = sprite.image.id() == Handle::<Image>::default().id();
        let has_image = !is_default_handle
            && (assets.get_path(sprite.image.id()).is_some()
                || images.get(&sprite.image).is_some_and(|i| i.data.is_some()));

        // For a solid-colour sprite, the size it DRAWS at. The pixels it uploads
        // are a fixed 1x1 tile (see SOLID_TILE_PX), so this is kept separate: the
        // draw size feeds the placement's cell span, not the bitmap.
        let solid_draw = if has_image {
            None
        } else {
            let Some(size) = sprite.custom_size else {
                continue; // nothing to draw and no size to fill
            };
            Some(solid_draw_size(size))
        };

        // The cache key. For a solid tile it deliberately does NOT include the
        // size, because the pixels do not depend on it: every panel of a given
        // colour shares one upload, whatever its dimensions.
        let cache_key = if solid_draw.is_some() {
            format!("solid{tint_key}")
        } else {
            format!(
                "{}{}{}{}",
                image_label(&assets, sprite.image.id()),
                cell_suffix,
                tint_key,
                flip_key
            )
        };

        // Resolve pixels once, cached across ticks AND across entities sharing the
        // same key.
        if !scene.bitmaps.contains_key(&cache_key) {
            let resolved: Option<Bitmap> = if solid_draw.is_some() {
                let n = SOLID_TILE_PX;
                Bitmap::new(solid_rgba(&tint, n, n), n, n)
            } else {
                let req = PixelRequest {
                    image: sprite.image.id(),
                    atlas_cell: cell_rect,
                    images: &images,
                    assets: &assets,
                };
                config.pixel_source.pixels(&req)
            };
            let Some(Bitmap { mut rgba, w, h }) = resolved else {
                // Normal while an asset loads; the renderer asks again next frame.
                continue;
            };
            // Bake the tint. A synthesised solid tile is already the right colour.
            if solid_draw.is_none() && !is_white(&tint) {
                apply_tint(&mut rgba, &tint);
            }
            if sprite.flip_x || sprite.flip_y {
                rgba = flip_rgba(&rgba, w, h, sprite.flip_x, sprite.flip_y);
            }
            // Upload ONCE, here, under this bitmap's own id. Every entity that
            // ever shows these pixels just places that id.
            let img_id = scene.alloc_img_id();
            proto::transmit_rgba(&mut buf, img_id, w, h, &rgba);
            uploaded += 1;
            debug!("[kitty] uploaded bitmap '{cache_key}' ({w}x{h}) as img {img_id}");
            scene
                .bitmaps
                .insert(cache_key.clone(), CachedBitmap { w, h, img_id });
        }
        let (bw, bh, img_id) = {
            let b = &scene.bitmaps[&cache_key];
            (b.w, b.h, b.img_id)
        };

        // Ensure this ENTITY owns a placement id, held for its whole life. It does
        // NOT own pixels: the bitmap does.
        if !scene.entities.contains_key(&entity) {
            let placement_id = scene.alloc_placement_id();
            scene.entities.insert(
                entity,
                EntityRender {
                    placement_id,
                    cur_img_id: 0,
                    last_geom: None,
                },
            );
        }
        let (placement_id, prev_img_id, img_changed) = {
            let er = &scene.entities[&entity];
            (er.placement_id, er.cur_img_id, er.cur_img_id != img_id)
        };

        // World -> viewport pixels. The render target is the offscreen image, so
        // this returns coords in `virtual_size` space.
        let world_pos = xf.translation();
        let Ok(vp) = camera.world_to_viewport(cam_xf, world_pos) else {
            continue; // behind the camera or off-screen
        };
        // Apply the entity's world SCALE: depth-scaling shrinks distant actors,
        // squash-and-stretch varies size, particles grow. Without this everything
        // draws at full size regardless.
        let scale = xf.scale().truncate();
        // The size to DRAW at, which for a solid tile is emphatically NOT the
        // bitmap's size: that is a 1x1 pixel standing in for the whole panel, so
        // falling back to it would collapse every panel to a single cell.
        let base_size = match solid_draw {
            Some(size) => size,
            None => sprite
                .custom_size
                .unwrap_or(Vec2::new(bw as f32, bh as f32)),
        };
        let draw_size = base_size * Vec2::new(scale.x.abs(), scale.y.abs());
        // Convert sprite CENTRE -> TOP-LEFT in viewport space, accounting for the
        // anchor (Bevy's default is Center; anchor is a fraction of size; +y is up
        // in world but viewport y grows downward).
        let a = anchor.map(|x| x.as_vec()).unwrap_or(Vec2::ZERO);
        let tl_vx = vp.x - draw_size.x * 0.5 - a.x * draw_size.x;
        let tl_vy = vp.y - draw_size.y * 0.5 + a.y * draw_size.y;

        let (row, col, xoff, yoff) = fit.map(tl_vx, tl_vy);
        // Sprites keep cell-based sizing. Cells are not square so this rounds the
        // aspect slightly (~3% on a 64px sprite, not visible), and unlike glyphs we
        // CANNOT pre-scale to exact pixels: depth-scaling varies a sprite's size
        // continuously, so every size would produce a new cached image. Glyphs are
        // the opposite case (tiny, so the same rounding is a ~20% distortion, but
        // their sizes are discrete), which is why the text pass pre-scales.
        let (cols, rows) = fit.span_cells(
            draw_size.x.round().max(1.0) as u32,
            draw_size.y.round().max(1.0) as u32,
        );
        let z = (world_pos.z * Z_SPREAD) as i32;
        let geom = Geom {
            row,
            col,
            xoff,
            yoff,
            z,
            cols,
            rows,
        };

        seen.push(entity);

        // Showing a different bitmap than last tick? A placement belongs to one
        // image id, so drop the old one first or the previous frame stays on
        // screen underneath. This is the ghosting bug, and this line is the fix.
        if img_changed && prev_img_id != 0 {
            proto::delete_placement(&mut buf, prev_img_id, placement_id);
        }
        // Re-place when the bitmap or the geometry changed. Same img_id +
        // placement_id means in-place and flicker-free.
        let geom_changed = scene.entities[&entity].last_geom != Some(geom);
        if img_changed || geom_changed {
            proto::cursor_to(&mut buf, row, col);
            proto::place_scaled(&mut buf, img_id, placement_id, z, cols, rows, xoff, yoff);
            emitted += 1;
        }
        if let Some(er) = scene.entities.get_mut(&entity) {
            er.cur_img_id = img_id;
            er.last_geom = Some(geom);
        }
    }

    // Retire placements for entities that vanished (despawned or off-screen). Only
    // the PLACEMENT goes: the shared bitmap stays uploaded, since another entity
    // (or this one respawning) will likely show it again.
    let gone: Vec<Entity> = scene
        .entities
        .keys()
        .copied()
        .filter(|e| !seen.contains(e))
        .collect();
    for e in gone {
        if let Some(er) = scene.entities.remove(&e) {
            if er.cur_img_id != 0 {
                proto::delete_placement(&mut buf, er.cur_img_id, er.placement_id);
            }
        }
    }

    let bytes = buf.len();
    if !buf.is_empty() && !write_stdout(&buf, "sprite") {
        return;
    }
    if scene.tick <= 3 || scene.tick.is_multiple_of(120) {
        info!(
            "[kitty] sprite tick #{}: {} (re)placements, {} new uploads, {} escape bytes, \
             {} bitmaps cached, {} live entities",
            scene.tick,
            emitted,
            uploaded,
            bytes,
            scene.bitmaps.len(),
            scene.entities.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ids_start_above_frame_modes_reserved_id() {
        // Frame mode owns id 1. Colliding with it would make a sprite overwrite
        // the whole-screen plate, which looks like the screen going blank.
        let mut scene = SpriteScene::default();
        let first = scene.alloc_img_id();
        assert!(
            first > 1,
            "first sprite img id {first} must clear frame mode's id 1"
        );
        assert_eq!(first, SPRITE_IMG_ID_BASE + 1);
        assert_eq!(scene.alloc_img_id(), SPRITE_IMG_ID_BASE + 2);
    }

    #[test]
    fn placement_ids_are_never_reused() {
        let mut scene = SpriteScene::default();
        let ids: Vec<u32> = (0..100).map(|_| scene.alloc_placement_id()).collect();
        let unique: std::collections::HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "placement ids must be unique");
    }

    #[test]
    fn a_fade_collapses_to_a_handful_of_cache_keys() {
        // The measured bug this quantisation fixes: at full precision a fade minted
        // a new key (and a new upload) every tick.
        use bevy::color::Srgba;
        let keys: std::collections::HashSet<String> = (0..=100)
            .map(|i| {
                let a = i as f32 / 100.0;
                quantise_tint(Srgba::new(1.0, 1.0, 1.0, a)).1
            })
            .collect();
        assert!(
            keys.len() <= 17,
            "101 fade steps should collapse to at most 17 keys, got {}",
            keys.len()
        );
    }

    #[test]
    fn quantised_colour_matches_the_key_that_names_it() {
        // If the pixels were tinted with the raw colour but keyed by the quantised
        // one, two visibly different bitmaps would share a cache entry.
        use bevy::color::Srgba;
        let (tint_a, key_a) = quantise_tint(Srgba::new(0.51, 0.2, 0.9, 1.0));
        let (tint_b, key_b) = quantise_tint(Srgba::new(0.515, 0.2, 0.9, 1.0));
        assert_eq!(key_a, key_b, "near-identical colours should share a key");
        assert_eq!(tint_a, tint_b, "and therefore share exact pixels");
    }

    #[test]
    fn a_generated_image_key_is_short_not_a_type_name() {
        // AssetId's Debug form is ~60-77 characters of type name, which buried the
        // informative part of the key in the logs. Both of its shapes (index for a
        // runtime asset, uuid for a compile-time constant) must come out short.
        let label = id_index(bevy::asset::AssetId::<Image>::default());
        assert!(
            label.len() <= 12,
            "expected a short id, got {} chars: {label}",
            label.len()
        );
        assert!(
            !label.contains("bevy_image"),
            "the type name leaked into the key: {label}"
        );
    }

    #[test]
    fn white_tint_produces_an_empty_key_suffix() {
        // So an untinted sprite shares one cache entry rather than one per colour.
        use bevy::color::Srgba;
        assert_eq!(quantise_tint(Srgba::WHITE).1, "");
        assert_ne!(quantise_tint(Srgba::new(1.0, 0.0, 0.0, 1.0)).1, "");
    }

    #[test]
    fn a_solid_tile_costs_one_pixel_however_big_it_draws() {
        // The measured trap: full-screen effect overlays get sized well beyond the
        // virtual resolution so camera shake cannot reveal an edge. One real case
        // used 3200x1800, which as a pixel-for-pixel RGBA tile is a 23 MB
        // allocation and base64 encode, per quantised colour change. Kitty
        // stretches the placement, so a flat colour needs exactly one pixel.
        assert_eq!(SOLID_TILE_PX, 1);
        let bytes = (SOLID_TILE_PX as usize).pow(2) * 4;
        assert_eq!(bytes, 4, "a solid upload should be 4 bytes, got {bytes}");
        // The DRAWN size is still the sprite's real size, so the placement spans
        // the right number of cells.
        assert_eq!(
            solid_draw_size(Vec2::new(3200.0, 1800.0)),
            Vec2::new(3200.0, 1800.0)
        );
    }

    #[test]
    fn solid_tiles_are_cached_by_colour_alone_not_by_size() {
        // Because the pixels do not depend on the size, differently sized panels
        // sharing a colour must share ONE upload. Keying by size would give a
        // hundred-panel UI a hundred identical 4-byte images.
        use bevy::color::Srgba;
        let key_of = |c: Srgba| format!("solid{}", quantise_tint(c).1);
        let red = Srgba::new(1.0, 0.0, 0.0, 1.0);
        assert_eq!(key_of(red), key_of(red));
        assert_ne!(key_of(red), key_of(Srgba::new(0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn a_negative_or_zero_custom_size_still_draws_at_least_one_pixel() {
        // A zero-area sprite would ask kitty for a zero-cell span, and a negative
        // size is a legitimate way to express a flip.
        assert_eq!(solid_draw_size(Vec2::ZERO), Vec2::ONE);
        assert_eq!(
            solid_draw_size(Vec2::new(-64.0, -32.0)),
            Vec2::new(64.0, 32.0)
        );
    }

    #[test]
    fn z_spread_keeps_a_narrow_band_distinguishable() {
        // The real failure: a `* 10.0` factor truncated a [1.0, 1.018] y-sort band
        // to a single z, losing front-to-back ordering entirely.
        let near = (1.0_f32 * Z_SPREAD) as i32;
        let far = (1.018_f32 * Z_SPREAD) as i32;
        assert!(far - near > 1000, "band spans only {} z values", far - near);
        // And text still clears the whole world band.
        assert!(
            TEXT_Z_BIAS > far,
            "text bias {TEXT_Z_BIAS} must clear world max {far}"
        );
    }
}
