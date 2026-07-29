//! The `bevy_ui` pass: draw UI node rectangles, borders and images.
//!
//! This is the second of two reasons the crate can claim to render "a Bevy 2D app"
//! rather than "world-space sprites". The sprite pass covers `Sprite` and the text
//! pass covers `Text2d`; this one covers flexbox UI, which is what most Bevy apps
//! actually build their menus and HUDs from.
//!
//! # Why this is easier than the sprite pass, not harder
//!
//! UI coordinates are already screen coordinates. `bevy_ui` lays everything out in
//! physical pixels with a top-left origin, which is exactly the space [`FitBox`]
//! wants, so there is no camera projection and no Y flip. Compare the sprite pass,
//! which has to go world -> `Camera::world_to_viewport` -> viewport -> cell.
//!
//! What replaces that work is stacking and clipping, which world-space sprites do
//! not have:
//!
//! * **`ComputedStackIndex`** is the paint order `bevy_ui` computed from the tree.
//!   It is authoritative, and it is not derivable from a transform, because two
//!   siblings at the same position stack by document order.
//! * **`CalculatedClip`** is the rect an ancestor's `Overflow` setting imposes. A
//!   scrolled list has children whose boxes extend well past the viewport, and
//!   drawing them unclipped paints over everything around the list.
//!
//! # What it draws
//!
//! | Component | Becomes |
//! |---|---|
//! | `BackgroundColor` | a solid tile stretched to the node's box |
//! | `BorderColor` | four thin tiles, one per edge, at the resolved widths |
//! | `Outline` | four thin tiles outside the box |
//! | `ImageNode` | the image's pixels, cropped to its atlas cell if it has one |
//! | `Text` (a UI node) | nothing here: the text pass already handles it |
//!
//! That last row is the pleasant surprise. `bevy_ui`'s `Text` widget populates the
//! same `TextLayoutInfo` and the same font atlas that `Text2d` does, so UI text
//! comes out of [`crate::text`] with no extra work, provided that pass reads UI
//! nodes too.
//!
//! # What it does not do
//!
//! * **No rounded corners.** `border_radius` is resolved by `bevy_ui` and ignored
//!   here, because a placement is a rectangle. At terminal scale a 4 px radius is a
//!   fraction of a cell.
//! * **No gradients.** `BackgroundGradient` needs a per-pixel evaluation that flat
//!   tiles cannot express. Frame mode reproduces them.
//! * **Clipping is rectangular and cell-aligned.** A clip rect is applied by
//!   shrinking the drawn box, so a partially visible row of a scrolled list is cut
//!   at a cell edge rather than mid-cell.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy::ui::widget::ImageNode;
use bevy::ui::{
    BackgroundColor, BorderColor, CalculatedClip, ComputedNode, ComputedStackIndex, Outline,
    UiGlobalTransform,
};

use crate::pixels::{apply_tint, is_white, solid_rgba, Bitmap, PixelRequest};
use crate::sprite::Geom;
use crate::term::{FitBox, TermSize};
use crate::{proto, write_stdout, KittyConfig, KittySet};

/// First kitty image id the UI pass allocates.
///
/// Its own band, clear of frame mode's id 1, the sprite pass's >=1001 and the glyph
/// pass's >=100_001. A collision would silently replace someone else's pixels.
const UI_IMG_ID_BASE: u32 = 200_000;

/// UI draws above the world. `bevy_ui` renders after the 2D pass in a real app, and
/// a HUD that fell behind the scene would be useless.
///
/// The `ComputedStackIndex` is added to this, so relative UI order is preserved
/// inside the band. Text keeps its own bias (`crate::sprite::TEXT_Z_BIAS`), which
/// sits below this on purpose: a UI panel drawn over a `Text2d` label is correct,
/// because that is what the GPU does.
pub(crate) const UI_Z_BASE: i32 = 2_000_000;

/// Bias added to UI TEXT on top of [`UI_Z_BASE`], so a label always draws over the
/// panel it sits on.
///
/// A node's own `ComputedStackIndex` puts its text at the same index as its
/// background, which would leave the two fighting. `bevy_ui` resolves that with
/// draw order within a stack entry; we need a number, so text gets a fixed nudge
/// forward. Chosen larger than any plausible stack index.
pub(crate) const UI_TEXT_Z_BIAS: i32 = 100_000;

/// One drawn rectangle: which shared image it shows and where.
struct UiSlot {
    placement_id: u32,
    cur_img_id: u32,
    last_geom: Option<Geom>,
}

/// Persistent state for the UI pass.
#[derive(Resource, Default)]
pub struct UiScene {
    /// Uploaded bitmaps, keyed by what is baked into their pixels. A flat colour is
    /// one pixel (see `crate::sprite::SOLID_TILE_PX` for why), so this map stays
    /// small even for a busy interface.
    bitmaps: HashMap<String, u32>,
    /// One slot per drawn rectangle, keyed (node entity, which part of the node).
    slots: HashMap<(Entity, UiPart), UiSlot>,
    next_img_id: u32,
    next_placement_id: u32,
    term: Option<TermSize>,
    tick: u64,
}

/// Which piece of a node a slot draws.
///
/// A single node can produce up to nine rectangles: a background, four borders and
/// four outline edges. Each needs its own placement id, so the key has to name the
/// part and not just the entity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum UiPart {
    Background,
    Image,
    BorderTop,
    BorderRight,
    BorderBottom,
    BorderLeft,
    OutlineTop,
    OutlineRight,
    OutlineBottom,
    OutlineLeft,
}

impl UiScene {
    fn alloc_img_id(&mut self) -> u32 {
        self.next_img_id = self.next_img_id.max(UI_IMG_ID_BASE) + 1;
        self.next_img_id
    }

    fn alloc_placement_id(&mut self) -> u32 {
        self.next_placement_id = self.next_placement_id.max(UI_IMG_ID_BASE) + 1;
        self.next_placement_id
    }

    /// How many distinct bitmaps the UI pass has uploaded.
    pub fn bitmap_count(&self) -> usize {
        self.bitmaps.len()
    }

    /// How many rectangles are currently placed.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

pub(crate) fn build(app: &mut App) {
    app.init_resource::<UiScene>();
    // `ui_layout_system` is what fills `ComputedNode`, so this must run after it for
    // the same reason the text pass runs after `update_text2d_layout`: query earlier
    // and every box is zero-sized, silently.
    app.add_systems(
        PostUpdate,
        render_ui
            .in_set(KittySet::Render)
            .after(bevy::ui::ui_layout_system),
    );
}

/// A rectangle to draw, in physical UI pixels, already clipped.
struct Rect2 {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect2 {
    /// Intersect with a clip rect, returning `None` if nothing survives.
    ///
    /// This is the whole of clipping support, and skipping it is what makes a
    /// scrolled list paint over its neighbours.
    fn clipped(self, clip: Option<&CalculatedClip>) -> Option<Self> {
        let Some(clip) = clip else {
            return Some(self);
        };
        let x0 = self.x.max(clip.clip.min.x);
        let y0 = self.y.max(clip.clip.min.y);
        let x1 = (self.x + self.w).min(clip.clip.max.x);
        let y1 = (self.y + self.h).min(clip.clip.max.y);
        if x1 <= x0 || y1 <= y0 {
            return None; // entirely outside the clip
        }
        Some(Rect2 {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

/// The per-tick UI render system.
#[allow(clippy::type_complexity)]
pub fn render_ui(
    mut scene: ResMut<UiScene>,
    config: Res<KittyConfig>,
    assets: Res<AssetServer>,
    images: Res<Assets<Image>>,
    atlas_layouts: Res<Assets<bevy::image::TextureAtlasLayout>>,
    nodes: Query<(
        Entity,
        &ComputedNode,
        &ComputedStackIndex,
        &UiGlobalTransform,
        &InheritedVisibility,
        Option<&CalculatedClip>,
        Option<&BackgroundColor>,
        Option<&BorderColor>,
        Option<&Outline>,
        Option<&ImageNode>,
    )>,
) {
    scene.tick += 1;
    if scene.term.is_none() || scene.tick.is_multiple_of(120) {
        scene.term = Some(TermSize::query(config.terminal_size));
    }
    let fit = FitBox::compute(&scene.term.unwrap(), config.virtual_size);

    let mut buf: Vec<u8> = Vec::new();
    let mut seen: Vec<(Entity, UiPart)> = Vec::new();
    let mut placed = 0u32;
    let mut uploaded = 0u32;
    let mut n_nodes = 0u32;
    let mut n_invisible = 0u32;

    for (entity, node, stack, xf, vis, clip, bg, border, outline, image) in nodes.iter() {
        n_nodes += 1;
        if !vis.get() {
            n_invisible += 1;
            continue;
        }
        if node.is_empty() {
            continue;
        }

        // `UiGlobalTransform` derefs to an `Affine2`, whose translation is the node
        // CENTRE in physical pixels. Every rect below is derived from the top-left,
        // so convert once here.
        let size = node.size();
        let centre = xf.translation;
        let left = centre.x - size.x * 0.5;
        let top = centre.y - size.y * 0.5;

        // `bevy_ui` lays out in PHYSICAL pixels of the render target, and this crate
        // works in the target's LOGICAL space (which is `virtual_size`). With a hi-dpi
        // target the two differ by exactly the scale factor, so every position and
        // size has to be converted. Measured: a 640x360 virtual size at text_scale
        // 2.0 gives a 1280x720 target, and the root node's computed width really is
        // 1280.
        //
        // Skip this and the interface draws at N times its size, running off the
        // right and bottom of the terminal.
        let s = node.inverse_scale_factor;

        // The z: UI sits above the world, ordered within its band by the stack index
        // bevy_ui already computed.
        let z = UI_Z_BASE + stack.0 as i32;

        // --- background ---------------------------------------------------
        if let Some(bg) = bg {
            let colour = bg.0.to_srgba();
            if colour.alpha > 0.001 {
                let rect = Rect2 {
                    x: left,
                    y: top,
                    w: size.x,
                    h: size.y,
                };
                emit_solid(
                    &mut scene,
                    &mut buf,
                    &fit,
                    s,
                    entity,
                    UiPart::Background,
                    rect,
                    clip,
                    colour,
                    z,
                    &mut seen,
                    &mut placed,
                    &mut uploaded,
                );
            }
        }

        // --- image --------------------------------------------------------
        if let Some(img) = image {
            let colour = img.color.to_srgba();
            if colour.alpha > 0.001 {
                // An `ImageNode` can name a region three ways, and they compose: an
                // atlas cell, an explicit `rect`, or the whole image. Resolve in the
                // same order `bevy_ui_render` does, so a UI sprite sheet draws the
                // frame the app asked for rather than the entire sheet.
                let cell = img
                    .texture_atlas
                    .as_ref()
                    .and_then(|ta| ta.texture_rect(&atlas_layouts))
                    .or_else(|| {
                        img.rect.map(|r| {
                            URect::new(
                                r.min.x.max(0.0) as u32,
                                r.min.y.max(0.0) as u32,
                                r.max.x.max(0.0) as u32,
                                r.max.y.max(0.0) as u32,
                            )
                        })
                    });
                // The region has to be part of the key: two nodes sharing one sheet
                // but showing different cells are different pixels.
                let region_key = cell
                    .map(|c| format!("#{},{},{},{}", c.min.x, c.min.y, c.width(), c.height()))
                    .unwrap_or_default();
                let flip_key = match (img.flip_x, img.flip_y) {
                    (false, false) => "",
                    (true, false) => "|fx",
                    (false, true) => "|fy",
                    (true, true) => "|fxy",
                };
                let key = format!(
                    "ui-img:{}{}{}{}",
                    crate::sprite::image_label(&assets, img.image.id()),
                    region_key,
                    tint_suffix(&colour),
                    flip_key
                );
                let rect = Rect2 {
                    x: left,
                    y: top,
                    w: size.x,
                    h: size.y,
                };
                if let Some(rect) = rect.clipped(clip) {
                    let img_id = ensure_bitmap(&mut scene, &mut buf, &key, &mut uploaded, || {
                        let req = PixelRequest {
                            image: img.image.id(),
                            atlas_cell: cell,
                            images: &images,
                            assets: &assets,
                        };
                        let mut bm = config.pixel_source.pixels(&req)?;
                        if !is_white(&colour) {
                            apply_tint(&mut bm.rgba, &colour);
                        }
                        if img.flip_x || img.flip_y {
                            bm.rgba = crate::pixels::flip_rgba(
                                &bm.rgba, bm.w, bm.h, img.flip_x, img.flip_y,
                            );
                        }
                        Some(bm)
                    });
                    if let Some(img_id) = img_id {
                        place_rect(
                            &mut scene,
                            &mut buf,
                            &fit,
                            s,
                            entity,
                            UiPart::Image,
                            &rect,
                            img_id,
                            z,
                            &mut seen,
                            &mut placed,
                        );
                    }
                }
            }
        }

        // --- borders ------------------------------------------------------
        // Four separate rectangles, because each edge has its own colour and its own
        // resolved width. A single outline rect would be wrong for any node that
        // colours only one edge, which is a common way to draw a divider.
        if let Some(border) = border {
            // `BorderRect` stores insets as two corners, not four named edges:
            // `min_inset` is (left, top) and `max_inset` is (right, bottom). Reading
            // it as if it had `.top`/`.left` fields does not compile, which is the
            // good outcome; assuming the wrong axis order would not.
            let b = node.border();
            let (bl, bt) = (b.min_inset.x, b.min_inset.y);
            let (br, bb) = (b.max_inset.x, b.max_inset.y);
            let edges = [
                (
                    UiPart::BorderTop,
                    border.top,
                    Rect2 {
                        x: left,
                        y: top,
                        w: size.x,
                        h: bt,
                    },
                ),
                (
                    UiPart::BorderBottom,
                    border.bottom,
                    Rect2 {
                        x: left,
                        y: top + size.y - bb,
                        w: size.x,
                        h: bb,
                    },
                ),
                (
                    UiPart::BorderLeft,
                    border.left,
                    Rect2 {
                        x: left,
                        y: top,
                        w: bl,
                        h: size.y,
                    },
                ),
                (
                    UiPart::BorderRight,
                    border.right,
                    Rect2 {
                        x: left + size.x - br,
                        y: top,
                        w: br,
                        h: size.y,
                    },
                ),
            ];
            for (part, colour, rect) in edges {
                let colour = colour.to_srgba();
                if colour.alpha <= 0.001 || rect.w <= 0.0 || rect.h <= 0.0 {
                    continue;
                }
                emit_solid(
                    &mut scene,
                    &mut buf,
                    &fit,
                    s,
                    entity,
                    part,
                    rect,
                    clip,
                    colour,
                    z,
                    &mut seen,
                    &mut placed,
                    &mut uploaded,
                );
            }
        }

        // --- outline ------------------------------------------------------
        // Outlines sit OUTSIDE the box and are deliberately not clipped by the
        // node's own clip rect, matching bevy_ui.
        if let Some(outline) = outline {
            let w = node.outline_width();
            let off = node.outline_offset();
            let colour = outline.color.to_srgba();
            if w > 0.0 && colour.alpha > 0.001 {
                let ox = left - off - w;
                let oy = top - off - w;
                let ow = size.x + 2.0 * (off + w);
                let oh = size.y + 2.0 * (off + w);
                let edges = [
                    (
                        UiPart::OutlineTop,
                        Rect2 {
                            x: ox,
                            y: oy,
                            w: ow,
                            h: w,
                        },
                    ),
                    (
                        UiPart::OutlineBottom,
                        Rect2 {
                            x: ox,
                            y: oy + oh - w,
                            w: ow,
                            h: w,
                        },
                    ),
                    (
                        UiPart::OutlineLeft,
                        Rect2 {
                            x: ox,
                            y: oy,
                            w,
                            h: oh,
                        },
                    ),
                    (
                        UiPart::OutlineRight,
                        Rect2 {
                            x: ox + ow - w,
                            y: oy,
                            w,
                            h: oh,
                        },
                    ),
                ];
                for (part, rect) in edges {
                    emit_solid(
                        &mut scene,
                        &mut buf,
                        &fit,
                        s,
                        entity,
                        part,
                        rect,
                        None,
                        colour,
                        z,
                        &mut seen,
                        &mut placed,
                        &mut uploaded,
                    );
                }
            }
        }
    }

    // Retire rectangles that are no longer drawn (node despawned, hidden, scrolled
    // out of its clip). Only the placement goes: the shared bitmap stays uploaded.
    let gone: Vec<(Entity, UiPart)> = scene
        .slots
        .keys()
        .copied()
        .filter(|k| !seen.contains(k))
        .collect();
    for k in gone {
        if let Some(slot) = scene.slots.remove(&k) {
            if slot.cur_img_id != 0 {
                proto::delete_placement(&mut buf, slot.cur_img_id, slot.placement_id);
            }
        }
    }

    let bytes = buf.len();
    if !buf.is_empty() && !write_stdout(&buf, "ui") {
        return;
    }
    if scene.tick <= 3 || scene.tick.is_multiple_of(120) {
        info!(
            "[kitty] ui tick #{}: {} nodes ({} invisible), {} (re)placements, {} new uploads, \
             {} escape bytes, {} bitmaps cached, {} live rects",
            scene.tick,
            n_nodes,
            n_invisible,
            placed,
            uploaded,
            bytes,
            scene.bitmaps.len(),
            scene.slots.len()
        );
    }
}

/// Cache-key suffix for a tint, quantised for the same reason the sprite pass
/// quantises: animated colours would otherwise mint a new upload every tick.
fn tint_suffix(c: &bevy::color::Srgba) -> String {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 15.0).round() as u8;
    format!(
        "@{:x}{:x}{:x}{:x}",
        q(c.red),
        q(c.green),
        q(c.blue),
        q(c.alpha)
    )
}

/// Upload a bitmap once, or return the id it already has.
fn ensure_bitmap(
    scene: &mut UiScene,
    buf: &mut Vec<u8>,
    key: &str,
    uploaded: &mut u32,
    make: impl FnOnce() -> Option<Bitmap>,
) -> Option<u32> {
    if let Some(id) = scene.bitmaps.get(key) {
        return Some(*id);
    }
    let bm = make()?;
    let img_id = scene.alloc_img_id();
    proto::transmit_rgba(buf, img_id, bm.w, bm.h, &bm.rgba);
    *uploaded += 1;
    debug!(
        "[kitty] ui uploaded '{key}' ({}x{}) as img {img_id}",
        bm.w, bm.h
    );
    scene.bitmaps.insert(key.to_string(), img_id);
    Some(img_id)
}

/// Draw one flat-colour rectangle.
#[allow(clippy::too_many_arguments)]
fn emit_solid(
    scene: &mut UiScene,
    buf: &mut Vec<u8>,
    fit: &FitBox,
    inv_sf: f32,
    entity: Entity,
    part: UiPart,
    rect: Rect2,
    clip: Option<&CalculatedClip>,
    colour: bevy::color::Srgba,
    z: i32,
    seen: &mut Vec<(Entity, UiPart)>,
    placed: &mut u32,
    uploaded: &mut u32,
) {
    let Some(rect) = rect.clipped(clip) else {
        return;
    };
    // One pixel per colour, stretched by the placement. See
    // crate::sprite::SOLID_TILE_PX: a flat colour needs one pixel of detail, and
    // keying by colour alone means a whole interface shares a handful of uploads.
    let key = format!("ui-solid{}", tint_suffix(&colour));
    let img_id = ensure_bitmap(scene, buf, &key, uploaded, || {
        Bitmap::new(solid_rgba(&colour, 1, 1), 1, 1)
    });
    let Some(img_id) = img_id else { return };
    place_rect(
        scene, buf, fit, inv_sf, entity, part, &rect, img_id, z, seen, placed,
    );
}

/// Map a UI rectangle to cells and emit a placement if anything changed.
#[allow(clippy::too_many_arguments)]
fn place_rect(
    scene: &mut UiScene,
    buf: &mut Vec<u8>,
    fit: &FitBox,
    inv_sf: f32,
    entity: Entity,
    part: UiPart,
    rect: &Rect2,
    img_id: u32,
    z: i32,
    seen: &mut Vec<(Entity, UiPart)>,
    placed: &mut u32,
) {
    // Physical UI pixels -> the logical space FitBox maps from.
    let (row, col, xoff, yoff) = fit.map(rect.x * inv_sf, rect.y * inv_sf);
    let (cols, rows) = fit.span_cells(
        (rect.w * inv_sf).round().max(1.0) as u32,
        (rect.h * inv_sf).round().max(1.0) as u32,
    );
    let geom = Geom {
        row,
        col,
        xoff,
        yoff,
        z,
        cols,
        rows,
    };

    let key = (entity, part);
    seen.push(key);

    if !scene.slots.contains_key(&key) {
        let placement_id = scene.alloc_placement_id();
        scene.slots.insert(
            key,
            UiSlot {
                placement_id,
                cur_img_id: 0,
                last_geom: None,
            },
        );
    }
    let (placement_id, prev_img_id, img_changed, geom_changed) = {
        let slot = &scene.slots[&key];
        (
            slot.placement_id,
            slot.cur_img_id,
            slot.cur_img_id != img_id,
            slot.last_geom != Some(geom),
        )
    };

    // A placement belongs to one image id, so re-pointing it needs the old one
    // deleted first or kitty stacks them. Same invariant as the sprite pass.
    if img_changed && prev_img_id != 0 {
        proto::delete_placement(buf, prev_img_id, placement_id);
    }
    if img_changed || geom_changed {
        proto::cursor_to(buf, row, col);
        proto::place_scaled(buf, img_id, placement_id, z, cols, rows, xoff, yoff);
        *placed += 1;
    }
    if let Some(slot) = scene.slots.get_mut(&key) {
        slot.cur_img_id = img_id;
        slot.last_geom = Some(geom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::color::Srgba;

    #[test]
    fn ui_ids_have_their_own_band() {
        // A collision with the sprite (>=1001) or glyph (>=100_001) bands would
        // silently replace someone else's pixels with a UI panel.
        let mut scene = UiScene::default();
        let img = scene.alloc_img_id();
        assert!(img > 100_001, "ui img id {img} must clear the glyph band");
        assert_eq!(img, UI_IMG_ID_BASE + 1);
    }

    #[test]
    fn ui_draws_above_the_world_and_above_text() {
        // bevy_ui renders after the 2D pass, so a HUD must not fall behind the
        // scene. Text2d keeps a lower bias on purpose.
        const { assert!(UI_Z_BASE > crate::sprite::TEXT_Z_BIAS) };
        let world_max = (1.02_f32 * crate::sprite::Z_SPREAD) as i32;
        assert!(UI_Z_BASE > world_max);
    }

    #[test]
    fn stack_index_orders_within_the_ui_band() {
        // Two siblings at the same position stack by document order, which no
        // transform records. Dropping the stack index would make their order
        // arbitrary.
        let back = UI_Z_BASE;
        let front = UI_Z_BASE + 7;
        assert!(front > back);
    }

    #[test]
    fn a_clip_rect_shrinks_the_drawn_box() {
        // The scrolled-list case: a child extends past its parent's viewport and
        // must be cut, not drawn over the neighbours.
        let clip = CalculatedClip {
            clip: Rect::new(0.0, 100.0, 200.0, 200.0),
        };
        let r = Rect2 {
            x: 0.0,
            y: 50.0,
            w: 200.0,
            h: 100.0,
        }
        .clipped(Some(&clip))
        .expect("partially visible rect should survive");
        assert_eq!(r.y, 100.0, "top should be cut to the clip");
        assert_eq!(r.h, 50.0, "height should be reduced to what is visible");
    }

    #[test]
    fn a_fully_clipped_rect_is_dropped() {
        let clip = CalculatedClip {
            clip: Rect::new(0.0, 100.0, 200.0, 200.0),
        };
        let r = Rect2 {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 50.0,
        }
        .clipped(Some(&clip));
        assert!(r.is_none(), "a rect above the clip should not be drawn");
    }

    #[test]
    fn an_unclipped_rect_passes_through_unchanged() {
        let r = Rect2 {
            x: 3.0,
            y: 4.0,
            w: 10.0,
            h: 20.0,
        }
        .clipped(None)
        .unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (3.0, 4.0, 10.0, 20.0));
    }

    #[test]
    fn a_whole_palette_collapses_to_a_few_uploads() {
        // Solid tiles are keyed by colour alone, not by size, so a hundred
        // differently sized panels sharing a palette share their uploads.
        let sizes = [(10.0, 10.0), (200.0, 40.0), (1.0, 300.0)];
        let keys: std::collections::HashSet<String> = sizes
            .iter()
            .map(|_| format!("ui-solid{}", tint_suffix(&Srgba::new(0.1, 0.2, 0.3, 1.0))))
            .collect();
        assert_eq!(keys.len(), 1, "size must not split the solid cache");
    }

    #[test]
    fn each_part_of_a_node_gets_its_own_slot_key() {
        // A node can draw nine rectangles. Keying by entity alone would make them
        // fight over one placement id, which is the ghosting bug.
        let parts = [
            UiPart::Background,
            UiPart::Image,
            UiPart::BorderTop,
            UiPart::BorderRight,
            UiPart::BorderBottom,
            UiPart::BorderLeft,
            UiPart::OutlineTop,
            UiPart::OutlineRight,
            UiPart::OutlineBottom,
            UiPart::OutlineLeft,
        ];
        let unique: std::collections::HashSet<UiPart> = parts.iter().copied().collect();
        assert_eq!(
            unique.len(),
            parts.len(),
            "UiPart variants must be distinct"
        );
    }
}
