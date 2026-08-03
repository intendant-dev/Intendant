//! Pixel-truth change detection for tile streaming.
//!
//! Two consumers share the per-tile hash baseline this tracker keeps:
//!
//! - **Frame-diff mode** ([`FrameDiffDamageTracker::diff_frame`]) — the
//!   fallback for platforms without per-frame damage metadata: hash
//!   every tile of the captured frame and emit tile rects whose hash
//!   changed since the previous diff. CPU-bound but works anywhere.
//! - **Damage verification** ([`FrameDiffDamageTracker::verify_damage`])
//!   — platform damage metadata (XDamage events, SCK dirty rects) tells
//!   the pipeline *where to look*, never *what changed*: OS damage
//!   over-reports by design (bounding boxes merge distant damage), and
//!   under a compositing WM root-window XDamage fires on *repaints*,
//!   not pixel change — GNOME Shell's clock tick reports an
//!   up-to-full-root box every second over pixel-identical content.
//!   Verification hashes only the tiles inside the reported rects and
//!   keeps the ones whose content really changed, so a phantom
//!   full-screen report costs one hash pass instead of a full-screen
//!   re-encode (and a video-fallback flap).

use super::super::tile::grid::{TileGrid, TileId};
use super::super::{Frame, FrameFormat};
use super::damage::Rect;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// Clears byte 3 of each of the two packed 4-byte pixels in a
/// little-endian `u64` word. Byte 3 is alpha on alpha-carrying sources
/// and **undefined padding** on 24-bit-depth X11 captures (BGRX/RGBX):
/// the server writes whatever it likes there, so two captures of
/// identical screen content can differ in those bytes — hashing them
/// minted phantom tile dirt. Screens are opaque (no capture source
/// renders alpha variation), so the channel is excluded from change
/// detection everywhere rather than per-backend.
const PIXEL_HASH_MASK: u64 = 0x00FF_FFFF_00FF_FFFF;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDiffError {
    InvalidGeometry,
    SourceTooSmall,
}

impl std::fmt::Display for FrameDiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGeometry => write!(f, "invalid frame-diff geometry"),
            Self::SourceTooSmall => write!(f, "frame-diff source buffer is too small"),
        }
    }
}

impl std::error::Error for FrameDiffError {}

pub struct FrameDiffDamageTracker {
    tile_size_px: u16,
    last_geometry: Option<(u32, u32, u32, FrameFormat)>,
    /// Per-tile hash of the previous frame, indexed
    /// `ty * width_tiles + tx`. A flat vector (not a map): the grid is
    /// dense and this runs for every tile of every diffed frame, so
    /// hashing a `TileId` key per tile would dominate the bookkeeping.
    last_hashes: Vec<u64>,
    /// False until the first full pass after construction or a geometry
    /// change; while false every tile is dirty regardless of its hash.
    has_baseline: bool,
}

impl FrameDiffDamageTracker {
    pub fn new(tile_size_px: u16) -> Self {
        Self {
            tile_size_px,
            last_geometry: None,
            last_hashes: Vec::new(),
            has_baseline: false,
        }
    }

    /// Validate the frame and (re)size per-tile hash state for its
    /// geometry. After a geometry change `self.has_baseline` is false
    /// until a full hash pass completes.
    fn prepare_grid(&mut self, frame: &Frame) -> Result<TileGrid, FrameDiffError> {
        validate_frame(frame)?;
        let grid = TileGrid::new(frame.width, frame.height, self.tile_size_px)
            .ok_or(FrameDiffError::InvalidGeometry)?;
        let geometry = (frame.width, frame.height, frame.stride, frame.format);
        if self.last_geometry != Some(geometry) {
            self.last_hashes.clear();
            self.last_hashes
                .resize(grid.width_tiles as usize * grid.height_tiles as usize, 0);
            self.has_baseline = false;
            self.last_geometry = Some(geometry);
        }
        Ok(grid)
    }

    pub fn diff_frame(&mut self, frame: &Frame) -> Result<Vec<Rect>, FrameDiffError> {
        let grid = self.prepare_grid(frame)?;
        let mut dirty = Vec::new();
        for ty in 0..grid.height_tiles {
            for tx in 0..grid.width_tiles {
                let tile = TileId::new(tx, ty);
                let hash = hash_tile(frame, &grid, tile)?;
                let idx = ty as usize * grid.width_tiles as usize + tx as usize;
                let changed = !self.has_baseline || self.last_hashes[idx] != hash;
                self.last_hashes[idx] = hash;
                if changed {
                    dirty.push(tile_rect(&grid, tile));
                }
            }
        }
        self.has_baseline = true;
        Ok(dirty)
    }

    /// Pixel-verify externally reported damage instead of trusting it.
    ///
    /// `candidates` (OS damage events, in-frame dirty rects) name where
    /// to look; only tiles inside them whose content hash actually
    /// changed since this tracker last saw them are returned. See the
    /// module docs for why reported damage must never be trusted as
    /// dirt directly.
    ///
    /// The first call (and the first after any geometry change) has no
    /// baseline to verify against: it hashes the whole frame to
    /// establish one and returns the candidate tiles unverified —
    /// dropping them instead would strand a real change until the
    /// periodic snapshot re-anchor.
    pub fn verify_damage(
        &mut self,
        frame: &Frame,
        candidates: &[Rect],
    ) -> Result<Vec<Rect>, FrameDiffError> {
        let grid = self.prepare_grid(frame)?;
        let candidate_tiles = grid.dirty_tiles(candidates);
        if !self.has_baseline {
            for ty in 0..grid.height_tiles {
                for tx in 0..grid.width_tiles {
                    let idx = ty as usize * grid.width_tiles as usize + tx as usize;
                    self.last_hashes[idx] = hash_tile(frame, &grid, TileId::new(tx, ty))?;
                }
            }
            self.has_baseline = true;
            return Ok(candidate_tiles
                .into_iter()
                .map(|tile| tile_rect(&grid, tile))
                .collect());
        }
        let mut dirty = Vec::new();
        for tile in candidate_tiles {
            let hash = hash_tile(frame, &grid, tile)?;
            let idx = tile.y as usize * grid.width_tiles as usize + tile.x as usize;
            if self.last_hashes[idx] != hash {
                self.last_hashes[idx] = hash;
                dirty.push(tile_rect(&grid, tile));
            }
        }
        Ok(dirty)
    }
}

fn validate_frame(frame: &Frame) -> Result<(), FrameDiffError> {
    if frame.width == 0 || frame.height == 0 || frame.stride < frame.width.saturating_mul(4) {
        return Err(FrameDiffError::InvalidGeometry);
    }
    let needed = (frame.height as usize - 1)
        .saturating_mul(frame.stride as usize)
        .saturating_add(frame.width as usize * 4);
    if frame.data.len() < needed {
        return Err(FrameDiffError::SourceTooSmall);
    }
    Ok(())
}

fn hash_tile(frame: &Frame, grid: &TileGrid, tile: TileId) -> Result<u64, FrameDiffError> {
    let ts = grid.tile_size_px as u32;
    let start_x = tile.x as u32 * ts;
    let start_y = tile.y as u32 * ts;
    let copy_w = frame.width.saturating_sub(start_x).min(ts) as usize;
    let copy_h = frame.height.saturating_sub(start_y).min(ts) as usize;

    // FNV-1a over 8-byte words instead of single bytes: the multiply is
    // the serial dependency, so folding 8 bytes per round is ~8× fewer
    // dependent multiplies over the same pixels. This changes the hash
    // *values* relative to byte-wise FNV, which is fine — hashes are
    // only ever compared against hashes of the previous frame computed
    // by the same code in the same process. Each word is two packed
    // pixels; [`PIXEL_HASH_MASK`] drops their byte-3 channel (alpha /
    // undefined BGRX padding) from change detection. `from_le_bytes`
    // (not `ne`) keeps the mask ↔ byte-lane pairing fixed regardless of
    // host endianness.
    let mut hash = FNV_OFFSET;
    for y in 0..copy_h {
        let row_start = (start_y as usize + y) * frame.stride as usize + start_x as usize * 4;
        let row = &frame.data[row_start..row_start + copy_w * 4];
        let mut words = row.chunks_exact(8);
        for w in words.by_ref() {
            hash ^= u64::from_le_bytes(w.try_into().expect("chunks_exact(8) yields 8 bytes"))
                & PIXEL_HASH_MASK;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        let rem = words.remainder();
        if !rem.is_empty() {
            // Tail (odd pixel count: rows are always 4-byte multiples,
            // so this is one trailing pixel): zero-pad into one final
            // word — the mask keeps the zeroed upper lane at zero, so
            // rows of equal content still hash equal.
            let mut tail = [0u8; 8];
            tail[..rem.len()].copy_from_slice(rem);
            hash ^= u64::from_le_bytes(tail) & PIXEL_HASH_MASK;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    Ok(hash)
}

fn tile_rect(grid: &TileGrid, tile: TileId) -> Rect {
    let ts = grid.tile_size_px as u32;
    let x = tile.x as u32 * ts;
    let y = tile.y as u32 * ts;
    Rect::new(
        x as i32,
        y as i32,
        grid.screen_w_px.saturating_sub(x).min(ts),
        grid.screen_h_px.saturating_sub(y).min(ts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn frame(data: Vec<u8>, width: u32, height: u32, stride: u32) -> Frame {
        Frame {
            data,
            format: FrameFormat::Bgra,
            width,
            height,
            stride,
            timestamp: Instant::now(),
            dirty_rects: None,
        }
    }

    #[test]
    fn first_frame_marks_every_tile_dirty() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![0; 4 * 4 * 4], 4, 4, 16);
        let dirty = t.diff_frame(&f).unwrap();
        assert_eq!(dirty.len(), 4);
        assert!(dirty.contains(&Rect::new(0, 0, 2, 2)));
        assert!(dirty.contains(&Rect::new(2, 2, 2, 2)));
    }

    #[test]
    fn unchanged_frame_yields_no_dirty_rects() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![7; 4 * 4 * 4], 4, 4, 16);
        assert_eq!(t.diff_frame(&f).unwrap().len(), 4);
        assert!(t.diff_frame(&f).unwrap().is_empty());
    }

    /// X11 24-bit-depth captures are BGRX: byte 3 of every pixel is
    /// *undefined padding* the server may fill differently between two
    /// captures of identical screen content. Change detection must not
    /// read it — two frames whose B/G/R planes match are the same frame.
    #[test]
    fn padding_byte_only_change_yields_no_dirty_rects() {
        let mut t = FrameDiffDamageTracker::new(2);
        let mut data = vec![7u8; 4 * 4 * 4];
        for px in data.chunks_exact_mut(4) {
            px[3] = 0xAA;
        }
        assert_eq!(
            t.diff_frame(&frame(data.clone(), 4, 4, 16)).unwrap().len(),
            4
        );
        for px in data.chunks_exact_mut(4) {
            px[3] = 0x55;
        }
        assert!(
            t.diff_frame(&frame(data, 4, 4, 16)).unwrap().is_empty(),
            "pad-byte-only differences must not mint dirty tiles"
        );
    }

    #[test]
    fn one_pixel_change_marks_owning_tile_only() {
        let mut t = FrameDiffDamageTracker::new(2);
        let mut data = vec![0; 4 * 4 * 4];
        let f = frame(data.clone(), 4, 4, 16);
        let _ = t.diff_frame(&f).unwrap();

        // Pixel at (3, 1) belongs to tile (1, 0).
        let idx = 1 * 16 + 3 * 4;
        data[idx] = 255;
        let dirty = t.diff_frame(&frame(data, 4, 4, 16)).unwrap();
        assert_eq!(dirty, vec![Rect::new(2, 0, 2, 2)]);
    }

    #[test]
    fn edge_tiles_are_clipped_to_screen() {
        let mut t = FrameDiffDamageTracker::new(3);
        let dirty = t.diff_frame(&frame(vec![1; 5 * 4 * 4], 5, 4, 20)).unwrap();
        assert!(dirty.contains(&Rect::new(3, 0, 2, 3)));
        assert!(dirty.contains(&Rect::new(3, 3, 2, 1)));
    }

    #[test]
    fn geometry_change_resets_hash_baseline() {
        let mut t = FrameDiffDamageTracker::new(2);
        let _ = t.diff_frame(&frame(vec![0; 4 * 4 * 4], 4, 4, 16)).unwrap();
        let dirty = t.diff_frame(&frame(vec![0; 6 * 4 * 4], 6, 4, 24)).unwrap();
        assert_eq!(dirty.len(), 6);
    }

    #[test]
    fn invalid_frame_is_rejected() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![0; 4], 2, 2, 4);
        assert_eq!(t.diff_frame(&f), Err(FrameDiffError::InvalidGeometry));
        let f = frame(vec![0; 4], 2, 2, 8);
        assert_eq!(t.diff_frame(&f), Err(FrameDiffError::SourceTooSmall));
        assert_eq!(
            t.verify_damage(&frame(vec![0; 4], 2, 2, 4), &[Rect::new(0, 0, 2, 2)]),
            Err(FrameDiffError::InvalidGeometry)
        );
    }

    const FULL_4X4: Rect = Rect {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };

    #[test]
    fn verify_damage_first_pass_trusts_candidates_and_baselines() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![9; 4 * 4 * 4], 4, 4, 16);
        // No baseline yet: the reported damage is passed through once…
        assert_eq!(t.verify_damage(&f, &[FULL_4X4]).unwrap().len(), 4);
        // …and the very same phantom report over identical pixels is
        // pruned on every later pass (the live X11 compositor shape).
        assert!(t.verify_damage(&f, &[FULL_4X4]).unwrap().is_empty());
        assert!(t.verify_damage(&f, &[FULL_4X4]).unwrap().is_empty());
    }

    #[test]
    fn verify_damage_confirms_only_actually_changed_tiles() {
        let mut t = FrameDiffDamageTracker::new(2);
        let mut data = vec![0u8; 4 * 4 * 4];
        let _ = t
            .verify_damage(&frame(data.clone(), 4, 4, 16), &[FULL_4X4])
            .unwrap();
        // Pixel at (3, 1) belongs to tile (1, 0); the OS still shouts
        // "everything changed".
        data[16 + 3 * 4] = 255;
        let dirty = t
            .verify_damage(&frame(data, 4, 4, 16), &[FULL_4X4])
            .unwrap();
        assert_eq!(dirty, vec![Rect::new(2, 0, 2, 2)]);
    }

    #[test]
    fn verify_damage_ignores_padding_byte_changes() {
        let mut t = FrameDiffDamageTracker::new(2);
        let mut data = vec![3u8; 4 * 4 * 4];
        let _ = t
            .verify_damage(&frame(data.clone(), 4, 4, 16), &[FULL_4X4])
            .unwrap();
        for px in data.chunks_exact_mut(4) {
            px[3] = 0xEE;
        }
        assert!(t
            .verify_damage(&frame(data, 4, 4, 16), &[FULL_4X4])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn verify_damage_outside_candidates_stays_stale_until_claimed() {
        let mut t = FrameDiffDamageTracker::new(2);
        let mut data = vec![0u8; 4 * 4 * 4];
        let _ = t
            .verify_damage(&frame(data.clone(), 4, 4, 16), &[FULL_4X4])
            .unwrap();
        // Pixel (0, 2) → tile (0, 1) changes, but damage only claims
        // tile (1, 0): nothing to report this tick…
        data[2 * 16] = 200;
        let f = frame(data, 4, 4, 16);
        assert!(t
            .verify_damage(&f, &[Rect::new(2, 0, 2, 2)])
            .unwrap()
            .is_empty());
        // …and the unclaimed tile's baseline did not advance, so the
        // change is reported the moment damage finally covers it, and
        // exactly once.
        assert_eq!(
            t.verify_damage(&f, &[Rect::new(0, 2, 2, 2)]).unwrap(),
            vec![Rect::new(0, 2, 2, 2)]
        );
        assert!(t
            .verify_damage(&f, &[Rect::new(0, 2, 2, 2)])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn verify_damage_reports_reverted_content() {
        let mut t = FrameDiffDamageTracker::new(2);
        let clean = vec![0u8; 4 * 4 * 4];
        let mut inked = clean.clone();
        inked[0] = 255;
        let _ = t
            .verify_damage(&frame(clean.clone(), 4, 4, 16), &[FULL_4X4])
            .unwrap();
        assert_eq!(
            t.verify_damage(&frame(inked, 4, 4, 16), &[FULL_4X4])
                .unwrap()
                .len(),
            1
        );
        // Reverting to the original content is a change from the
        // last-verified state and must repaint.
        assert_eq!(
            t.verify_damage(&frame(clean, 4, 4, 16), &[FULL_4X4])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn verify_damage_geometry_change_rebaselines() {
        let mut t = FrameDiffDamageTracker::new(2);
        let _ = t
            .verify_damage(&frame(vec![0; 4 * 4 * 4], 4, 4, 16), &[FULL_4X4])
            .unwrap();
        // New geometry: trust pass again, then verified.
        let f6 = frame(vec![0; 6 * 4 * 4], 6, 4, 24);
        let full6 = Rect::new(0, 0, 6, 4);
        assert_eq!(t.verify_damage(&f6, &[full6]).unwrap().len(), 6);
        assert!(t.verify_damage(&f6, &[full6]).unwrap().is_empty());
    }

    #[test]
    fn verify_damage_empty_candidates_report_nothing() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![1; 4 * 4 * 4], 4, 4, 16);
        assert!(t.verify_damage(&f, &[]).unwrap().is_empty());
        assert!(t.verify_damage(&f, &[]).unwrap().is_empty());
    }

    #[test]
    fn verify_and_full_diff_share_one_baseline() {
        let mut t = FrameDiffDamageTracker::new(2);
        let f = frame(vec![3; 4 * 4 * 4], 4, 4, 16);
        // A full diff establishes the baseline…
        assert_eq!(t.diff_frame(&f).unwrap().len(), 4);
        // …so verification needs no trust pass of its own…
        assert!(t.verify_damage(&f, &[FULL_4X4]).unwrap().is_empty());
        // …and per-frame mode mixing (the macOS per-frame degrade
        // path) keeps one coherent baseline in both directions.
        assert!(t.diff_frame(&f).unwrap().is_empty());
    }
}
