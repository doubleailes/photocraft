//! Sparse, copy-on-write tiled surfaces in any [`PixelFormat`].
//!
//! A [`Surface`] is an infinite plane of 256×256 tiles. Missing tiles read as the surface's
//! default pixel (transparent for layers, white for "reveal all" masks). Tiles are `Arc`-shared,
//! so cloning a surface is O(tiles) and mutation copies only the touched tiles. That makes
//! undo snapshots, background jobs and autosave cheap.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod interrupt;
pub use interrupt::{Cancelled, Interrupt};

use std::collections::BTreeMap;
use std::sync::Arc;

use photocraft_color::{ColorMode, PixelFormat, SampleType, read_sample, write_sample};
use photocraft_geom::{Rect, TILE_SIZE, TileCoord};

/// Pixel storage for one tile: `TILE_SIZE² × bytes_per_pixel`, row-major, interleaved channels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    data: Box<[u8]>,
}

impl Tile {
    fn filled(format: &PixelFormat, pixel: &[u8]) -> Self {
        let n = (TILE_SIZE * TILE_SIZE) as usize;
        let mut data = vec![0u8; n * format.bytes_per_pixel()];
        if pixel.iter().any(|&b| b != 0) {
            for px in data.chunks_exact_mut(pixel.len()) {
                px.copy_from_slice(pixel);
            }
        }
        Tile { data: data.into_boxed_slice() }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

#[derive(Clone, Debug)]
pub struct Surface {
    format: PixelFormat,
    default_pixel: Box<[u8]>,
    tiles: BTreeMap<TileCoord, Arc<Tile>>,
}

impl PartialEq for Surface {
    fn eq(&self, other: &Self) -> bool {
        self.format == other.format && self.default_pixel == other.default_pixel && self.tiles == other.tiles
    }
}

impl Surface {
    /// Transparent/zero surface.
    pub fn new(format: PixelFormat) -> Self {
        Self { format, default_pixel: vec![0u8; format.bytes_per_pixel()].into_boxed_slice(), tiles: BTreeMap::new() }
    }

    /// Surface whose untouched pixels read as `pixel` (normalised channel values).
    pub fn with_default(format: PixelFormat, pixel: &[f32]) -> Self {
        let mut s = Self::new(format);
        encode_pixel(&format, pixel, &mut s.default_pixel);
        s
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }
    pub fn channels(&self) -> usize {
        self.format.channels()
    }
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }
    pub fn tiles(&self) -> impl Iterator<Item = (&TileCoord, &Arc<Tile>)> {
        self.tiles.iter()
    }
    pub fn tile(&self, c: TileCoord) -> Option<&Arc<Tile>> {
        self.tiles.get(&c)
    }
    pub fn default_pixel(&self) -> Vec<f32> {
        decode_pixel(&self.format, &self.default_pixel)
    }

    /// Mutable access to a tile, allocating it (filled with the default pixel) or un-sharing it.
    pub fn tile_mut(&mut self, c: TileCoord) -> &mut Tile {
        let fmt = self.format;
        let dp = self.default_pixel.clone();
        let arc = self.tiles.entry(c).or_insert_with(|| Arc::new(Tile::filled(&fmt, &dp)));
        Arc::make_mut(arc)
    }

    /// Coarse bounds: union of allocated tile rects.
    pub fn tile_bounds(&self) -> Rect {
        self.tiles.keys().fold(Rect::EMPTY, |r, c| r.union(&c.rect()))
    }

    /// Exact bounds of pixels that differ from the default pixel.
    pub fn content_bounds(&self) -> Rect {
        let bpp = self.format.bytes_per_pixel();
        if self.tiles.is_empty() {
            return Rect::EMPTY;
        }
        // Scan tiles from the outside of the tile grid inwards; a tile lying entirely inside the
        // bounds found so far cannot extend them and is skipped (so a full-canvas selection scans
        // roughly its outer ring of tiles instead of every pixel).
        let (mut gx0, mut gy0, mut gx1, mut gy1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for c in self.tiles.keys() {
            (gx0, gy0, gx1, gy1) = (gx0.min(c.tx), gy0.min(c.ty), gx1.max(c.tx), gy1.max(c.ty));
        }
        let mut order: Vec<(&TileCoord, &Arc<Tile>)> = self.tiles.iter().collect();
        order.sort_by_key(|(c, _)| (c.tx - gx0).min(gx1 - c.tx).min(c.ty - gy0).min(gy1 - c.ty));
        let dp = &*self.default_pixel;
        let ts = TILE_SIZE as usize;
        let mut out = Rect::EMPTY;
        for (c, t) in order {
            let origin = c.rect();
            if !out.is_empty() && out.contains_rect(&origin) {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
            for (y, row) in t.data.chunks_exact(ts * bpp).enumerate() {
                let Some(first) = row.chunks_exact(bpp).position(|px| px != dp) else { continue };
                let last = ts - 1 - row.chunks_exact(bpp).rev().position(|px| px != dp).unwrap_or(0);
                x0 = x0.min(first);
                x1 = x1.max(last + 1);
                y0 = y0.min(y);
                y1 = y + 1;
            }
            if x0 != usize::MAX {
                out = out.union(&Rect::new(x0 as i32, y0 as i32, x1 as i32, y1 as i32).translate(origin.x0, origin.y0));
            }
        }
        out
    }

    /// Drop tiles that are entirely the default pixel.
    pub fn prune(&mut self) {
        let dp = self.default_pixel.clone();
        let bpp = dp.len();
        self.tiles.retain(|_, t| t.data.chunks_exact(bpp).any(|px| px != &*dp));
    }

    #[inline]
    fn locate(&self, x: i32, y: i32) -> (TileCoord, usize) {
        let c = TileCoord::containing(x, y);
        let lx = x.rem_euclid(TILE_SIZE) as usize;
        let ly = y.rem_euclid(TILE_SIZE) as usize;
        (c, (ly * TILE_SIZE as usize + lx) * self.format.channels())
    }

    /// Read one pixel as normalised channel values (`channels()` floats).
    pub fn pixel(&self, x: i32, y: i32) -> Vec<f32> {
        let mut out = vec![0.0; self.channels()];
        self.read_pixel(x, y, &mut out);
        out
    }

    pub fn read_pixel(&self, x: i32, y: i32, out: &mut [f32]) {
        let (c, base) = self.locate(x, y);
        let n = self.channels();
        match self.tiles.get(&c) {
            Some(t) => {
                for (i, o) in out.iter_mut().enumerate().take(n) {
                    *o = read_sample(&t.data, self.format.sample, base + i);
                }
            }
            None => {
                for (i, o) in out.iter_mut().enumerate().take(n) {
                    *o = read_sample(&self.default_pixel, self.format.sample, i);
                }
            }
        }
    }

    pub fn write_pixel(&mut self, x: i32, y: i32, px: &[f32]) {
        let (c, base) = self.locate(x, y);
        let n = self.channels();
        let sample = self.format.sample;
        let t = self.tile_mut(c);
        for (i, v) in px.iter().enumerate().take(n) {
            write_sample(&mut t.data, sample, base + i, *v);
        }
    }

    /// One channel of one pixel, without allocating.
    #[inline]
    pub fn sample_channel(&self, x: i32, y: i32, c: usize) -> f32 {
        let (tc, base) = self.locate(x, y);
        match self.tiles.get(&tc) {
            Some(t) => read_sample(&t.data, self.format.sample, base + c),
            None => read_sample(&self.default_pixel, self.format.sample, c),
        }
    }

    /// Channel `c` at `(x0 + i·step, y)` for `i in 0..out.len()`, looking each tile up once per run
    /// (much faster than per-sample [`Surface::sample_channel`] for strided scans).
    pub fn sample_row_strided(&self, y: i32, x0: i32, step: i32, c: usize, out: &mut [f32]) {
        let step = step.max(1);
        let dp = read_sample(&self.default_pixel, self.format.sample, c);
        let n = self.format.channels();
        let ty = y.div_euclid(TILE_SIZE);
        let ly = y.rem_euclid(TILE_SIZE) as usize;
        let mut i = 0;
        while i < out.len() {
            let x = x0 + i as i32 * step;
            let tx = x.div_euclid(TILE_SIZE);
            // Samples that fall in this tile.
            let tile_end = (tx + 1) * TILE_SIZE;
            let count = (((tile_end - x) + step - 1) / step).max(1) as usize;
            let end = (i + count).min(out.len());
            match self.tiles.get(&TileCoord { tx, ty }) {
                None => out[i..end].fill(dp),
                Some(t) => {
                    for (k, o) in out[i..end].iter_mut().enumerate() {
                        let lx = (x + k as i32 * step - tx * TILE_SIZE) as usize;
                        *o = read_sample(&t.data, self.format.sample, (ly * TILE_SIZE as usize + lx) * n + c);
                    }
                }
            }
            i = end;
        }
    }

    /// [`Surface::read_region`] into a reusable buffer (resized to fit).
    pub fn read_region_into(&self, r: Rect, out: &mut Vec<f32>) {
        let n = self.channels();
        let w = r.width() as usize;
        out.clear();
        out.resize(w * r.height() as usize * n, 0.0);
        let dp = self.default_pixel();
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let tile = self.tiles.get(&tc);
            for y in tr.y0..tr.y1 {
                let o = (((y - r.y0) as usize) * w + (tr.x0 - r.x0) as usize) * n;
                let span = tr.width() as usize * n;
                let dst = &mut out[o..o + span];
                match tile {
                    Some(t) => {
                        let base = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (tr.x0 - tc.tx * TILE_SIZE) as usize) * n;
                        for (i, d) in dst.iter_mut().enumerate() {
                            *d = read_sample(&t.data, self.format.sample, base + i);
                        }
                    }
                    None => {
                        for px in dst.chunks_exact_mut(n) {
                            px.copy_from_slice(&dp);
                        }
                    }
                }
            }
        }
    }

    /// Reads `r` as straight RGBA floats (converting from the surface's
    /// model) into `out` (`w*h` entries), without per-pixel allocation.
    /// Fast paths for RGBA8, RGBA16F and GRAYA8 read tile bytes
    /// directly; missing tiles are filled with the default pixel.
    pub fn read_rgba_into(&self, r: Rect, out: &mut [[f32; 4]]) {
        let w = r.width() as usize;
        debug_assert_eq!(out.len(), w * r.height() as usize);
        let fmt = self.format;
        let n = fmt.channels();
        let dp = to_rgba(&fmt, &self.default_pixel());
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let tile = self.tiles.get(&tc);
            for y in tr.y0..tr.y1 {
                let o = ((y - r.y0) as usize) * w + (tr.x0 - r.x0) as usize;
                let dst = &mut out[o..o + tr.width() as usize];
                let Some(t) = tile else {
                    dst.fill(dp);
                    continue;
                };
                let base = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (tr.x0 - tc.tx * TILE_SIZE) as usize) * n;
                match (fmt.mode, fmt.sample, fmt.alpha) {
                    (ColorMode::Rgb, SampleType::U8, true) => {
                        let src = &t.data[base..base + dst.len() * 4];
                        for (d, s) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                            *d = [s[0] as f32 / 255.0, s[1] as f32 / 255.0, s[2] as f32 / 255.0, s[3] as f32 / 255.0];
                        }
                    }
                    (ColorMode::Rgb, SampleType::F16, true) => {
                        let src = &t.data[base * 2..(base + dst.len() * 4) * 2];
                        for (d, s) in dst.iter_mut().zip(src.as_chunks::<8>().0) {
                            let h = |i: usize| half::f16::from_ne_bytes([s[i], s[i + 1]]).to_f32();
                            *d = [h(0), h(2), h(4), h(6)];
                        }
                    }
                    (ColorMode::Grayscale, SampleType::U8, true) => {
                        let src = &t.data[base..base + dst.len() * 2];
                        for (d, s) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                            let g = s[0] as f32 / 255.0;
                            *d = [g, g, g, s[1] as f32 / 255.0];
                        }
                    }
                    _ => {
                        let mut px = [0.0f32; 8];
                        for (i, d) in dst.iter_mut().enumerate() {
                            for (c, v) in px.iter_mut().enumerate().take(n) {
                                *v = read_sample(&t.data, fmt.sample, base + i * n + c);
                            }
                            *d = to_rgba(&fmt, &px[..n]);
                        }
                    }
                }
            }
        }
    }

    /// Reads `r` as straight 8-bit RGBA into `out` (`w*h` entries). RGBA8 surfaces copy tile bytes
    /// directly; other formats convert through [`Surface::read_rgba_into`] one tile row at a time.
    pub fn read_rgba8_into(&self, r: Rect, out: &mut [[u8; 4]]) {
        let w = r.width() as usize;
        debug_assert_eq!(out.len(), w * r.height() as usize);
        let fmt = self.format;
        let rgba8 = matches!((fmt.mode, fmt.sample, fmt.alpha), (ColorMode::Rgb, SampleType::U8, true));
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let dp = to_rgba(&fmt, &self.default_pixel());
        let dp8 = [q(dp[0]), q(dp[1]), q(dp[2]), q(dp[3])];
        let mut row = Vec::new();
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let tile = self.tiles.get(&tc);
            for y in tr.y0..tr.y1 {
                let o = ((y - r.y0) as usize) * w + (tr.x0 - r.x0) as usize;
                let dst = &mut out[o..o + tr.width() as usize];
                match tile {
                    None => dst.fill(dp8),
                    Some(t) if rgba8 => {
                        let base = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (tr.x0 - tc.tx * TILE_SIZE) as usize) * 4;
                        let len = dst.len();
                        for (d, s) in dst.iter_mut().zip(t.data[base..base + len * 4].as_chunks::<4>().0) {
                            *d = [s[0], s[1], s[2], s[3]];
                        }
                    }
                    Some(_) => {
                        row.resize(dst.len(), [0.0f32; 4]);
                        self.read_rgba_into(Rect::new(tr.x0, y, tr.x1, y + 1), &mut row);
                        for (d, p) in dst.iter_mut().zip(&row) {
                            *d = [q(p[0]), q(p[1]), q(p[2]), q(p[3])];
                        }
                    }
                }
            }
        }
    }

    /// `true` if any allocated tile intersects `r` (else `r` reads as the
    /// default pixel everywhere).
    pub fn has_tiles_in(&self, r: Rect) -> bool {
        r.tiles().any(|tc| self.tiles.contains_key(&tc))
    }

    /// Read a rectangle as interleaved normalised floats (`w*h*channels`).
    pub fn read_region(&self, r: Rect) -> Vec<f32> {
        let n = self.channels();
        let w = r.width() as usize;
        let mut out = vec![0.0f32; w * r.height() as usize * n];
        let dp = self.default_pixel();
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let tile = self.tiles.get(&tc);
            for y in tr.y0..tr.y1 {
                for x in tr.x0..tr.x1 {
                    let o = (((y - r.y0) as usize) * w + (x - r.x0) as usize) * n;
                    match tile {
                        Some(t) => {
                            let base = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (x - tc.tx * TILE_SIZE) as usize) * n;
                            for i in 0..n {
                                out[o + i] = read_sample(&t.data, self.format.sample, base + i);
                            }
                        }
                        None => out[o..o + n].copy_from_slice(&dp),
                    }
                }
            }
        }
        out
    }

    /// Write interleaved normalised floats into a rectangle.
    pub fn write_region(&mut self, r: Rect, data: &[f32]) {
        let n = self.channels();
        let w = r.width() as usize;
        assert_eq!(data.len(), w * r.height() as usize * n, "region data length mismatch");
        let sample = self.format.sample;
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let t = self.tile_mut(tc);
            for y in tr.y0..tr.y1 {
                for x in tr.x0..tr.x1 {
                    let o = (((y - r.y0) as usize) * w + (x - r.x0) as usize) * n;
                    let base = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (x - tc.tx * TILE_SIZE) as usize) * n;
                    for i in 0..n {
                        write_sample(&mut t.data, sample, base + i, data[o + i]);
                    }
                }
            }
        }
    }

    /// Take the tiles covering `r` out of the surface (missing ones filled with the default
    /// pixel), so they can be edited elsewhere, e.g. in parallel: `Arc::unwrap_or_clone` copies a
    /// tile only if an undo snapshot still shares it. Give them back with [`Surface::put_tiles`].
    pub fn take_tiles(&mut self, r: Rect) -> Vec<(TileCoord, Arc<Tile>)> {
        let fmt = self.format;
        let mut blank: Option<Arc<Tile>> = None;
        r.tiles()
            .map(|c| {
                let t = self.tiles.remove(&c).unwrap_or_else(|| blank.get_or_insert_with(|| Arc::new(Tile::filled(&fmt, &self.default_pixel))).clone());
                (c, t)
            })
            .collect()
    }

    /// Put tiles back (see [`Surface::take_tiles`]); each replaces the tile at its coordinate.
    pub fn put_tiles(&mut self, tiles: impl IntoIterator<Item = (TileCoord, Arc<Tile>)>) {
        for (c, t) in tiles {
            self.tiles.insert(c, t);
        }
    }

    /// A tile of this surface's format filled with `px` (normalised channel values). Sharing one
    /// across many coordinates fills them without copying (tiles are copy-on-write).
    pub fn solid_tile(&self, px: &[f32]) -> Arc<Tile> {
        let mut enc = vec![0u8; self.format.bytes_per_pixel()];
        encode_pixel(&self.format, px, &mut enc);
        Arc::new(Tile::filled(&self.format, &enc))
    }

    /// The default pixel's encoded bytes.
    pub fn default_bytes(&self) -> &[u8] {
        &self.default_pixel
    }

    /// Fill a rectangle with one pixel value.
    pub fn fill_rect(&mut self, r: Rect, px: &[f32]) {
        let n = self.channels();
        let mut enc = vec![0u8; self.format.bytes_per_pixel()];
        encode_pixel(&self.format, &px[..n], &mut enc);
        let bpp = enc.len();
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let t = self.tile_mut(tc);
            for y in tr.y0..tr.y1 {
                let ly = (y - tc.ty * TILE_SIZE) as usize;
                let lx0 = (tr.x0 - tc.tx * TILE_SIZE) as usize;
                let lx1 = (tr.x1 - tc.tx * TILE_SIZE) as usize;
                let row = &mut t.data[(ly * TILE_SIZE as usize + lx0) * bpp..(ly * TILE_SIZE as usize + lx1) * bpp];
                for p in row.chunks_exact_mut(bpp) {
                    p.copy_from_slice(&enc);
                }
            }
        }
    }

    /// Build from interleaved *encoded* bytes of `format` covering `r`.
    pub fn from_interleaved(format: PixelFormat, r: Rect, bytes: &[u8]) -> Self {
        let mut s = Surface::new(format);
        s.write_interleaved(r, bytes);
        s
    }

    /// Write interleaved encoded bytes (same format as the surface) into `r`.
    pub fn write_interleaved(&mut self, r: Rect, bytes: &[u8]) {
        let bpp = self.format.bytes_per_pixel();
        let w = r.width() as usize;
        assert_eq!(bytes.len(), w * r.height() as usize * bpp, "interleaved length mismatch");
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let t = self.tile_mut(tc);
            let span = tr.width() as usize * bpp;
            for y in tr.y0..tr.y1 {
                let src = (((y - r.y0) as usize) * w + (tr.x0 - r.x0) as usize) * bpp;
                let dst = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (tr.x0 - tc.tx * TILE_SIZE) as usize) * bpp;
                t.data[dst..dst + span].copy_from_slice(&bytes[src..src + span]);
            }
        }
    }

    /// Read `r` as interleaved encoded bytes in the surface's format.
    pub fn to_interleaved(&self, r: Rect) -> Vec<u8> {
        let bpp = self.format.bytes_per_pixel();
        let w = r.width() as usize;
        let mut out = vec![0u8; w * r.height() as usize * bpp];
        for tc in r.tiles() {
            let tr = tc.rect().intersect(&r);
            let span = tr.width() as usize * bpp;
            for y in tr.y0..tr.y1 {
                let dst = (((y - r.y0) as usize) * w + (tr.x0 - r.x0) as usize) * bpp;
                match self.tiles.get(&tc) {
                    Some(t) => {
                        let src = (((y - tc.ty * TILE_SIZE) as usize) * TILE_SIZE as usize + (tr.x0 - tc.tx * TILE_SIZE) as usize) * bpp;
                        out[dst..dst + span].copy_from_slice(&t.data[src..src + span]);
                    }
                    None => {
                        for p in out[dst..dst + span].chunks_exact_mut(bpp) {
                            p.copy_from_slice(&self.default_pixel);
                        }
                    }
                }
            }
        }
        out
    }

    /// Convert to another pixel format (depth and/or model; colour models via [`convert_pixel`]).
    /// Tiles convert in parallel, and a same-model conversion (a depth change, alpha added or
    /// dropped) allocates nothing per pixel.
    pub fn convert(&self, to: PixelFormat) -> Surface {
        let default = convert_pixel(&self.format, &to, &self.default_pixel());
        self.map_tiles_into(to, &default, |src, dst| self.convert_tile(src, dst, &to))
    }

    /// A surface in `to` whose allocated tiles are `f(source tile bytes, destination tile
    /// bytes)`, computed in parallel off wasm; the unallocated area is `default` (a pixel in
    /// `to`). The destination bytes start zeroed.
    pub fn map_tiles_into(&self, to: PixelFormat, default: &[f32], f: impl Fn(&[u8], &mut [u8]) + Sync) -> Surface {
        let mut out = Surface::with_default(to, default);
        let len = (TILE_SIZE * TILE_SIZE) as usize * to.bytes_per_pixel();
        let tiles: Vec<(TileCoord, &Tile)> = self.tiles.iter().map(|(c, t)| (*c, t.as_ref())).collect();
        let mapped = map_tiles(&tiles, |t| {
            let mut data = vec![0u8; len];
            f(&t.data, &mut data);
            Tile { data: data.into_boxed_slice() }
        });
        for (c, tile) in mapped {
            out.tiles.insert(c, Arc::new(tile));
        }
        out
    }

    /// One tile of [`Surface::convert`].
    fn convert_tile(&self, src_bytes: &[u8], data: &mut [u8], to: &PixelFormat) {
        let from = &self.format;
        let px_count = (TILE_SIZE * TILE_SIZE) as usize;
        let (from_n, to_n) = (from.channels(), to.channels());
        let mut src = vec![0.0f32; from_n];
        let same_model = from.mode == to.mode;
        let colour_n = from.mode.color_channels();
        for i in 0..px_count {
            for (k, v) in src.iter_mut().enumerate() {
                *v = read_sample(src_bytes, from.sample, i * from_n + k);
            }
            if same_model {
                // What `convert_pixel` returns for a same-model pixel, without its allocation.
                for (k, v) in src.iter().take(colour_n).enumerate() {
                    write_sample(data, to.sample, i * to_n + k, *v);
                }
                if to.alpha {
                    let a = if from.alpha { src.get(colour_n).copied().unwrap_or(1.0) } else { 1.0 };
                    write_sample(data, to.sample, i * to_n + colour_n, a);
                }
            } else {
                for (k, v) in convert_pixel(from, to, &src).iter().enumerate() {
                    write_sample(data, to.sample, i * to_n + k, *v);
                }
            }
        }
    }

    /// Composite-friendly RGBA read (converts from the surface's model).
    pub fn rgba(&self, x: i32, y: i32) -> [f32; 4] {
        let p = self.pixel(x, y);
        to_rgba(&self.format, &p)
    }
}

/// Encode normalised floats into one pixel's bytes.
/// `f` over every tile, in parallel off wasm (tiles are independent).
fn map_tiles(tiles: &[(TileCoord, &Tile)], f: impl Fn(&Tile) -> Tile + Sync) -> Vec<(TileCoord, Tile)> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        tiles.par_iter().map(|(c, t)| (*c, f(t))).collect()
    }
    #[cfg(target_arch = "wasm32")]
    tiles.iter().map(|(c, t)| (*c, f(t))).collect()
}

pub fn encode_pixel(format: &PixelFormat, px: &[f32], out: &mut [u8]) {
    for (i, v) in px.iter().enumerate().take(format.channels()) {
        write_sample(out, format.sample, i, *v);
    }
}

pub fn decode_pixel(format: &PixelFormat, bytes: &[u8]) -> Vec<f32> {
    (0..format.channels()).map(|i| read_sample(bytes, format.sample, i)).collect()
}

/// Convert one pixel between formats via sRGB RGBA (CMYK through the built-in CMYK profile;
/// the engine's mode conversions use the document's profiles instead).
pub fn convert_pixel(from: &PixelFormat, to: &PixelFormat, px: &[f32]) -> Vec<f32> {
    if from.mode == to.mode {
        let n = from.mode.color_channels();
        let mut out: Vec<f32> = px[..n].to_vec();
        if to.alpha {
            out.push(if from.alpha { px[n] } else { 1.0 });
        }
        return out;
    }
    let rgba = to_rgba(from, px);
    from_rgba(to, rgba)
}

pub fn to_rgba(format: &PixelFormat, px: &[f32]) -> [f32; 4] {
    let n = format.mode.color_channels();
    let a = if format.alpha { px[n] } else { 1.0 };
    let rgb = match format.mode {
        ColorMode::Grayscale => [px[0]; 3],
        ColorMode::Rgb => [px[0], px[1], px[2]],
    };
    [rgb[0], rgb[1], rgb[2], a]
}

pub fn from_rgba(format: &PixelFormat, rgba: [f32; 4]) -> Vec<f32> {
    let rgb = [rgba[0], rgba[1], rgba[2]];
    let mut out: Vec<f32> = match format.mode {
        ColorMode::Grayscale => vec![photocraft_color::convert::rgb_to_gray(rgb)],
        ColorMode::Rgb => rgb.to_vec(),
    };
    if format.alpha {
        out.push(rgba[3]);
    }
    out
}

/// [`from_rgba`] without allocating: writes `format.channels()` values into
/// `out` and returns how many were written.
#[inline]
pub fn from_rgba_into(format: &PixelFormat, rgba: [f32; 4], out: &mut [f32]) -> usize {
    let rgb = [rgba[0], rgba[1], rgba[2]];
    let n = match format.mode {
        ColorMode::Grayscale => {
            out[0] = photocraft_color::convert::rgb_to_gray(rgb);
            1
        }
        ColorMode::Rgb => {
            out[..3].copy_from_slice(&rgb);
            3
        }
    };
    if format.alpha {
        out[n] = rgba[3];
        n + 1
    } else {
        n
    }
}

/// A simple owned RGBA8 image, used for display and thumbnails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgba8Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Rgba8Image {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, pixels: vec![0; width as usize * height as usize * 4] }
    }
}

/// Is this sample type able to hold values above 1.0 (HDR)?
pub fn is_hdr(sample: SampleType) -> bool {
    sample.is_float()
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::PixelFormat;
    use proptest::prelude::*;

    /// The per-pixel reference [`Surface::convert`] must match: every tile, every pixel, through
    /// `convert_pixel`.
    fn convert_reference(s: &Surface, to: PixelFormat) -> Surface {
        let mut out = Surface::with_default(to, &convert_pixel(&s.format, &to, &s.default_pixel()));
        let n = s.channels();
        for (c, t) in &s.tiles {
            let dst = out.tile_mut(*c);
            for i in 0..(TILE_SIZE * TILE_SIZE) as usize {
                let src: Vec<f32> = (0..n).map(|k| read_sample(&t.data, s.format.sample, i * n + k)).collect();
                for (k, v) in convert_pixel(&s.format, &to, &src).iter().enumerate() {
                    write_sample(&mut dst.data, to.sample, i * to.channels() + k, *v);
                }
            }
        }
        out
    }

    #[test]
    fn convert_matches_the_per_pixel_reference() {
        // Pixels in several tiles, so the parallel path is exercised; values chosen so every
        // depth has an exact representation or a rounding the reference shares.
        let sources = [
            (PixelFormat::RGBA8, vec![0.0, 0.25, 0.5, 1.0]),
            (PixelFormat::RGBA16, vec![0.1, 0.2, 0.3, 0.75]),
            (PixelFormat::RGBA16F, vec![0.5, 0.125, 2.0, 0.5]),
            (PixelFormat::RGBA32F, vec![-0.5, 1.5, 0.333, 0.9]),
            (PixelFormat::GRAY8, vec![0.5, 0.0, 0.0, 0.0]),
            (PixelFormat::GRAYA8, vec![0.5, 0.25, 0.0, 0.0]),
        ];
        let targets = [
            PixelFormat::RGBA8,
            PixelFormat::RGBA16F,
            PixelFormat::RGBA32F,
            PixelFormat::GRAY8,
            PixelFormat::GRAYA8,
            PixelFormat::new(ColorMode::Rgb, SampleType::F32, false),
            PixelFormat::new(ColorMode::Grayscale, SampleType::F16, true),
        ];
        for (from, px) in &sources {
            let mut s = Surface::with_default(*from, &convert_pixel(from, from, &[0.0; 4][..from.channels()]));
            for (x, y) in [(0, 0), (300, 10), (5, 700), (513, 513)] {
                s.write_pixel(x, y, &px[..from.channels()]);
            }
            for to in targets {
                assert_eq!(s.convert(to), convert_reference(&s, to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn from_rgba_into_matches_from_rgba() {
        for fmt in
            [PixelFormat::RGBA8, PixelFormat::GRAY8, PixelFormat::GRAYA8, PixelFormat::RGBA16F, PixelFormat::new(ColorMode::Grayscale, SampleType::F32, true)]
        {
            let mut buf = [0.0f32; 8];
            let n = from_rgba_into(&fmt, [0.2, 0.5, 0.7, 0.4], &mut buf);
            assert_eq!(&buf[..n], &from_rgba(&fmt, [0.2, 0.5, 0.7, 0.4])[..]);
        }
    }

    #[test]
    fn zero_alloc_accessors_match_read_region() {
        for fmt in [PixelFormat::RGBA8, PixelFormat::RGBA16, PixelFormat::RGBA16F, PixelFormat::RGBA32F, PixelFormat::GRAYA8, PixelFormat::GRAY8] {
            let mut s = Surface::with_default(fmt, &vec![0.25; fmt.channels()]);
            let r = Rect::new(-300, -20, 300, 40);
            for (i, (x, y)) in [(-299, -19), (0, 0), (255, 39), (256, 10), (299, 0)].into_iter().enumerate() {
                let px: Vec<f32> = (0..fmt.channels()).map(|c| ((i * 3 + c) % 5) as f32 / 4.0).collect();
                s.write_pixel(x, y, &px);
            }
            let region = s.read_region(r);
            let mut into = vec![9.0; 3];
            s.read_region_into(r, &mut into);
            assert_eq!(region, into);
            let mut rgba = vec![[0.0; 4]; (r.width() * r.height()) as usize];
            s.read_rgba_into(r, &mut rgba);
            let n = fmt.channels();
            for (i, p) in region.chunks_exact(n).enumerate() {
                let want = to_rgba(&fmt, p);
                for c in 0..4 {
                    assert!((rgba[i][c] - want[c]).abs() < 1e-6, "{fmt:?} px {i}");
                }
                let (x, y) = (r.x0 + (i % r.width() as usize) as i32, r.y0 + (i / r.width() as usize) as i32);
                assert_eq!(s.sample_channel(x, y, 0), p[0]);
            }
            assert!(s.has_tiles_in(Rect::new(0, 0, 1, 1)));
            assert!(!s.has_tiles_in(Rect::new(1000, 1000, 1001, 1001)));
        }
    }

    #[test]
    fn empty_surface_reads_default() {
        let s = Surface::new(PixelFormat::RGBA8);
        assert_eq!(s.pixel(10, -500), vec![0.0; 4]);
        let m = Surface::with_default(PixelFormat::GRAY8, &[1.0]);
        assert_eq!(m.pixel(123_456, -9), vec![1.0]);
        assert_eq!(m.tile_count(), 0);
    }

    #[test]
    fn write_read_pixel_every_depth() {
        for sample in SampleType::ALL {
            let f = PixelFormat::RGBA8.with_sample(sample);
            let mut s = Surface::new(f);
            s.write_pixel(-3, 700, &[0.2, 0.4, 0.6, 1.0]);
            let p = s.pixel(-3, 700);
            for (a, b) in p.iter().zip([0.2, 0.4, 0.6, 1.0]) {
                assert!((a - b).abs() <= 1.0 / 255.0, "{sample:?}");
            }
            assert_eq!(s.tile_count(), 1);
            assert_eq!(s.tile_bounds(), TileCoord::containing(-3, 700).rect());
        }
    }

    #[test]
    fn copy_on_write_shares_untouched_tiles() {
        let mut a = Surface::new(PixelFormat::RGBA8);
        a.fill_rect(Rect::new(0, 0, 512, 512), &[1.0, 0.0, 0.0, 1.0]);
        let mut b = a.clone();
        b.write_pixel(10, 10, &[0.0, 1.0, 0.0, 1.0]);
        // a unchanged
        assert_eq!(a.pixel(10, 10), vec![1.0, 0.0, 0.0, 1.0]);
        assert_eq!(b.pixel(10, 10), vec![0.0, 1.0, 0.0, 1.0]);
        // only tile (0,0) was copied
        let shared = a.tiles().filter(|(c, t)| Arc::ptr_eq(t, b.tile(**c).unwrap())).count();
        assert_eq!(shared, 3);
    }

    #[test]
    fn content_bounds_and_prune() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        s.fill_rect(Rect::new(300, 10, 310, 20), &[1.0, 1.0, 1.0, 1.0]);
        s.write_pixel(-5, -5, &[0.0, 0.0, 0.0, 1.0]);
        assert_eq!(s.content_bounds(), Rect::new(-5, -5, 310, 20));
        // erase the single pixel -> its tile becomes default and is pruned
        s.write_pixel(-5, -5, &[0.0, 0.0, 0.0, 0.0]);
        s.prune();
        assert_eq!(s.tile_count(), 1);
        assert_eq!(s.content_bounds(), Rect::new(300, 10, 310, 20));
    }

    #[test]
    fn interleaved_roundtrip_crossing_tiles() {
        let r = Rect::new(-20, 250, 280, 262);
        let bpp = PixelFormat::RGBA16.bytes_per_pixel();
        let bytes: Vec<u8> = (0..r.width() as usize * r.height() as usize * bpp).map(|i| (i * 31 % 251) as u8).collect();
        let s = Surface::from_interleaved(PixelFormat::RGBA16, r, &bytes);
        assert_eq!(s.to_interleaved(r), bytes);
        assert_eq!(s.tile_count(), 6); // x tiles -1..=1, y tiles 0..=1
    }

    #[test]
    fn region_outside_reads_default() {
        let s = Surface::with_default(PixelFormat::GRAY8, &[1.0]);
        let v = s.read_region(Rect::new(0, 0, 3, 3));
        assert_eq!(v, vec![1.0; 9]);
    }

    #[test]
    fn convert_depths_and_models() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        s.write_pixel(1, 1, &[1.0, 1.0, 1.0, 1.0]);
        let g = s.convert(PixelFormat::GRAYA8);
        assert!((g.pixel(1, 1)[0] - 1.0).abs() < 1e-2);
        let f = s.convert(PixelFormat::RGBA32F);
        assert_eq!(f.pixel(1, 1), vec![1.0, 1.0, 1.0, 1.0]);
        let back = g.convert(PixelFormat::RGBA8);
        assert_eq!(back.pixel(1, 1), vec![1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn rgba_from_gray() {
        let mut g = Surface::new(PixelFormat::GRAYA8);
        g.write_pixel(0, 0, &[0.5, 1.0]);
        let v = g.rgba(0, 0);
        assert!((v[0] - v[2]).abs() < 1e-6 && (v[3] - 1.0).abs() < 1e-6);
    }

    proptest! {
        #[test]
        fn region_write_read_roundtrip(x in -600i32..600, y in -600i32..600, w in 1u32..90, h in 1u32..90, seed in any::<u32>()) {
            let r = Rect::from_xywh(x, y, w, h);
            let n = 4usize;
            let data: Vec<f32> = (0..(w * h) as usize * n).map(|i| ((i as u32).wrapping_mul(2654435761).wrapping_add(seed) % 256) as f32 / 255.0).collect();
            let mut s = Surface::new(PixelFormat::RGBA8);
            s.write_region(r, &data);
            let back = s.read_region(r);
            for (a, b) in back.iter().zip(&data) {
                prop_assert!((a - b).abs() < 1e-6);
            }
            prop_assert_eq!(s.content_bounds().union(&r), r);
        }

        #[test]
        fn clone_mutation_isolated(x in -300i32..300, y in -300i32..300) {
            let mut a = Surface::new(PixelFormat::RGBA8);
            a.fill_rect(Rect::new(-256, -256, 256, 256), &[0.5, 0.5, 0.5, 1.0]);
            let snapshot = a.clone();
            a.write_pixel(x, y, &[1.0, 0.0, 0.0, 1.0]);
            let expect = if Rect::new(-256, -256, 256, 256).contains(x, y) { vec![0.5f32, 0.5, 0.5, 1.0] } else { vec![0.0; 4] };
            let got = snapshot.pixel(x, y);
            for (g, e) in got.iter().zip(&expect) {
                prop_assert!((g - e).abs() <= 1.0 / 255.0);
            }
        }
    }

    #[test]
    fn strided_row_sampling_matches_sample_channel() {
        let mut s = Surface::with_default(PixelFormat::GRAY8, &[0.25]);
        s.fill_rect(Rect::new(-300, 10, 200, 40), &[1.0]);
        s.fill_rect(Rect::new(250, 10, 700, 20), &[0.5]);
        for (y, x0, step) in [(15, -520, 7), (15, -1, 1), (30, 3, 64), (500, 0, 9)] {
            let mut out = vec![0.0; 120];
            s.sample_row_strided(y, x0, step, 0, &mut out);
            for (i, v) in out.iter().enumerate() {
                assert_eq!(*v, s.sample_channel(x0 + i as i32 * step, y, 0), "y={y} x0={x0} step={step} i={i}");
            }
        }
    }

    #[test]
    fn content_bounds_with_tile_skipping() {
        // Interior content only; ring tiles present but all-default (unpruned).
        let mut s = Surface::new(PixelFormat::GRAY8);
        s.fill_rect(Rect::new(0, 0, 1024, 1024), &[0.0]);
        s.fill_rect(Rect::new(300, 310, 700, 720), &[1.0]);
        assert_eq!(s.content_bounds(), Rect::new(300, 310, 700, 720));
        // Full-canvas content plus a far-away speck.
        let mut s = Surface::new(PixelFormat::GRAY8);
        s.fill_rect(Rect::new(0, 0, 2000, 1500), &[1.0]);
        s.fill_rect(Rect::new(-700, 4000, -699, 4001), &[0.5]);
        assert_eq!(s.content_bounds(), Rect::new(-700, 0, 2000, 4001));
    }
}
