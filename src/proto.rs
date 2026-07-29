//! The kitty graphics protocol encoder. Pure functions that append escape
//! sequences to a byte buffer: no Bevy, no terminal, no I/O, unit-testable in
//! isolation.
//!
//! An "APC" packet is `ESC _ G <control> ; <payload> ESC \` where `<control>` is
//! comma-separated `key=value` pairs and `<payload>` is base64. Payloads over
//! 4096 bytes must be split into chunks flagged `m=1` (more follow) / `m=0`
//! (last).
//!
//! This module is the part most likely to be useful to someone writing their own
//! renderer, so it deliberately knows nothing about the rest of the crate.

use base64::Engine as _;

/// Max base64 bytes per APC chunk, per the kitty spec.
const CHUNK: usize = 4096;

/// The introducer every graphics packet starts with: `ESC _ G`.
///
/// Exported because stdout is a shared channel. Anything else writing its own APC
/// blocks to the same stream (an audio side-channel, a status protocol) has to pick
/// a different letter after `ESC _`, and needs something to compare against. A
/// private literal invites a copied one, which stays green in tests while the two
/// streams collide on a real terminal.
pub const APC_GRAPHICS_INTRODUCER: &[u8] = b"\x1b_G";

/// The terminator ending every APC block: `ESC \`.
pub const APC_TERMINATOR: &[u8] = b"\x1b\\";

/// The base64 alphabet kitty expects (standard, with padding).
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Write one kitty APC packet, chunking the base64 payload across multiple
/// packets when it exceeds the 4096-byte limit. `control` is the control block
/// for the FIRST chunk (e.g. `"a=T,f=32,s=320,v=180,i=1,q=2"`); continuation
/// chunks carry only `m=1`/`m=0` as the spec requires.
fn write_apc(out: &mut Vec<u8>, control: &str, payload_b64: &[u8]) {
    if payload_b64.is_empty() {
        // A control-only packet (e.g. a delete/placement command).
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(control.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        return;
    }
    let chunks: Vec<&[u8]> = payload_b64.chunks(CHUNK).collect();
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.iter().enumerate() {
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            out.extend_from_slice(control.as_bytes());
            out.push(b',');
        }
        // m=1 on every chunk except the last.
        if i == last {
            out.extend_from_slice(b"m=0");
        } else {
            out.extend_from_slice(b"m=1");
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Transmit-and-display a full RGBA image under a fixed image id. Sending the
/// same `id` again REPLACES the prior image data, so the frame count on the
/// terminal side never grows. `f=32` = RGBA, `q=2` = suppress all terminal
/// responses (so the app's stdin is not polluted with replies).
pub fn transmit_and_display_rgba(out: &mut Vec<u8>, id: u32, width: u32, height: u32, rgba: &[u8]) {
    let control = format!("a=T,f=32,s={width},v={height},i={id},q=2");
    let payload = b64(rgba);
    write_apc(out, &control, payload.as_bytes());
}

/// Transmit an image WITHOUT displaying it (`a=t`), keyed by `id`, for later
/// placement. This is how sprite mode uploads each distinct bitmap exactly once.
pub fn transmit_rgba(out: &mut Vec<u8>, id: u32, width: u32, height: u32, rgba: &[u8]) {
    let control = format!("a=t,f=32,s={width},v={height},i={id},q=2");
    let payload = b64(rgba);
    write_apc(out, &control, payload.as_bytes());
}

/// Place an already-transmitted image `img_id` at the current cursor cell, as
/// placement `placement_id`, at stacking order `z` (higher = in front). `C=1`
/// keeps the cursor from moving after placement.
pub fn place(out: &mut Vec<u8>, img_id: u32, placement_id: u32, z: i32) {
    let control = format!("a=p,i={img_id},p={placement_id},z={z},C=1,q=2");
    write_apc(out, &control, &[]);
}

/// Place `img_id` scaled to span `cols`x`rows` terminal cells at the current
/// cursor cell, with a sub-cell pixel offset (`xoff`,`yoff`) into the first
/// cell. Same placement id => replaces the prior placement in place (no
/// flicker), which is how sprite mode moves a sprite each tick cheaply. `z` sets
/// stacking order. Any of cols/rows/offset that are zero are omitted, so kitty
/// draws the image at its natural size (the aspect-preserving default).
#[allow(clippy::too_many_arguments)]
pub fn place_scaled(
    out: &mut Vec<u8>,
    img_id: u32,
    placement_id: u32,
    z: i32,
    cols: u32,
    rows: u32,
    xoff: u32,
    yoff: u32,
) {
    let mut control = format!("a=p,i={img_id},p={placement_id},z={z},C=1,q=2");
    if cols > 0 {
        control.push_str(&format!(",c={cols}"));
    }
    if rows > 0 {
        control.push_str(&format!(",r={rows}"));
    }
    if xoff > 0 {
        control.push_str(&format!(",X={xoff}"));
    }
    if yoff > 0 {
        control.push_str(&format!(",Y={yoff}"));
    }
    write_apc(out, &control, &[]);
}

/// Delete a specific placement of an image (frees the on-screen instance but
/// keeps the transmitted image data for re-placement). `d=i` with a placement id
/// deletes just that placement.
pub fn delete_placement(out: &mut Vec<u8>, img_id: u32, placement_id: u32) {
    let control = format!("a=d,d=i,i={img_id},p={placement_id},q=2");
    write_apc(out, &control, &[]);
}

/// Delete ALL placements (keeps image data). A cheap full-screen clear when
/// tracking individual placements is not worth it.
pub fn delete_all_placements(out: &mut Vec<u8>) {
    write_apc(out, "a=d,d=a,q=2", &[]);
}

/// Move the terminal cursor to a 1-based (row, col) cell. Plain CSI, not a kitty
/// APC: used to position the next placement.
pub fn cursor_to(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

/// Clear the screen, home the cursor and hide it. Emitted once at startup.
pub fn enter_screen(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[2J\x1b[H\x1b[?25l");
}

/// Show the cursor again. Pairs with [`enter_screen`] on the way out, so a run
/// never leaves the user's terminal without a cursor.
pub fn leave_screen(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[?25h");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payload_is_single_chunk_with_m0() {
        let mut out = Vec::new();
        // 2x1 RGBA = 8 bytes -> base64 is short, one chunk.
        transmit_and_display_rgba(&mut out, 1, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b_Ga=T,f=32,s=2,v=1,i=1,q=2,m=0;"));
        assert!(s.ends_with("\x1b\\"));
        // exactly one APC packet
        assert_eq!(s.matches("\x1b_G").count(), 1);
    }

    #[test]
    fn large_payload_splits_into_chunks() {
        let mut out = Vec::new();
        // 4096-byte base64 limit; 320x180x4 raw -> ~307KB base64 -> many chunks.
        let rgba = vec![128u8; 320 * 180 * 4];
        transmit_and_display_rgba(&mut out, 1, 320, 180, &rgba);
        let s = String::from_utf8(out).unwrap();
        let packets = s.matches("\x1b_G").count();
        assert!(packets > 1, "expected multiple chunks, got {packets}");
        // first packet carries the control block, exactly one m=0 (the last).
        assert!(s.contains("a=T,f=32,s=320,v=180,i=1,q=2,m=1;"));
        assert_eq!(s.matches("m=0;").count(), 1);
    }

    #[test]
    fn control_only_packet_has_no_payload_sep_chunks() {
        let mut out = Vec::new();
        delete_all_placements(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b_Ga=d,d=a,q=2\x1b\\");
    }

    #[test]
    fn place_scaled_omits_zero_keys() {
        // Zero cols/rows is how the glyph pass asks kitty to draw an image at
        // its natural pixel size. Emitting `c=0` instead would collapse it.
        let mut out = Vec::new();
        place_scaled(&mut out, 7, 9, 42, 0, 0, 3, 4);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b_Ga=p,i=7,p=9,z=42,C=1,q=2,X=3,Y=4\x1b\\");
        assert!(!s.contains("c="));
        assert!(!s.contains("r="));
    }

    #[test]
    fn place_scaled_emits_all_keys_when_set() {
        let mut out = Vec::new();
        place_scaled(&mut out, 1001, 1, 100246, 12, 6, 6, 16);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "\x1b_Ga=p,i=1001,p=1,z=100246,C=1,q=2,c=12,r=6,X=6,Y=16\x1b\\"
        );
    }

    #[test]
    fn negative_z_survives_formatting() {
        // The background sits at a negative z. `z` is i32 on purpose.
        let mut out = Vec::new();
        place(&mut out, 1, 1, -100_000);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("z=-100000"), "got {s:?}");
    }

    #[test]
    fn chunk_boundary_payload_still_ends_with_m0() {
        // A payload landing exactly on the 4096-byte base64 boundary is the
        // classic off-by-one: the last chunk must still be flagged m=0.
        // 3072 raw bytes -> exactly 4096 base64 chars.
        let mut out = Vec::new();
        transmit_rgba(&mut out, 5, 768, 1, &vec![7u8; 3072]);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.matches("\x1b_G").count(), 1, "should be one chunk");
        assert_eq!(s.matches("m=0;").count(), 1);
        assert!(!s.contains("m=1"));
    }

    #[test]
    fn cursor_is_one_based_row_then_col() {
        // Row first, then column: transposing these puts everything on the
        // wrong axis, and the failure looks like a coordinate bug elsewhere.
        let mut out = Vec::new();
        cursor_to(&mut out, 42, 81);
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[42;81H");
    }

    #[test]
    fn delete_placement_targets_one_image_and_placement() {
        let mut out = Vec::new();
        delete_placement(&mut out, 1001, 3);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b_Ga=d,d=i,i=1001,p=3,q=2\x1b\\"
        );
    }

    #[test]
    fn the_exported_introducer_matches_what_is_actually_written() {
        // The point of exporting it: another writer sharing stdout compares against
        // this constant to guarantee its own APC letter does not collide. If the
        // constant drifted from the bytes, that check would pass while the streams
        // collided on a real terminal.
        let mut out = Vec::new();
        transmit_rgba(&mut out, 1, 1, 1, &[0, 0, 0, 255]);
        assert!(out.starts_with(APC_GRAPHICS_INTRODUCER));
        assert!(out.ends_with(APC_TERMINATOR));

        let mut out = Vec::new();
        delete_all_placements(&mut out);
        assert!(out.starts_with(APC_GRAPHICS_INTRODUCER));
        assert!(out.ends_with(APC_TERMINATOR));
    }
}
