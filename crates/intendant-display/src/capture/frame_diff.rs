//! Frame-diff damage tracking for platforms without OS damage events.
//!
//! This is a D-4 fallback behind XDamage / future platform-native
//! damage. It hashes every tile of the captured frame and emits tile
//! rects whose hash changed since the previous frame. That is CPU-bound
//! compared with OS damage, but it keeps the dirty-region architecture
//! usable on platforms whose capture APIs do not expose dirty rects yet.

use super::super::tile::grid::{TileGrid, TileId};
use super::super::{Frame, FrameFormat};
use super::damage::Rect;
use std::collections::HashMap;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

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
    last_hashes: HashMap<TileId, u64>,
}

impl FrameDiffDamageTracker {
    pub fn new(tile_size_px: u16) -> Self {
        Self {
            tile_size_px,
            last_geometry: None,
            last_hashes: HashMap::new(),
        }
    }

    pub fn diff_frame(&mut self, frame: &Frame) -> Result<Vec<Rect>, FrameDiffError> {
        validate_frame(frame)?;
        let grid = TileGrid::new(frame.width, frame.height, self.tile_size_px)
            .ok_or(FrameDiffError::InvalidGeometry)?;
        let geometry = (frame.width, frame.height, frame.stride, frame.format);
        let geometry_changed = self.last_geometry != Some(geometry);
        if geometry_changed {
            self.last_hashes.clear();
            self.last_geometry = Some(geometry);
        }

        let mut dirty = Vec::new();
        for ty in 0..grid.height_tiles {
            for tx in 0..grid.width_tiles {
                let tile = TileId::new(tx, ty);
                let hash = hash_tile(frame, &grid, tile)?;
                let changed = self.last_hashes.get(&tile).is_none_or(|prev| *prev != hash);
                self.last_hashes.insert(tile, hash);
                if changed {
                    dirty.push(tile_rect(&grid, tile));
                }
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

    let mut hash = FNV_OFFSET;
    for y in 0..copy_h {
        let row_start = (start_y as usize + y) * frame.stride as usize + start_x as usize * 4;
        let row = &frame.data[row_start..row_start + copy_w * 4];
        for b in row {
            hash ^= *b as u64;
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
    }
}
