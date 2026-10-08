//! Patterns ⇄ PSD: the global `Patt`/`Pat2`/`Pat3` blocks and `.pat` files.
//!
//! Import parses the blocks into [`Document::patterns`] and keeps the raw blocks. Export reuses
//! the raw blocks verbatim while they still decode to the document's patterns (byte-exact round
//! trips); otherwise they are replaced by one block written from the document.

use std::sync::Arc;

use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Pattern};
use photocraft_geom::Rect;
use photocraft_psd::patterns::{PsdPattern, block_key, parse_pattern_block, write_pattern_block};
use photocraft_raster::Surface;

use crate::pixels::{deinterleave, interleave, max_sample, psd_depth, sample_for_depth, zero_sample};

/// Global block keys holding patterns.
pub const PATTERN_KEYS: [[u8; 4]; 3] = [*b"Patt", *b"Pat2", *b"Pat3"];

fn psd_mode(m: ColorMode) -> u32 {
    match m {
        ColorMode::Grayscale => 1,
        ColorMode::Rgb => 3,
    }
}

/// A PSD pattern as a document pattern: indexed patterns become RGB through their palette, CMYK
/// and Lab ones through [`crate::native::Native`].
pub fn from_psd(p: &PsdPattern) -> Option<Pattern> {
    let (w, h) = (p.width as usize, p.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    // 1-bit patterns: expand to 8-bit grey.
    let (channels, depth): (Vec<Vec<u8>>, u16) = if p.depth == 1 {
        let row = w.div_ceil(8);
        let expand = |plane: &Vec<u8>| -> Vec<u8> {
            (0..w * h).map(|i| if plane.get((i / w) * row + (i % w) / 8).is_some_and(|b| b & (0x80 >> (i % w % 8)) != 0) { 0 } else { 255 }).collect()
        };
        (p.channels.iter().map(expand).collect(), 8)
    } else {
        (p.channels.clone(), p.depth)
    };
    let native = match p.mode {
        4 => Some(crate::native::Native::cmyk(None)),
        9 => Some(crate::native::Native::Lab),
        _ => None,
    };
    let (mode, channels) = match p.mode {
        1 | 8 => (ColorMode::Grayscale, channels),
        2 => {
            let pal = p.palette.as_deref()?;
            let idx = channels.first()?;
            let ch: Vec<Vec<u8>> = (0..3).map(|c| idx.iter().map(|i| pal.get(usize::from(*i) * 3 + c).copied().unwrap_or(0)).collect()).collect();
            (ColorMode::Rgb, ch)
        }
        3 | 4 | 9 => (ColorMode::Rgb, channels),
        _ => (ColorMode::Grayscale, channels.into_iter().take(1).collect()),
    };
    let s = sample_for_depth(depth);
    let fmt = PixelFormat::new(mode, s, p.alpha.is_some());
    let cc = native.as_ref().map_or(mode.color_channels(), crate::native::Native::channels);
    if channels.len() < cc {
        return None;
    }
    let mut refs: Vec<Option<&[u8]>> = channels.iter().take(cc).map(|c| Some(c.as_slice())).collect();
    let mut fill = vec![zero_sample(s); cc];
    let mut invert = vec![p.mode == 4; cc];
    if let Some(a) = &p.alpha {
        refs.push(Some(a.as_slice()));
        fill.push(max_sample(s));
        invert.push(false);
    }
    let mut bytes = interleave(&refs, &fill, w * h, s, &invert);
    let rect = Rect::new(0, 0, w as i32, h as i32);
    let surface = match &native {
        Some(native) => {
            if p.mode == 9 && s == SampleType::U16 {
                crate::pixels::lab16_chroma(&mut bytes, refs.len(), true);
            }
            let mut out = Surface::new(fmt);
            out.write_region(rect, &native.to_rgb(&bytes, s, p.alpha.is_some()));
            out
        }
        None => Surface::from_interleaved(fmt, rect, &bytes),
    };
    Some(Pattern { id: p.id.clone(), name: p.name.clone(), width: p.width, height: p.height, surface })
}

/// A document pattern as a PSD pattern (its own depth and colour model; other models as RGB).
pub fn to_psd(p: &Pattern) -> PsdPattern {
    let f = p.surface.format();
    let mode = f.mode;
    // PSD has no half float: half-float patterns are stored as 32-bit float (lossless).
    let sample = if f.sample == SampleType::F16 { SampleType::F32 } else { f.sample };
    let fmt = PixelFormat::new(mode, sample, f.alpha);
    let surf = if fmt == f { p.surface.clone() } else { p.surface.convert(fmt) };
    let bytes = surf.to_interleaved(p.rect());
    let ch = fmt.channels();
    let invert = vec![false; ch];
    let mut planes = deinterleave(&bytes, ch, fmt.sample, &invert);
    let alpha = fmt.alpha.then(|| planes.pop()).flatten();
    PsdPattern {
        mode: psd_mode(mode),
        width: p.width,
        height: p.height,
        name: p.name.clone(),
        id: p.id.clone(),
        palette: None,
        depth: psd_depth(fmt.sample),
        channels: planes,
        alpha,
    }
}

/// Patterns decoded from the document's raw pattern blocks (malformed blocks are skipped).
pub fn from_global_blocks(doc: &Document) -> Vec<Pattern> {
    doc.metadata
        .psd_global_blocks
        .iter()
        .filter(|(_, k, _)| PATTERN_KEYS.contains(k))
        .filter_map(|(_, _, d)| parse_pattern_block(d).ok())
        .flatten()
        .filter_map(|p| from_psd(&p))
        .collect()
}

/// Global blocks to write: the raw ones while they still match [`Document::patterns`], else the
/// raw pattern blocks replaced by one written from the document (at the first one's position).
pub fn export_global_blocks(doc: &Document) -> Vec<photocraft_doc::PsdGlobalBlock> {
    let raw = &doc.metadata.psd_global_blocks;
    if from_global_blocks(doc) == doc.patterns {
        return raw.clone();
    }
    let key = block_key(psd_depth(doc.depth));
    let fresh = write_pattern_block(&doc.patterns.iter().map(to_psd).collect::<Vec<_>>()).unwrap_or_default();
    let mut out = Vec::with_capacity(raw.len() + 1);
    let mut placed = false;
    for b in raw {
        if PATTERN_KEYS.contains(&b.1) {
            if !placed {
                out.push((*b"8BIM", key, Arc::new(fresh.clone())));
                placed = true;
            }
        } else {
            out.push(b.clone());
        }
    }
    if !placed && !doc.patterns.is_empty() {
        out.push((*b"8BIM", key, Arc::new(fresh)));
    }
    out
}

/// Reads a `.pat` pattern file.
pub fn read_pat(bytes: &[u8]) -> Result<Vec<Pattern>, String> {
    let ps = photocraft_psd::patterns::parse_pat_file(bytes).map_err(|e| format!("not a readable .pat file: {e}"))?;
    Ok(ps.iter().filter_map(from_psd).collect())
}

/// Writes patterns as a `.pat` file.
pub fn write_pat(patterns: &[Pattern]) -> Result<Vec<u8>, String> {
    photocraft_psd::patterns::write_pat_file(&patterns.iter().map(to_psd).collect::<Vec<_>>()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::SampleType;

    fn pattern(mode: ColorMode, s: SampleType, alpha: bool) -> Pattern {
        let fmt = PixelFormat::new(mode, s, alpha);
        let mut surf = Surface::new(fmt);
        let n = fmt.channels();
        for y in 0..3 {
            for x in 0..5 {
                let px: Vec<f32> = (0..n).map(|c| ((x * 3 + y * 7 + c as i32 * 5) % 11) as f32 / 10.0).collect();
                surf.write_pixel(x, y, &px);
            }
        }
        Pattern::new("P", surf, 5, 3)
    }

    #[test]
    fn patterns_round_trip_through_psd_at_every_depth_and_model() {
        for mode in [ColorMode::Rgb, ColorMode::Grayscale] {
            for s in [SampleType::U8, SampleType::U16, SampleType::F32] {
                for alpha in [false, true] {
                    let p = pattern(mode, s, alpha);
                    let back = from_psd(&to_psd(&p)).unwrap();
                    assert_eq!(back, p, "{mode:?} {s:?} {alpha}");
                }
            }
        }
    }

    #[test]
    fn cmyk_and_lab_patterns_open_as_rgb() {
        // A CMYK pattern with no ink (planes stored inverted: 255 = paper) is white.
        let cmyk = PsdPattern {
            mode: 4,
            width: 2,
            height: 1,
            name: "C".into(),
            id: "c".into(),
            palette: None,
            depth: 8,
            channels: vec![vec![255, 255]; 4],
            alpha: None,
        };
        let p = from_psd(&cmyk).unwrap();
        assert_eq!(p.surface.format().mode, ColorMode::Rgb);
        assert!(p.surface.pixel(0, 0).iter().all(|v| *v > 0.98), "{:?}", p.surface.pixel(0, 0));
        // Lab L 100, a 0, b 0 is white too.
        let lab = PsdPattern { mode: 9, channels: vec![vec![255, 255], vec![128, 128], vec![128, 128]], ..cmyk };
        let p = from_psd(&lab).unwrap();
        assert!(p.surface.pixel(1, 0).iter().all(|v| *v > 0.98), "{:?}", p.surface.pixel(1, 0));
    }

    #[test]
    fn export_reuses_raw_blocks_until_patterns_change() {
        let mut d = Document::new("x", photocraft_geom::Size { width: 4, height: 4 }, ColorMode::Rgb, SampleType::U8);
        let p = pattern(ColorMode::Rgb, SampleType::U8, false);
        let raw = write_pattern_block(&[to_psd(&p)]).unwrap();
        d.metadata.psd_global_blocks = vec![(*b"8BIM", *b"Txt2", Arc::new(vec![1])), (*b"8BIM", *b"Patt", Arc::new(raw.clone()))];
        d.patterns = from_global_blocks(&d);
        assert_eq!(d.patterns.len(), 1);
        assert_eq!(export_global_blocks(&d), d.metadata.psd_global_blocks);
        d.patterns.push(pattern(ColorMode::Grayscale, SampleType::U16, true));
        let out = export_global_blocks(&d);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].1, *b"Patt");
        let mut d2 = d.clone();
        d2.metadata.psd_global_blocks = out;
        assert_eq!(from_global_blocks(&d2), d.patterns);
        assert_eq!(read_pat(&write_pat(&d.patterns).unwrap()).unwrap(), d.patterns);
    }
}
