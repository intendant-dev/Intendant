//! Per-tile BGRA/RGBA encoding for dirty-region tile streaming.
//!
//! D-3 kept this deliberately small: fixed-size square tiles encoded
//! as either raw BGRA or a simple BGRA run-length encoding. D-4 adds
//! lossless WebP as a byte-saving option when it beats both simple
//! encodings.

use super::grid::TileId;
use super::transport::{TileEncoding, TileRecord};
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TilePixelFormat {
    Bgra,
    Rgba,
}

#[derive(Clone, Copy, Debug)]
pub struct TileSource<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: TilePixelFormat,
    pub tile_size_px: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileEncodeError {
    InvalidGeometry,
    SourceTooSmall,
    WebpEncode(String),
}

impl std::fmt::Display for TileEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGeometry => write!(f, "invalid tile source geometry"),
            Self::SourceTooSmall => write!(f, "tile source buffer is too small"),
            Self::WebpEncode(e) => write!(f, "lossless WebP tile encode failed: {e}"),
        }
    }
}

impl std::error::Error for TileEncodeError {}

/// Encode one tile. Partial right/bottom edge tiles are padded to a
/// full tile with opaque black pixels so the browser compositor can
/// treat every payload as `tile_size_px * tile_size_px`.
#[allow(dead_code)]
pub fn encode_tile(src: &TileSource<'_>, tile: TileId) -> Result<TileRecord, TileEncodeError> {
    validate_source(src)?;
    let raw = raw_bgra_tile(src, tile)?;
    encode_raw_bgra_payload(tile, raw, src.tile_size_px)
}

pub fn encode_raw_bgra_payload(
    tile: TileId,
    raw: Vec<u8>,
    tile_size_px: u16,
) -> Result<TileRecord, TileEncodeError> {
    let expected = tile_size_px as usize * tile_size_px as usize * 4;
    if tile_size_px == 0 || raw.len() != expected {
        return Err(TileEncodeError::InvalidGeometry);
    }
    let rle = rle_bgra(&raw);

    let (mut encoding, mut payload) = if rle.len() < raw.len() {
        (TileEncoding::RleBgra, rle)
    } else {
        (TileEncoding::RawBgra, raw.clone())
    };

    if should_try_webp_lossless(payload.len(), raw.len()) {
        match webp_lossless_bgra(&raw, tile_size_px as u32, tile_size_px as u32) {
            Ok(webp) if webp.len() < payload.len() => {
                encoding = TileEncoding::WebpLossless;
                payload = webp;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[display/tile] lossless WebP encode skipped: {e}");
            }
        }
    }

    Ok(TileRecord::new(tile.x, tile.y, encoding, payload))
}

pub fn raw_bgra_tile(src: &TileSource<'_>, tile: TileId) -> Result<Vec<u8>, TileEncodeError> {
    validate_source(src)?;
    let ts = src.tile_size_px as usize;
    let mut out = vec![0u8; ts * ts * 4];
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }

    let start_x = tile.x as u32 * src.tile_size_px as u32;
    let start_y = tile.y as u32 * src.tile_size_px as u32;
    let copy_w = src
        .width
        .saturating_sub(start_x)
        .min(src.tile_size_px as u32) as usize;
    let copy_h = src
        .height
        .saturating_sub(start_y)
        .min(src.tile_size_px as u32) as usize;

    for y in 0..copy_h {
        let src_row = (start_y as usize + y) * src.stride as usize;
        let dst_row = y * ts * 4;
        for x in 0..copy_w {
            let si = src_row + (start_x as usize + x) * 4;
            let di = dst_row + x * 4;
            match src.format {
                TilePixelFormat::Bgra => {
                    out[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
                }
                TilePixelFormat::Rgba => {
                    out[di] = src.data[si + 2];
                    out[di + 1] = src.data[si + 1];
                    out[di + 2] = src.data[si];
                    out[di + 3] = src.data[si + 3];
                }
            }
        }
    }

    Ok(out)
}

/// Encode `[B, G, R, A, run_len]` records. `run_len` is 1..=255.
pub fn rle_bgra(raw_bgra: &[u8]) -> Vec<u8> {
    if raw_bgra.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= raw_bgra.len() {
        let px = &raw_bgra[i..i + 4];
        let mut run: u8 = 1;
        while run < u8::MAX {
            let next = i + (run as usize) * 4;
            if next + 4 > raw_bgra.len() || &raw_bgra[next..next + 4] != px {
                break;
            }
            run += 1;
        }
        out.extend_from_slice(px);
        out.push(run);
        i += run as usize * 4;
    }
    out
}

pub fn webp_lossless_bgra(
    raw_bgra: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, TileEncodeError> {
    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(4))
        .ok_or(TileEncodeError::InvalidGeometry)? as usize;
    if expected == 0 || raw_bgra.len() != expected {
        return Err(TileEncodeError::InvalidGeometry);
    }

    let mut rgba = Vec::with_capacity(raw_bgra.len());
    for px in raw_bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }

    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|e| TileEncodeError::WebpEncode(e.to_string()))?;
    Ok(out)
}

fn should_try_webp_lossless(best_len: usize, raw_len: usize) -> bool {
    // Flat UI/background tiles are already tiny after RLE; spending
    // WebP CPU on them is counterproductive. Once the best simple
    // encoding is still a material fraction of raw, lossless WebP can
    // pay for itself on terminal text, icons, and antialiased edges.
    best_len > 1024 && best_len > raw_len / 2
}

fn validate_source(src: &TileSource<'_>) -> Result<(), TileEncodeError> {
    if src.width == 0
        || src.height == 0
        || src.tile_size_px == 0
        || src.stride < src.width.saturating_mul(4)
    {
        return Err(TileEncodeError::InvalidGeometry);
    }
    let needed = (src.height as usize - 1)
        .saturating_mul(src.stride as usize)
        .saturating_add(src.width as usize * 4);
    if src.data.len() < needed {
        return Err(TileEncodeError::SourceTooSmall);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_src(data: &[u8], width: u32, height: u32, stride: u32, tile: u16) -> TileSource<'_> {
        TileSource {
            data,
            width,
            height,
            stride,
            format: TilePixelFormat::Bgra,
            tile_size_px: tile,
        }
    }

    #[test]
    fn raw_tile_copies_bgra_pixels() {
        let data = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let src = bgra_src(&data, 2, 2, 8, 2);
        let raw = raw_bgra_tile(&src, TileId::new(0, 0)).unwrap();
        assert_eq!(raw, data);
    }

    #[test]
    fn raw_tile_swaps_rgba_to_bgra() {
        let data = [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255];
        let src = TileSource {
            data: &data,
            width: 2,
            height: 2,
            stride: 8,
            format: TilePixelFormat::Rgba,
            tile_size_px: 2,
        };
        let raw = raw_bgra_tile(&src, TileId::new(0, 0)).unwrap();
        assert_eq!(
            raw,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255,]
        );
    }

    #[test]
    fn edge_tile_is_padded_opaque_black() {
        let data = [
            1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255, 13, 14, 15, 255, 16, 17, 18,
            255, 19, 20, 21, 255, 22, 23, 24, 255, 25, 26, 27, 255,
        ];
        let src = bgra_src(&data, 3, 3, 12, 2);
        let raw = raw_bgra_tile(&src, TileId::new(1, 1)).unwrap();
        assert_eq!(
            raw,
            vec![25, 26, 27, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,]
        );
    }

    #[test]
    fn rle_compresses_identical_pixels() {
        let raw = [9, 8, 7, 255].repeat(10);
        assert_eq!(rle_bgra(&raw), vec![9, 8, 7, 255, 10]);
    }

    #[test]
    fn encode_tile_chooses_rle_only_when_smaller() {
        let flat = [1, 2, 3, 255].repeat(16);
        let src = bgra_src(&flat, 4, 4, 16, 4);
        let rec = encode_tile(&src, TileId::new(0, 0)).unwrap();
        assert_eq!(rec.encoding, TileEncoding::RleBgra);

        let mut noisy = Vec::new();
        for i in 0..16u8 {
            noisy.extend_from_slice(&[i, i.wrapping_add(1), i.wrapping_add(2), 255]);
        }
        let src = bgra_src(&noisy, 4, 4, 16, 4);
        let rec = encode_tile(&src, TileId::new(0, 0)).unwrap();
        assert_eq!(rec.encoding, TileEncoding::RawBgra);
    }

    #[test]
    fn webp_lossless_bgra_round_trips() {
        let bgra = vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let encoded = webp_lossless_bgra(&bgra, 2, 2).unwrap();
        let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::WebP)
            .unwrap()
            .to_rgba8()
            .into_raw();
        assert_eq!(
            decoded,
            vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255,]
        );
    }

    #[test]
    fn encode_tile_chooses_webp_when_it_beats_simple_encodings() {
        let tile_size = 64u16;
        let mut data = Vec::with_capacity(tile_size as usize * tile_size as usize * 4);
        for y in 0..tile_size {
            for x in 0..tile_size {
                data.extend_from_slice(&[
                    ((x as u32 * 3 + y as u32 * 5) % 256) as u8,
                    ((x as u32 * 7 + y as u32 * 2) % 256) as u8,
                    ((x as u32 + y as u32 * 11) % 256) as u8,
                    255,
                ]);
            }
        }
        let src = bgra_src(
            &data,
            tile_size as u32,
            tile_size as u32,
            tile_size as u32 * 4,
            tile_size,
        );
        let rec = encode_tile(&src, TileId::new(0, 0)).unwrap();
        assert_eq!(rec.encoding, TileEncoding::WebpLossless);
        assert!(rec.payload.len() < data.len());
    }

    #[test]
    fn webp_lossless_rejects_invalid_geometry() {
        assert_eq!(
            webp_lossless_bgra(&[0, 0, 0, 255], 2, 2),
            Err(TileEncodeError::InvalidGeometry),
        );
    }

    #[test]
    fn invalid_source_is_rejected() {
        let data = [0u8; 4];
        let src = bgra_src(&data, 2, 2, 4, 2);
        assert_eq!(
            raw_bgra_tile(&src, TileId::new(0, 0)),
            Err(TileEncodeError::InvalidGeometry)
        );

        let src = bgra_src(&data, 2, 2, 8, 2);
        assert_eq!(
            raw_bgra_tile(&src, TileId::new(0, 0)),
            Err(TileEncodeError::SourceTooSmall)
        );
    }
}
