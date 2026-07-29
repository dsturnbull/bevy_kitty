//! The text pass: render every `Text2d` as reusable glyph images.
//!
//! Same idea as the sprite pass, one letter at a time, and it reuses what Bevy has
//! ALREADY computed rather than rasterising anything itself. `TextLayoutInfo`
//! gives per-glyph positions, and Bevy's font atlas keeps its pixels CPU-side
//! (unlike sprite images), so each glyph can be cropped, tinted, uploaded ONCE and
//! then placed as many times as it appears.
//!
//! The reuse is the whole win: one measured frame drew **167 glyphs on screen from
//! 69 uploaded images**, and the ratio keeps improving as the cache warms.
//!
//! # Two traps live in this file
//!
//! **`ViewVisibility` cannot be used here.** It is set by frustum culling in the
//! render pipeline. `Text2d`'s `Aabb` is computed after layout, and headless
//! culling never marked it visible, so every string silently vanished. The query
//! below reads [`InheritedVisibility`], the propagated authored intent, which is
//! what an app's own UI code toggles. The sprite pass keeps `ViewVisibility`
//! because for sprites it genuinely works; do not "fix" that asymmetry.
//!
//! **The system must run after `update_text2d_layout`.** That is what fills
//! `TextLayoutInfo.glyphs`. Query before it and you get an empty vector and no
//! text, with no error.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::{ComputedTextBlock, TextBounds, TextColor, TextLayoutInfo};

use crate::pixels::{apply_tint, crop_image_rgba, is_white, scale_rgba};
use crate::sprite::{Geom, TEXT_Z_BIAS, Z_SPREAD};
use crate::term::{FitBox, TermSize};
use crate::{proto, write_stdout, KittyCamera, KittyConfig, KittySet};

/// First kitty image id the glyph pass allocates.
///
/// Its own band, so glyph ids can never collide with frame mode's id 1 or the
/// sprite pass's allocator. A collision would silently replace a sprite's pixels
/// with a letter.
const GLYPH_ID_BASE: u32 = 100_000;

/// A glyph bitmap uploaded to the terminal exactly ONCE, ever.
struct GlyphImage {
    img_id: u32,
}

/// Per-glyph-instance render state.
///
/// A slot NEVER owns pixels: it owns only a placement id, so changing text
/// re-points the placement at a different (already-uploaded) glyph image instead
/// of re-uploading.
struct GlyphSlot {
    placement_id: u32,
    /// The glyph image id this placement currently shows (0 = none yet).
    cur_img_id: u32,
    last_geom: Option<Geom>,
}

/// Persistent state for the text pass.
#[derive(Resource, Default)]
pub struct GlyphScene {
    /// Uploaded glyph images, keyed by (atlas texture, rect, tint, target size).
    glyphs: HashMap<String, GlyphImage>,
    /// One slot per on-screen glyph instance, keyed (text entity, glyph index).
    slots: HashMap<(Entity, usize), GlyphSlot>,
    next_img_id: u32,
    next_placement_id: u32,
    tick: u64,
    term: Option<TermSize>,
}

impl GlyphScene {
    fn alloc_img_id(&mut self) -> u32 {
        self.next_img_id = self.next_img_id.max(GLYPH_ID_BASE) + 1;
        self.next_img_id
    }

    fn alloc_placement_id(&mut self) -> u32 {
        self.next_placement_id = self.next_placement_id.max(GLYPH_ID_BASE) + 1;
        self.next_placement_id
    }

    /// How many distinct glyph images are uploaded.
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }

    /// How many glyph instances are currently placed.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

pub(crate) fn build(app: &mut App) {
    app.init_resource::<GlyphScene>();
    // `.after(update_text2d_layout)` is load-bearing: that system fills
    // TextLayoutInfo.glyphs for this frame. When the `ui` feature is on we also wait
    // for `bevy_ui`'s own text system, which fills the same component for UI nodes.
    // Both kinds of text share ONE pass and ONE glyph cache, so an "A" in a UI label
    // and an "A" in a world-space label at the same size and colour are the same
    // upload.
    let system = render_glyphs
        .in_set(KittySet::Render)
        .after(bevy::sprite::update_text2d_layout);
    #[cfg(feature = "ui")]
    let system = system.after(bevy::ui::widget::text_system);
    app.add_systems(PostUpdate, system);
}

/// Draw one glyph, given its top-left already resolved to viewport pixels.
///
/// Everything here is independent of WHERE the text lives, which is the whole reason
/// `Text2d` and `bevy_ui` text can share a pass. The two callers differ only in how
/// they compute that top-left: world space needs the camera and a Y flip, UI space is
/// already screen space. The cropping, tinting, pre-scaling, caching and placement
/// bookkeeping below are identical, and a second copy would be a second place for the
/// one-image-id-per-placement-id invariant to rot.
///
/// Returns true if a placement was emitted.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    scene: &mut GlyphScene,
    buf: &mut Vec<u8>,
    images: &Assets<Image>,
    fit: &FitBox,
    glyph: &bevy::text::PositionedGlyph,
    colour: &bevy::color::Srgba,
    inv_sf: f32,
    top_left: Vec2,
    z: i32,
    slot_key: (Entity, usize),
    uploaded: &mut u32,
) -> bool {
    let rect = glyph.atlas_info.rect;
    let gw = rect.width().round().max(1.0) as u32;
    let gh = rect.height().round().max(1.0) as u32;

    // Target size in TERMINAL pixels. The glyph is pre-scaled to this exactly and
    // placed 1:1 (no c/r), because sizing via whole cells distorts small glyphs by
    // ~20% on one axis. The size is part of the cache key, so a terminal resize just
    // builds new glyph images.
    let tw = (gw as f32 * inv_sf * fit.scale).round().max(1.0) as u32;
    let th = (gh as f32 * inv_sf * fit.scale).round().max(1.0) as u32;

    let key = glyph_key(
        &format!("{:?}", glyph.atlas_info.texture),
        rect.min.x,
        rect.min.y,
        gw,
        gh,
        colour,
        tw,
        th,
    );

    // Crop, tint and UPLOAD this glyph exactly once, ever. Every later occurrence in
    // the same colour and size reuses this image id, so text costs placements rather
    // than pixel uploads.
    if !scene.glyphs.contains_key(&key) {
        let Some(atlas) = images.get(glyph.atlas_info.texture) else {
            return false; // atlas not available yet
        };
        let x0 = rect.min.x.round().max(0.0) as u32;
        let y0 = rect.min.y.round().max(0.0) as u32;
        let Some(bitmap) = crop_image_rgba(atlas, Some(URect::new(x0, y0, x0 + gw, y0 + gh)))
        else {
            return false;
        };
        let mut rgba = bitmap.rgba;
        // Mask glyphs are white-RGB plus a coverage alpha, meant to be tinted by
        // TextColor. Colour glyphs (emoji) keep their own colours, but still honour a
        // non-white tint.
        if glyph.atlas_info.is_alpha_mask || !is_white(colour) {
            apply_tint(&mut rgba, colour);
        }
        // Pre-scale to the exact target pixels so the placement needs no cell-based
        // scaling (which would distort the aspect).
        let (rgba, w, h) = if (bitmap.w, bitmap.h) == (tw, th) {
            (rgba, bitmap.w, bitmap.h)
        } else {
            (scale_rgba(&rgba, bitmap.w, bitmap.h, tw, th), tw, th)
        };
        let img_id = scene.alloc_img_id();
        proto::transmit_rgba(buf, img_id, w, h, &rgba);
        *uploaded += 1;
        scene.glyphs.insert(key.clone(), GlyphImage { img_id });
    }
    let glyph_img_id = scene.glyphs[&key].img_id;

    let (row, col, xoff, yoff) = fit.map(top_left.x, top_left.y);
    // cols/rows = 0 means omitted from the placement, so kitty draws the image at its
    // natural (already exactly-scaled) pixel size. This is what keeps glyph aspect
    // correct.
    let geom = Geom {
        row,
        col,
        xoff,
        yoff,
        z,
        cols: 0,
        rows: 0,
    };

    if !scene.slots.contains_key(&slot_key) {
        let placement_id = scene.alloc_placement_id();
        scene.slots.insert(
            slot_key,
            GlyphSlot {
                placement_id,
                cur_img_id: 0,
                last_geom: None,
            },
        );
    }
    let (placement_id, prev_img_id, img_changed, geom_changed) = {
        let s = &scene.slots[&slot_key];
        (
            s.placement_id,
            s.cur_img_id,
            s.cur_img_id != glyph_img_id,
            s.last_geom != Some(geom),
        )
    };

    // No re-transmit here: the glyph image is already uploaded and shared. If this
    // slot now shows a DIFFERENT glyph, drop its old placement first (a placement
    // belongs to one image id; silently re-pointing it would leave the old letter on
    // screen), then place the new image under the same placement id.
    if img_changed && prev_img_id != 0 {
        proto::delete_placement(buf, prev_img_id, placement_id);
    }
    let emitted = img_changed || geom_changed;
    if emitted {
        proto::cursor_to(buf, row, col);
        proto::place_scaled(buf, glyph_img_id, placement_id, z, 0, 0, xoff, yoff);
    }
    if let Some(s) = scene.slots.get_mut(&slot_key) {
        s.cur_img_id = glyph_img_id;
        s.last_geom = Some(geom);
    }
    emitted
}

/// The per-section colour for a glyph, quantised.
///
/// Quantised for the same reason as sprites: text colours are animated (fades, a
/// blinking cursor, tier colours), and at full precision each step would produce a new
/// glyph image and a new upload.
fn section_colour(
    block: &ComputedTextBlock,
    section_index: usize,
    text_colors: &Query<&TextColor>,
) -> bevy::color::Srgba {
    let raw = block
        .entities()
        .get(section_index)
        .map(|te| te.entity)
        .and_then(|e| text_colors.get(e).ok())
        .map(|tc| tc.0.to_srgba())
        .unwrap_or(bevy::color::Srgba::WHITE);
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 15.0).round()) / 15.0;
    bevy::color::Srgba::new(q(raw.red), q(raw.green), q(raw.blue), q(raw.alpha))
}

/// Render all `Text2d` as reusable glyph images.
///
/// The per-glyph transform mirrors `bevy_sprite_render`'s `extract_text2d_sprite`
/// exactly, including the Y flip and the `1/scale_factor`, so terminal text lands
/// where the real renderer would put it.
#[allow(clippy::type_complexity)]
pub fn render_glyphs(
    mut scene: ResMut<GlyphScene>,
    config: Res<KittyConfig>,
    images: Res<Assets<Image>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<KittyCamera>>,
    texts: Query<(
        Entity,
        &TextLayoutInfo,
        &ComputedTextBlock,
        &TextBounds,
        &Anchor,
        &GlobalTransform,
        // NOT ViewVisibility. See the module docs: culling never marks Text2d
        // visible headless, so every string would silently vanish.
        &InheritedVisibility,
    )>,
    // `bevy_ui` text. A UI text node has no `TextBounds` and no `Anchor`, so it
    // cannot match the query above; it needs its own. Both feed one glyph cache.
    #[cfg(feature = "ui")] ui_texts: Query<(
        Entity,
        &TextLayoutInfo,
        &ComputedTextBlock,
        &bevy::ui::ComputedNode,
        &bevy::ui::UiGlobalTransform,
        &InheritedVisibility,
        Option<&bevy::ui::CalculatedClip>,
    )>,
    text_colors: Query<&TextColor>,
) {
    scene.tick += 1;
    if scene.term.is_none() || scene.tick.is_multiple_of(120) {
        scene.term = Some(TermSize::query(config.terminal_size));
    }
    let fit = FitBox::compute(&scene.term.unwrap(), config.virtual_size);

    let Ok((camera, cam_xf)) = camera_q.single() else {
        return;
    };

    let mut buf: Vec<u8> = Vec::new();
    let mut seen: Vec<(Entity, usize)> = Vec::new();
    let mut placed = 0u32;

    // Diagnostics: a silent text pass is indistinguishable from a broken one, so
    // account for what the query saw and why glyphs were skipped.
    let mut n_text = 0u32;
    let mut n_invisible = 0u32;
    let mut n_no_glyphs = 0u32;
    let mut n_glyphs = 0u32;
    // Glyph bitmaps uploaded this tick. Should trend to zero as the cache warms;
    // if it keeps climbing, glyph reuse is broken.
    let mut uploaded = 0u32;

    for (entity, layout, block, bounds, anchor, xf, vis) in texts.iter() {
        n_text += 1;
        if !vis.get() {
            n_invisible += 1;
            continue;
        }
        if layout.glyphs.is_empty() {
            n_no_glyphs += 1;
            continue;
        }
        n_glyphs += layout.glyphs.len() as u32;

        // --- the extract_text2d_sprite transform, verbatim ------------------
        let inv_sf = layout.scale_factor.recip();
        let size = Vec2::new(
            bounds.width.unwrap_or(layout.size.x),
            bounds.height.unwrap_or(layout.size.y),
        );
        let top_left = (Anchor::TOP_LEFT.0 - anchor.as_vec()) * size;
        // base = global * translate(top_left) * scale(inv_sf, -inv_sf, 1)
        let base = *xf
            * GlobalTransform::from_translation(top_left.extend(0.0))
            * GlobalTransform::from_scale(Vec3::new(inv_sf, -inv_sf, 1.0));

        // Text sits above the world sprites. Text2d entities carry a z too, so
        // keep their relative order but bias the whole layer forward.
        let base_z = (xf.translation().z * Z_SPREAD) as i32 + TEXT_Z_BIAS;

        let mut cur_section = usize::MAX;
        let mut cur_color = bevy::color::Srgba::WHITE;

        for (gi, glyph) in layout.glyphs.iter().enumerate() {
            // Per-section colour, cached while the section is unchanged (exactly
            // how the real extractor does it).
            if glyph.section_index != cur_section {
                cur_color = section_colour(block, glyph.section_index, &text_colors);
                cur_section = glyph.section_index;
            }

            // Glyph world position -> viewport. This is the only part that differs
            // from the UI path below.
            let world = base * GlobalTransform::from_translation(glyph.position.extend(0.0));
            let Ok(vp) = camera.world_to_viewport(cam_xf, world.translation()) else {
                continue;
            };
            // `vp` is the glyph CENTRE; convert to its top-left. Glyph size in
            // world units is the atlas rect scaled by 1/scale_factor.
            let rect = glyph.atlas_info.rect;
            let vw = rect.width().round().max(1.0) * inv_sf;
            let vh = rect.height().round().max(1.0) * inv_sf;
            let top_left = Vec2::new(vp.x - vw * 0.5, vp.y - vh * 0.5);

            let slot_key = (entity, gi);
            seen.push(slot_key);
            if draw_glyph(
                &mut scene,
                &mut buf,
                &images,
                &fit,
                glyph,
                &cur_color,
                inv_sf,
                top_left,
                base_z,
                slot_key,
                &mut uploaded,
            ) {
                placed += 1;
            }
        }
    }

    // --- bevy_ui text -----------------------------------------------------
    //
    // The pleasant surprise of UI support: `bevy_ui`'s `Text` widget fills the SAME
    // `TextLayoutInfo` and rasterises into the SAME font atlas as `Text2d`, so UI
    // labels need no new machinery. They only need a different position calculation,
    // and a simpler one: UI coordinates are already screen coordinates, so there is
    // no camera projection and no Y flip.
    //
    // The glyph cache is shared, which is the real win. An "A" in a menu button and
    // an "A" in a world-space label, at the same size and colour, are one upload.
    #[cfg(feature = "ui")]
    for (entity, layout, block, node, xf, vis, clip) in ui_texts.iter() {
        n_text += 1;
        if !vis.get() {
            n_invisible += 1;
            continue;
        }
        if layout.glyphs.is_empty() || node.is_empty() {
            n_no_glyphs += 1;
            continue;
        }
        n_glyphs += layout.glyphs.len() as u32;

        // Mirrors bevy_ui_render's `extract_text_sections`: the glyph origin is the
        // node's content box corner, offset from the node centre. All of it is in the
        // target's PHYSICAL pixels, so it converts to logical space by the node's
        // inverse scale factor, the same as the UI rect pass does.
        //
        // The glyph atlas rect is in rasterised pixels and shares that same factor
        // here, because bevy_ui rasterises text at the target's scale factor. So one
        // conversion covers both position and size.
        let inv_sf = node.inverse_scale_factor;
        let centre = xf.translation;
        let content = node.content_box();
        let origin = centre + content.min;

        // UI sits above the world in the same band the UI pass uses, so a label is
        // never hidden behind its own panel.
        let ui_z = crate::ui::UI_Z_BASE + crate::ui::UI_TEXT_Z_BIAS;

        let mut cur_section = usize::MAX;
        let mut cur_color = bevy::color::Srgba::WHITE;

        for (gi, glyph) in layout.glyphs.iter().enumerate() {
            if glyph.section_index != cur_section {
                cur_color = section_colour(block, glyph.section_index, &text_colors);
                cur_section = glyph.section_index;
            }
            // `glyph.position` is the glyph CENTRE, in physical layout pixels.
            let rect = glyph.atlas_info.rect;
            let gw = rect.width().round().max(1.0);
            let gh = rect.height().round().max(1.0);
            let centre_px = origin + glyph.position;
            // Drop glyphs an ancestor's overflow setting clips away, so a scrolled
            // list does not paint its hidden rows over the surrounding interface. The
            // clip rect is in the same physical space, so compare before converting.
            if let Some(clip) = clip {
                if !clip.clip.contains(centre_px) {
                    continue;
                }
            }
            // Physical -> logical, for both the position and the glyph's own size.
            let top_left = (centre_px - Vec2::new(gw * 0.5, gh * 0.5)) * inv_sf;

            // UI glyph slots are keyed in the same map as Text2d ones. Entity ids are
            // unique across both, so they cannot collide.
            let slot_key = (entity, gi);
            seen.push(slot_key);
            if draw_glyph(
                &mut scene,
                &mut buf,
                &images,
                &fit,
                glyph,
                &cur_color,
                inv_sf,
                top_left,
                ui_z,
                slot_key,
                &mut uploaded,
            ) {
                placed += 1;
            }
        }
    }

    // Retire glyph slots no longer on screen (text changed or despawned).
    let gone: Vec<(Entity, usize)> = scene
        .slots
        .keys()
        .copied()
        .filter(|k| !seen.contains(k))
        .collect();
    for k in gone {
        if let Some(s) = scene.slots.remove(&k) {
            if s.cur_img_id != 0 {
                // Delete only this placement; the shared glyph IMAGE stays uploaded
                // for other and future occurrences.
                proto::delete_placement(&mut buf, s.cur_img_id, s.placement_id);
            }
        }
    }

    // Report what the query saw BEFORE any early return. A text pass that
    // silently does nothing must still say so, or a broken query looks like
    // "there was no text".
    if scene.tick <= 3 || scene.tick.is_multiple_of(120) {
        info!(
            "[kitty] glyph scan #{}: {} text entities ({} invisible, {} without laid-out \
             glyphs), {} glyphs total",
            scene.tick, n_text, n_invisible, n_no_glyphs, n_glyphs
        );
    }

    let bytes = buf.len();
    if !buf.is_empty() && !write_stdout(&buf, "glyph") {
        return;
    }
    if scene.tick <= 3 || scene.tick.is_multiple_of(120) {
        info!(
            "[kitty] glyph tick #{}: {} (re)placements, {} new glyph uploads, {} escape \
             bytes, {} glyph images cached, {} live slots",
            scene.tick,
            placed,
            uploaded,
            bytes,
            scene.glyphs.len(),
            scene.slots.len()
        );
    }
}

/// The glyph cache key.
///
/// Everything baked into the pixels has to appear here: kitty cannot tint or
/// non-uniformly scale a placement, so two colours or two target sizes of the same
/// letter are genuinely different images.
#[allow(clippy::too_many_arguments)]
fn glyph_key(
    texture: &str,
    rx: f32,
    ry: f32,
    gw: u32,
    gh: u32,
    color: &bevy::color::Srgba,
    tw: u32,
    th: u32,
) -> String {
    format!(
        "{texture}|{rx},{ry},{gw},{gh}|{:.3},{:.3},{:.3},{:.3}|{tw}x{th}",
        color.red, color.green, color.blue, color.alpha
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::color::Srgba;

    #[test]
    fn glyph_ids_never_collide_with_the_sprite_or_frame_bands() {
        let mut scene = GlyphScene::default();
        let id = scene.alloc_img_id();
        assert!(id > GLYPH_ID_BASE, "{id}");
        // Well clear of frame mode's id 1 and the sprite pass's 1000-based band.
        assert!(id > 1000, "glyph id {id} would collide with sprite ids");
    }

    #[test]
    fn the_same_letter_in_the_same_colour_and_size_shares_one_key() {
        // This is the reuse the whole pass exists for: 167 glyphs from 69 images.
        let a = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::WHITE, 30, 42);
        let b = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::WHITE, 30, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn colour_and_target_size_both_split_the_cache() {
        // Kitty cannot tint or rescale a placement, so either difference is a
        // genuinely different image. Omitting one from the key would draw the
        // wrong colour or a squashed letter.
        let base = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::WHITE, 30, 42);
        let recoloured = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::RED, 30, 42);
        let resized = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::WHITE, 60, 84);
        assert_ne!(base, recoloured);
        assert_ne!(base, resized);
    }

    #[test]
    fn different_atlas_rects_are_different_glyphs() {
        // Two letters at the same size in the same colour differ only by rect.
        let a = glyph_key("atlas0", 4.0, 8.0, 5, 7, &Srgba::WHITE, 30, 42);
        let b = glyph_key("atlas0", 12.0, 8.0, 5, 7, &Srgba::WHITE, 30, 42);
        assert_ne!(a, b);
    }

    #[test]
    fn glyph_placements_carry_no_cell_span() {
        // The fix for squashed text: cols/rows must stay 0 so kitty draws the
        // pre-scaled glyph 1:1. proto::place_scaled omits zero keys.
        let geom = Geom {
            row: 2,
            col: 66,
            xoff: 1,
            yoff: 20,
            z: 5_100_000,
            cols: 0,
            rows: 0,
        };
        let mut buf = Vec::new();
        proto::place_scaled(
            &mut buf, 100_001, 100_001, geom.z, geom.cols, geom.rows, geom.xoff, geom.yoff,
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains(",c="),
            "glyph placement must not size by cells: {s}"
        );
        assert!(
            !s.contains(",r="),
            "glyph placement must not size by cells: {s}"
        );
        assert!(s.contains("X=1"));
        assert!(s.contains("Y=20"));
    }

    #[test]
    fn text_sorts_above_y_sorted_world_sprites() {
        // The guarantee: text beats the y-sort band (world z around 1.0), so a
        // label is never hidden behind the actor it names, even at text z=0.
        let y_sort_max = (1.02_f32 * Z_SPREAD) as i32;
        let text_at_zero = (0.0_f32 * Z_SPREAD) as i32 + TEXT_Z_BIAS;
        assert!(
            text_at_zero > y_sort_max,
            "text at z=0 ({text_at_zero}) must beat the y-sort band ({y_sort_max})"
        );
    }

    #[test]
    fn a_high_z_overlay_still_draws_over_text_which_is_correct() {
        // Documenting the LIMIT of the bias rather than pretending it is absolute.
        // A full-screen colour grade at world z 100 tints the text along with the
        // scene, which is what a real window does. An app wanting text above its
        // own overlays raises that text's world z, same as on the GPU.
        let grade = (100.0_f32 * Z_SPREAD) as i32;
        let text_at_zero = TEXT_Z_BIAS;
        assert!(
            grade > text_at_zero,
            "a z=100 overlay ({grade}) is expected to sit above text ({text_at_zero})"
        );
    }
}
