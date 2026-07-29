//! Frame mode: capture the GPU's composited framebuffer each tick and send the
//! whole plate.
//!
//! This is the opposite approach to the sprite pass, and it exists partly as the
//! contrast. It reads pixels back out of the render target, so it reproduces
//! **everything** the GPU composites: z-ordering, alpha, baked lighting,
//! post-processing, per-pixel shaders. The sprite pass cannot do any of that.
//!
//! The cost is about 307 KB of base64 per changed frame at 320x180, roughly
//! 1 MB/s at 6 fps, which is why it is not the default and not viable over SSH.
//!
//! Keep it anyway. When sprite mode looks wrong, this is the ground truth to diff
//! against, and that is how several real bugs were found.

use bevy::prelude::*;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};

use crate::{proto, write_stdout, KittyConfig, TargetState};

/// Round `value` up to a multiple of 256, wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT`.
///
/// Bevy's gpu readback returns the raw, row-PADDED copy buffer: it does NOT strip
/// the padding. So the decode path has to be stride-aware even though at 320 px
/// (1280 B = 5 x 256) there happens to be no padding today. A future resolution
/// change must not silently corrupt every frame.
fn aligned_bytes_per_row(unpadded: u32) -> u32 {
    let align = 256;
    unpadded.div_ceil(align) * align
}

/// A cheap non-cryptographic hash (FNV-1a) for dirty-skip. We only need "did this
/// frame change", not collision resistance.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// State for frame mode: last-frame hash for dirty-skip, plus a reused buffer.
#[derive(Resource, Default)]
pub struct FrameState {
    last_hash: u64,
    scratch: Vec<u8>,
    frames_emitted: u64,
    frames_skipped: u64,
}

pub(crate) fn build(app: &mut App) {
    app.init_resource::<FrameState>();
    app.add_systems(Update, attach_readback_once);
    app.add_observer(on_readback_complete);
}

/// Once the render target exists, spawn an entity holding the readback request.
///
/// Bevy then fires `ReadbackComplete` on it every frame until the component is
/// removed.
fn attach_readback_once(mut commands: Commands, target: Res<TargetState>, mut done: Local<bool>) {
    if *done {
        return;
    }
    let Some(handle) = target.image.clone() else {
        return; // target not armed yet
    };
    commands.spawn(Readback::texture(handle));
    *done = true;
    info!("[kitty] frame readback attached");
}

/// Fired when a gpu readback completes. Decode stride-aware, dirty-skip unchanged
/// frames, and emit the plate to the terminal.
fn on_readback_complete(
    trigger: On<ReadbackComplete>,
    mut state: ResMut<FrameState>,
    config: Res<KittyConfig>,
) {
    let padded = &trigger.event().data;
    // Frame mode never scales its target (see KittyConfig::target_scale_factor),
    // so the readback is exactly virtual_size.
    let width = config.virtual_size.x;
    let height = config.virtual_size.y;
    let unpadded_row = (width * 4) as usize;
    let padded_row = aligned_bytes_per_row(width * 4) as usize;

    let expected = padded_row * height as usize;
    if padded.len() < expected {
        error!(
            "[kitty] readback short: got {} bytes, expected >= {} ({}x{} @ stride {}) - \
             dropping frame",
            padded.len(),
            expected,
            width,
            height,
            padded_row
        );
        return;
    }

    // Repack padded rows into tight RGBA in the reused scratch buffer.
    let tight_len = unpadded_row * height as usize;
    state.scratch.clear();
    state.scratch.reserve(tight_len);
    if padded_row == unpadded_row {
        // No padding at this resolution: one contiguous copy.
        state.scratch.extend_from_slice(&padded[..tight_len]);
    } else {
        for row in 0..height as usize {
            let start = row * padded_row;
            state
                .scratch
                .extend_from_slice(&padded[start..start + unpadded_row]);
        }
    }

    // Dirty-skip: an unchanged frame costs zero pty bytes.
    let hash = fnv1a(&state.scratch);
    if hash == state.last_hash && state.frames_emitted > 0 {
        state.frames_skipped += 1;
        // 1, 61, 121, ... rather than every 60th: the FIRST skip is the
        // interesting one, because it says dirty-skip started working at all.
        if state.frames_skipped % 60 == 1 {
            debug!(
                "[kitty] frame unchanged (skipped {} so far) - not re-sending",
                state.frames_skipped
            );
        }
        return;
    }
    state.last_hash = hash;

    // Encode and emit. Fixed image id 1, so each frame replaces the last and the
    // terminal's image count never grows.
    let mut buf = Vec::with_capacity(state.scratch.len() * 2);
    proto::cursor_to(&mut buf, 1, 1);
    proto::transmit_and_display_rgba(&mut buf, 1, width, height, &state.scratch);

    if !write_stdout(&buf, "frame") {
        return;
    }

    state.frames_emitted += 1;
    if state.frames_emitted <= 3 || state.frames_emitted.is_multiple_of(120) {
        info!(
            "[kitty] emitted frame #{} ({} bytes RGBA, {} escape bytes)",
            state.frames_emitted,
            state.scratch.len(),
            buf.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_alignment_rounds_up_to_256() {
        assert_eq!(
            aligned_bytes_per_row(1280),
            1280,
            "320px is already aligned"
        );
        assert_eq!(aligned_bytes_per_row(1281), 1536);
        assert_eq!(aligned_bytes_per_row(1), 256);
        assert_eq!(aligned_bytes_per_row(256), 256);
    }

    #[test]
    fn a_common_resolution_actually_needs_padding() {
        // The reason the decode is stride-aware even though 320px is not padded:
        // change the width and it immediately matters. 200px -> 800B -> 1024B.
        assert_ne!(aligned_bytes_per_row(200 * 4), 200 * 4);
    }

    #[test]
    fn hash_distinguishes_frames_but_matches_identical_ones() {
        let a = vec![1u8, 2, 3, 4];
        let b = vec![1u8, 2, 3, 5];
        assert_eq!(fnv1a(&a), fnv1a(&a.clone()));
        assert_ne!(fnv1a(&a), fnv1a(&b));
        // An empty frame must not collide with the zero-initialised last_hash of
        // 0, or the very first frame would be skipped as "unchanged".
        assert_ne!(fnv1a(&[]), 0);
    }
}
