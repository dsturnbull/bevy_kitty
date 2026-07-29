//! Terminal geometry: how big the terminal is, and how the virtual world maps
//! into it.
//!
//! Two types, and the split matters. [`TermSize`] is what the terminal reports
//! (cells, and pixels if we can get them). [`FitBox`] is the derived mapping from
//! virtual pixels to terminal cells, which every placement goes through.

use bevy::log::{info, warn};
use bevy::math::{UVec2, Vec2};

/// Terminal geometry: cells and (if the terminal reports it) pixels. Queried via
/// `ioctl(TIOCGWINSZ)` so it works over SSH without a stdin round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// Cached result of the one-shot `CSI 14 t` pixel-size query.
///
/// Only ever query once per process. [`TermSize::query`] re-runs periodically to
/// catch window resizes, but this round trip briefly puts the tty in raw mode and
/// reads from it, which would race an input reader and could swallow a keystroke.
/// The cell SIZE does not change while the process runs (only the grid does), so
/// one query is enough: a resize changes cols/rows, and we keep using the cell
/// size measured here.
static CSI_PIXEL_SIZE: std::sync::OnceLock<Option<(u16, u16)>> = std::sync::OnceLock::new();

/// Cell size in pixels as measured by the one-shot CSI query, if it worked.
fn csi_cell_size(cols: u16, rows: u16) -> Option<(f32, f32)> {
    let (w, h) = (*CSI_PIXEL_SIZE.get_or_init(query_pixel_size_via_csi))?;
    if cols == 0 || rows == 0 || w == 0 || h == 0 {
        return None;
    }
    Some((w as f32 / cols as f32, h as f32 / rows as f32))
}

/// Ask the terminal for its pixel size with `CSI 14 t`, reading the reply
/// `CSI 4 ; <height> ; <width> t` from the tty. Returns `(width, height)`.
///
/// Needed because an SSH pty reports the cell grid but not the pixel size, and
/// guessing a cell size gets the aspect wrong. This is the fallback kitty's own
/// documentation recommends.
///
/// Opens `/dev/tty` directly rather than using stdin/stdout: stdout may be a
/// pipe, and the input reader owns stdin. Puts the tty in raw mode only for the
/// round trip, restores it after, and gives up after a short timeout so a
/// terminal that does not answer costs a moment, not a hang.
fn query_pixel_size_via_csi() -> Option<(u16, u16)> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();

    // Save the termios so we can put the terminal back exactly as we found it.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return None;
    }
    let mut raw = saved;
    // Raw enough to read the reply without the line discipline eating it, and
    // with a 300ms read timeout (VTIME is in tenths of a second).
    unsafe { libc::cfmakeraw(&mut raw) };
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 3;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    let result = (|| {
        tty.write_all(b"\x1b[14t").ok()?;
        tty.flush().ok()?;
        // Reply is short; read until 't' or the timeout expires.
        let mut buf = Vec::with_capacity(32);
        let mut byte = [0u8; 1];
        for _ in 0..64 {
            match tty.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if byte[0] == b't' {
                        break;
                    }
                }
                // 0 bytes means the read timed out.
                _ => break,
            }
        }
        let s = String::from_utf8_lossy(&buf);
        parse_csi_14t_reply(&s)
    })();

    // Always restore, even if the query failed.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    result
}

/// Parse a `CSI 4 ; <height> ; <width> t` reply into `(width, height)`.
///
/// Split out from the I/O so it can be tested without a terminal. Note the reply
/// carries HEIGHT before WIDTH, which is the opposite of every other size in this
/// crate: getting it backwards swaps the aspect and misplaces everything.
fn parse_csi_14t_reply(s: &str) -> Option<(u16, u16)> {
    let body = s.trim_start_matches('\x1b').trim_start_matches('[');
    let body = body.strip_suffix('t').unwrap_or(body);
    let mut parts = body.split(';');
    if parts.next()? != "4" {
        return None;
    }
    let h: u16 = parts.next()?.trim().parse().ok()?;
    let w: u16 = parts.next()?.trim().parse().ok()?;
    Some((w, h))
}

impl TermSize {
    /// Query the controlling terminal. Falls back to a sane 80x24 / 640x384 if
    /// the ioctl fails or the terminal does not report a pixel size (and logs
    /// loudly, because a wrong size is not visibly a size problem).
    ///
    /// `forced` skips the query entirely. Pass it when stdout is a pipe (there is
    /// no tty to ask) or to pin a size for testing.
    pub fn query(forced: Option<TermSize>) -> Self {
        if let Some(ts) = forced {
            return ts;
        }
        // SAFETY: winsize is POD; ioctl fills it. We check the return code.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            let mut xpixel = ws.ws_xpixel;
            let mut ypixel = ws.ws_ypixel;
            if xpixel == 0 || ypixel == 0 {
                // Over SSH the pty carries the cell GRID but usually not the
                // pixel size, so ask the terminal itself: `CSI 14 t`, and it
                // replies `CSI 4 ; height ; width t`. kitty's own docs recommend
                // this exact fallback. Guessing a cell size instead gets the
                // ASPECT wrong (assumed 8x16 = 1:2 vs kitty's ~9x21 = 1:2.3),
                // which misplaces every glyph and makes text overlap.
                //
                // Cell size is measured once; multiplying by the CURRENT grid
                // means a window resize is still handled correctly.
                match csi_cell_size(ws.ws_col, ws.ws_row) {
                    Some((cw, chh)) => {
                        let w = (ws.ws_col as f32 * cw).round() as u16;
                        let h = (ws.ws_row as f32 * chh).round() as u16;
                        info!(
                            "[kitty] pixel size via CSI 14t: cell {cw:.2}x{chh:.2} -> {w}x{h} \
                             (grid {}x{})",
                            ws.ws_col, ws.ws_row
                        );
                        xpixel = w;
                        ypixel = h;
                    }
                    _ => {
                        warn!(
                            "[kitty] terminal reported no pixel size (cols={}, rows={}) and \
                             CSI 14t got no answer; assuming 8x16 px cells, so text may be \
                             mis-sized. Set KittyConfig::terminal_size to fix.",
                            ws.ws_col, ws.ws_row
                        );
                        xpixel = ws.ws_col.saturating_mul(8);
                        ypixel = ws.ws_row.saturating_mul(16);
                    }
                }
            }
            return TermSize {
                cols: ws.ws_col,
                rows: ws.ws_row,
                xpixel,
                ypixel,
            };
        }
        warn!("[kitty] TIOCGWINSZ failed (rc={rc}); falling back to 80x24 @ 640x384");
        TermSize {
            cols: 80,
            rows: 24,
            xpixel: 640,
            ypixel: 384,
        }
    }

    /// Parse `colsxrows@wxh` (e.g. `212x51@1920x1080`) into a `TermSize`.
    ///
    /// Kept in the crate rather than left to callers because the format appears
    /// in this crate's own docs and test recipes.
    pub fn parse_spec(spec: &str) -> Option<Self> {
        let (grid, px) = spec.split_once('@')?;
        let (cols, rows) = grid.split_once('x')?;
        let (w, h) = px.split_once('x')?;
        Some(TermSize {
            cols: cols.trim().parse().ok()?,
            rows: rows.trim().parse().ok()?,
            xpixel: w.trim().parse().ok()?,
            ypixel: h.trim().parse().ok()?,
        })
    }

    /// Pixels per cell (integer, floored). Guards against divide-by-zero.
    fn cell_px(&self) -> (u32, u32) {
        let cw = if self.cols > 0 {
            self.xpixel as u32 / self.cols as u32
        } else {
            8
        };
        let ch = if self.rows > 0 {
            self.ypixel as u32 / self.rows as u32
        } else {
            16
        };
        (cw.max(1), ch.max(1))
    }
}

/// The letterboxed viewport the virtual world maps into, in terminal PIXELS: a
/// FRACTIONAL scale factor plus a pixel origin (padding to centre).
///
/// Fractional (not integer) scale so the game fills as much of the terminal as
/// possible while preserving aspect. Integer scaling left the game tiny in a big
/// window (up to a whole game-height wasted). Kitty scales images on the GPU, so
/// a fractional zoom costs nothing and pixel art still reads fine. Aspect is
/// preserved (same scale on both axes), so nothing is distorted.
#[derive(Clone, Copy, Debug)]
pub struct FitBox {
    /// Virtual pixels -> terminal pixels multiplier (fractional).
    pub scale: f32,
    /// Left padding, terminal px.
    pub ox: f32,
    /// Top padding, terminal px.
    pub oy: f32,
    /// Terminal px per cell, horizontally. NOT equal to `cell_h`.
    pub cell_w: f32,
    /// Terminal px per cell, vertically.
    pub cell_h: f32,
    /// The virtual resolution being mapped.
    pub virtual_size: UVec2,
}

impl FitBox {
    /// Aspect-fit (contain) `virtual_size` into the terminal at the largest
    /// fractional scale that fits, centred with letterbox padding. Never
    /// distorts.
    pub fn compute(term: &TermSize, virtual_size: UVec2) -> Self {
        let (cell_w, cell_h) = term.cell_px();
        // Fit into the CELL-ALIGNED area, not the raw pixel count.
        //
        // Cell size is a floored integer (1920/212 = 9.06 -> 9), so the grid
        // covers 212*9 = 1908 px, not 1920. Scaling against the raw 1920 put the
        // far edge of the world at column 213 of a 212-column grid: kitty then
        // clamps or wraps, and the rightmost sliver of the scene is drawn in the
        // wrong place. Costs a fraction of a percent of scale (5.95x instead of
        // 6.0x here) to guarantee every placement lands on a cell that exists.
        let avail_w = (term.cols as u32 * cell_w) as f32;
        let avail_h = (term.rows as u32 * cell_h) as f32;
        let vw = virtual_size.x.max(1) as f32;
        let vh = virtual_size.y.max(1) as f32;
        let scale = (avail_w / vw).min(avail_h / vh).max(0.01);
        let used_w = vw * scale;
        let used_h = vh * scale;
        let ox = (avail_w - used_w).max(0.0) / 2.0;
        let oy = (avail_h - used_h).max(0.0) / 2.0;
        FitBox {
            scale,
            ox,
            oy,
            cell_w: cell_w as f32,
            cell_h: cell_h as f32,
            virtual_size,
        }
    }

    /// Map a viewport pixel (top-left origin, in `virtual_size` space) to a
    /// 1-based terminal (row, col) plus the sub-cell (X,Y) pixel offset kitty
    /// wants.
    pub fn map(&self, vx: f32, vy: f32) -> (u16, u16, u32, u32) {
        let px = self.ox + vx * self.scale;
        let py = self.oy + vy * self.scale;
        let col = (px / self.cell_w).floor().max(0.0);
        let row = (py / self.cell_h).floor().max(0.0);
        let xoff = (px - col * self.cell_w).max(0.0) as u32;
        let yoff = (py - row * self.cell_h).max(0.0) as u32;
        // kitty cells are 1-based.
        ((row as u32 + 1) as u16, (col as u32 + 1) as u16, xoff, yoff)
    }

    /// How many terminal cells a `w`x`h` virtual-pixel image spans after scaling.
    pub fn span_cells(&self, w: u32, h: u32) -> (u32, u32) {
        let pw = w as f32 * self.scale;
        let ph = h as f32 * self.scale;
        (
            (pw / self.cell_w).ceil().max(1.0) as u32,
            (ph / self.cell_h).ceil().max(1.0) as u32,
        )
    }

    /// Reverse of [`FitBox::map`]: a 1-based terminal (col, row) cell back to a
    /// viewport position (cell centre). Returns `None` if the cell is in the
    /// letterbox padding, so a click outside the world is never fabricated into
    /// one inside it.
    pub fn cell_to_viewport(&self, col: u16, row: u16) -> Option<Vec2> {
        // Cell centre in terminal pixels (col/row are 1-based).
        let px = (col as f32 - 1.0 + 0.5) * self.cell_w;
        let py = (row as f32 - 1.0 + 0.5) * self.cell_h;
        // Undo the letterbox origin + scale to get viewport coords.
        let vx = (px - self.ox) / self.scale;
        let vy = (py - self.oy) / self.scale;
        if vx < 0.0
            || vy < 0.0
            || vx >= self.virtual_size.x as f32
            || vy >= self.virtual_size.y as f32
        {
            return None; // clicked the letterbox, not the game
        }
        Some(Vec2::new(vx, vy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured real terminal from the docs: 1920x1080 over a 212x51 grid.
    fn real_term() -> TermSize {
        TermSize {
            cols: 212,
            rows: 51,
            xpixel: 1920,
            ypixel: 1080,
        }
    }

    const V: UVec2 = UVec2::new(320, 180);

    #[test]
    fn parses_a_full_spec() {
        let ts = TermSize::parse_spec("212x51@1920x1080").unwrap();
        assert_eq!(ts, real_term());
    }

    #[test]
    fn rejects_malformed_specs() {
        assert!(TermSize::parse_spec("212x51").is_none());
        assert!(TermSize::parse_spec("212@1920x1080").is_none());
        assert!(TermSize::parse_spec("").is_none());
        assert!(TermSize::parse_spec("axb@cxd").is_none());
    }

    #[test]
    fn csi_reply_is_height_then_width() {
        // The one ordering trap in the whole query path.
        assert_eq!(parse_csi_14t_reply("\x1b[4;1080;1920t"), Some((1920, 1080)));
        assert_eq!(parse_csi_14t_reply("4;1080;1920"), Some((1920, 1080)));
        // A response to a different query must not be read as a size.
        assert_eq!(parse_csi_14t_reply("\x1b[6;51;212t"), None);
        assert_eq!(parse_csi_14t_reply(""), None);
    }

    #[test]
    fn fit_is_fractional_and_preserves_aspect() {
        let fit = FitBox::compute(&real_term(), V);
        // Cells are NOT square: 1920/212 = 9.06, 1080/51 = 21.18. The integer
        // floor is what kitty's c/r keys quantise to.
        assert_eq!(fit.cell_w, 9.0);
        assert_eq!(fit.cell_h, 21.0);
        // Fits the cell-aligned area (212*9 = 1908 by 51*21 = 1071), not the raw
        // 1920x1080: min(1908/320, 1071/180) = min(5.9625, 5.95) = 5.95.
        assert!((fit.scale - 5.95).abs() < 1e-4, "scale {}", fit.scale);
        // Fractional, not floored to an integer. Integer scaling wasted up to a
        // whole game-height of a big terminal.
        assert!(fit.scale.fract() > 0.0, "scale should be fractional");
    }

    #[test]
    fn the_far_edge_of_the_world_lands_on_a_cell_that_exists() {
        // The bug this guards: cell size is FLOORED, so scaling against the raw
        // pixel count put the bottom-right of the scene at cell (213,52) of a
        // 212x51 grid. Off-grid placements are drawn in the wrong place.
        let term = real_term();
        let fit = FitBox::compute(&term, V);
        let (row, col, _, _) = fit.map(V.x as f32 - 1.0, V.y as f32 - 1.0);
        assert!(
            col <= term.cols,
            "col {col} exceeds the {}-column grid",
            term.cols
        );
        assert!(
            row <= term.rows,
            "row {row} exceeds the {}-row grid",
            term.rows
        );
    }

    #[test]
    fn fit_letterboxes_the_narrower_axis() {
        // A 4:3 terminal fitting a 16:9 world: bars top and bottom, none at the
        // sides. 800/100 = 8 and 600/50 = 12 divide exactly, so the cell-aligned
        // area is the full 800x600 and the scale is a clean min(2.5, 3.33) = 2.5.
        let term = TermSize {
            cols: 100,
            rows: 50,
            xpixel: 800,
            ypixel: 600,
        };
        let fit = FitBox::compute(&term, V);
        assert!((fit.scale - 2.5).abs() < 1e-4, "scale {}", fit.scale);
        assert_eq!(fit.ox, 0.0);
        assert!(fit.oy > 0.0, "expected vertical letterbox, got {}", fit.oy);
    }

    #[test]
    fn map_then_cell_to_viewport_round_trips_within_a_cell() {
        let fit = FitBox::compute(&real_term(), V);
        // Any point inside the world maps to a cell that maps back to within one
        // cell's worth of virtual pixels. Exact equality is impossible: `map`
        // quantises to cells, so the reverse lands at the cell CENTRE.
        let cell_vx = fit.cell_w / fit.scale;
        let cell_vy = fit.cell_h / fit.scale;
        for (vx, vy) in [(0.0, 0.0), (160.0, 90.0), (319.0, 179.0), (7.5, 133.25)] {
            let (row, col, _, _) = fit.map(vx, vy);
            let back = fit.cell_to_viewport(col, row).unwrap_or_else(|| {
                panic!("({vx},{vy}) -> cell ({col},{row}) mapped outside world")
            });
            assert!(
                (back.x - vx).abs() <= cell_vx && (back.y - vy).abs() <= cell_vy,
                "({vx},{vy}) round-tripped to ({},{}) via cell ({col},{row})",
                back.x,
                back.y
            );
        }
    }

    #[test]
    fn clicks_in_the_letterbox_are_rejected() {
        // The whole point of the Option: a click on a black bar must not be
        // fabricated into a click at the world edge.
        let term = TermSize {
            cols: 100,
            rows: 50,
            xpixel: 800,
            ypixel: 600,
        };
        let fit = FitBox::compute(&term, V);
        // Top row is padding (oy > 0 with scale 2.5 => 75px of bar, ~6 rows).
        assert!(fit.cell_to_viewport(50, 1).is_none());
        // Middle of the screen is inside the world.
        assert!(fit.cell_to_viewport(50, 25).is_some());
        // Bottom row is padding again.
        assert!(fit.cell_to_viewport(50, 50).is_none());
    }

    #[test]
    fn map_is_one_based() {
        // The virtual origin is terminal cell (1,1), not (0,0). Off-by-one here
        // shifts the entire scene up and left by a cell.
        let fit = FitBox::compute(&real_term(), V);
        let (row, col, xoff, yoff) = fit.map(0.0, 0.0);
        assert_eq!((row, col), (1, 1));
        // A sub-cell offset at the origin is legitimate: it is the letterbox
        // padding. It just has to stay inside the first cell, or the scene starts
        // in a cell we did not move the cursor to.
        assert!(
            (xoff as f32) < fit.cell_w,
            "xoff {xoff} escapes the first cell"
        );
        assert!(
            (yoff as f32) < fit.cell_h,
            "yoff {yoff} escapes the first cell"
        );
    }

    #[test]
    fn an_exactly_divisible_terminal_has_no_padding_at_the_origin() {
        // 640x360 over 80x24 cells (8x15) fitting 320x180: scale 2.0 exactly, and
        // 16:9 into 16:9 leaves no letterbox at all.
        let term = TermSize {
            cols: 80,
            rows: 24,
            xpixel: 640,
            ypixel: 360,
        };
        let fit = FitBox::compute(&term, V);
        assert!((fit.scale - 2.0).abs() < 1e-4, "scale {}", fit.scale);
        assert_eq!(fit.ox, 0.0);
        let (row, col, xoff, yoff) = fit.map(0.0, 0.0);
        assert_eq!((row, col, xoff), (1, 1, 0));
        // Vertical padding remains: 24 rows of 15px is 360, and 180*2 = 360, so
        // none here either.
        assert_eq!(yoff, 0);
    }

    #[test]
    fn span_cells_never_returns_zero() {
        // A sub-cell sprite still has to occupy one cell, or kitty draws nothing.
        let fit = FitBox::compute(&real_term(), V);
        assert_eq!(fit.span_cells(0, 0), (1, 1));
        let (c, r) = fit.span_cells(64, 64);
        assert!(c >= 1 && r >= 1, "{c}x{r}");
    }

    #[test]
    fn degenerate_virtual_size_does_not_divide_by_zero() {
        let fit = FitBox::compute(&real_term(), UVec2::ZERO);
        assert!(
            fit.scale.is_finite() && fit.scale > 0.0,
            "scale {}",
            fit.scale
        );
    }
}
