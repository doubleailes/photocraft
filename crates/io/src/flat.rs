//! Flat raster formats via `photocraft-codecs`.

use std::sync::Arc;

use photocraft_codecs::{self as codecs, ChannelLayout, Format, Image, SampleType as CSample};
use photocraft_color::{BlendMode, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer, LayerContent};
use photocraft_geom::{Rect, Size, TILE_SIZE};
use photocraft_raster::Surface;

use crate::{ExportOptions, ExportResult, ImportResult, IoError, XmpEmbed};

/// Bytes per band when converting or exporting a band of rows at a time.
const BAND_BYTES: usize = 32 << 20;

/// Decodes a flat image into a single-layer document; a TIFF with Photoshop layer data opens
/// layered (see [`crate::tiff_layers`]).
pub fn import_flat(name: &str, bytes: &[u8]) -> Result<ImportResult, IoError> {
    if codecs::detect(bytes) == Some(Format::Tiff) {
        return import_tiff_page(name, bytes, None);
    }
    let img = codecs::decode(bytes)?;
    let mut r = image_to_document(name, &img)?;
    // OpenEXR and Radiance HDR hold linear, scene-referred values (Rec. 709 primaries unless
    // stated otherwise): tag them linear so they display and convert correctly.
    let d = &mut r.document;
    if d.color_space.is_empty() && d.mode == ColorMode::Rgb && matches!(codecs::detect(bytes), Some(Format::OpenExr | Format::Hdr)) {
        d.color_space = photocraft_color::space::LINEAR.to_string();
        d.source_space = photocraft_color::space::LINEAR.to_string();
    }
    Ok(r)
}

/// Opens one page of a TIFF or BigTIFF file: `None` is the page Photoshop opens (the first
/// full-resolution one), `Some(i)` an index into [`codecs::tiff_info`]'s pages (every IFD and
/// SubIFD). A page with Photoshop layer data opens layered.
pub fn import_tiff_page(name: &str, bytes: &[u8], page: Option<usize>) -> Result<ImportResult, IoError> {
    // The orientation is applied after the layer check: rotating the composite but not the
    // layers would misalign them, so a layered TIFF keeps its stored orientation.
    let keep = codecs::DecodeOptions { keep_orientation: true, ..Default::default() };
    let (img, orientation) = match page {
        None => (codecs::decode_with(bytes, &keep)?, codecs::tiff_orientation(bytes)),
        Some(p) => (codecs::decode_tiff_page(bytes, p, &keep)?, codecs::tiff_page_orientation(bytes, p)),
    };
    if let Some(layers) = img.meta.photoshop_layers.as_deref().filter(|l| !l.is_empty()) {
        let mut r = crate::tiff_layers::import_layered(name, &img, layers)?;
        if orientation != 1 {
            r.warnings.push("the TIFF's orientation tag was ignored so the layers stay aligned with the image".to_string());
        }
        return Ok(r);
    }
    image_to_document(name, &img.oriented(orientation)?)
}

/// A decoded flat image as a single-layer document.
pub(crate) fn image_to_document(name: &str, img: &Image) -> Result<ImportResult, IoError> {
    // What the decoder noticed (frames or pages left out, data ending early) comes first.
    let mut warnings: Vec<String> = img.warnings.iter().map(ToString::to_string).collect();
    // CMYK files (JPEG, TIFF) convert to sRGB through their profile: documents are RGB or gray.
    let converted;
    let converted_from_cmyk = img.layout().is_cmyk();
    let img = if converted_from_cmyk {
        converted = cmyk_image_to_srgb(img)?;
        warnings.push("CMYK image converted to RGB (PhotoCraft documents are RGB or grayscale)".to_string());
        &converted
    } else {
        img
    };
    let (mode, target_layout) = match img.layout() {
        ChannelLayout::Gray | ChannelLayout::GrayA => (ColorMode::Grayscale, ChannelLayout::GrayA),
        _ => (ColorMode::Rgb, ChannelLayout::Rgba),
    };
    let (depth, csample) = match img.sample_type() {
        CSample::U8 => (SampleType::U8, CSample::U8),
        CSample::U16 => (SampleType::U16, CSample::U16),
        CSample::F16 => (SampleType::F16, CSample::F16),
        CSample::F32 => (SampleType::F32, CSample::F32),
    };
    let (w, h) = img.dimensions();
    let mut doc = Document::new(name, Size::new(w, h), mode, depth);
    let fmt = PixelFormat::new(mode, depth, true);
    let mut s = Surface::new(fmt);
    if img.layout() == target_layout && img.sample_type() == csample {
        s.write_interleaved(Rect::new(0, 0, w as i32, h as i32), img.data());
    } else {
        // Converted a band of rows at a time: no second full-size copy of the image.
        let row = img.data().len() / (h.max(1) as usize);
        let band = (BAND_BYTES / row.max(1)).max(1);
        for (i, rows) in img.data().chunks(row.max(1) * band).enumerate() {
            let n = (rows.len() / row.max(1)) as u32;
            let part = Image::from_raw(w, n, img.layout(), img.sample_type(), rows.to_vec())?.convert(target_layout, csample);
            let y0 = (i * band) as i32;
            s.write_interleaved(Rect::new(0, y0, w as i32, y0 + n as i32), part.data());
        }
    }
    s.prune();
    let mut bg = Layer::new("Background", LayerContent::Raster(s));
    if !img.layout().has_alpha() {
        bg.locks.transparency = true;
        bg.locks.position = true;
    }
    doc.layers.push(bg);
    if converted_from_cmyk {
        // Converted to sRGB above: the file's CMYK profile no longer describes the pixels.
        doc.color_space = photocraft_color::space::SRGB.to_string();
    } else {
        crate::spaces::tag_from_icc(&mut doc, img.icc.as_deref(), &mut warnings);
    }
    doc.metadata.exif = img.meta.exif.clone().map(Arc::new);
    doc.metadata.xmp = img.meta.xmp.clone();
    if let Some((x, _)) = img.meta.dpi {
        doc.resolution_dpi = x;
    }
    if !img.meta.text.is_empty() {
        warnings.push(format!("{} text metadata entries are not kept in the document", img.meta.text.len()));
    }
    Ok(ImportResult { document: doc, warnings })
}

/// `Some(surface)` when the document is exactly one visible, unmasked,
/// normal, fully opaque raster layer: its pixels can be written natively
/// (keeping CMYK / depth exactly) instead of going through the compositor.
pub(crate) fn single_layer(doc: &Document) -> Option<&Surface> {
    let [l] = &doc.layers[..] else { return None };
    let ok = l.visible
        && l.opacity >= 1.0
        && l.fill_opacity >= 1.0
        && l.mask.is_none()
        && l.vector_mask.as_ref().is_none_or(|mask| !mask.enabled)
        && l.effects.items.is_empty()
        && l.effects.psd_raw.is_none()
        && matches!(l.blend, BlendMode::Normal | BlendMode::PassThrough)
        && photocraft_compose::channel_weights(l, doc.mode).is_none()
        && !photocraft_compose::blend_if_active(l, doc.mode);
    match (&l.content, ok) {
        (LayerContent::Raster(s), true) if s.format() == doc.pixel_format() => Some(s),
        _ => None,
    }
}

pub(crate) fn layout_for(mode: ColorMode, alpha: bool) -> ChannelLayout {
    match (mode, alpha) {
        (ColorMode::Grayscale, false) => ChannelLayout::Gray,
        (ColorMode::Grayscale, true) => ChannelLayout::GrayA,
        (ColorMode::Rgb, false) => ChannelLayout::Rgb,
        (ColorMode::Rgb, true) => ChannelLayout::Rgba,
    }
}

pub(crate) fn csample(s: SampleType) -> CSample {
    match s {
        SampleType::U8 => CSample::U8,
        SampleType::U16 => CSample::U16,
        SampleType::F16 => CSample::F16,
        SampleType::F32 => CSample::F32,
    }
}

/// Renders the document to a flat codec image (native pixels when possible).
pub fn document_to_image(doc: &Document, warnings: &mut Vec<String>) -> Result<Image, IoError> {
    let (w, h) = (doc.size.width, doc.size.height);
    let canvas = doc.bounds();
    let fmt = doc.pixel_format();
    let n = (w as usize) * (h as usize);
    let native = single_layer(doc);
    let icc = crate::spaces::embedded_icc(doc);
    let img = if let Some(s) = native {
        // Native path: keep model and depth. The surface's encoded samples are the codec's raw
        // native-endian samples, copied a band of rows at a time (alpha dropped when opaque).
        let ch = fmt.channels();
        let opaque = opaque_surface(s, canvas);
        let layout = layout_for(fmt.mode, !opaque);
        let bps = fmt.sample.bytes();
        let keep = if opaque { (ch - 1) * bps } else { ch * bps };
        let mut data = try_buffer(n, keep)?;
        let rows = (BAND_BYTES / (w as usize * ch * bps).max(1)).max(1) as i32;
        let mut y = 0;
        while y < h as i32 {
            let y1 = y.saturating_add(rows).min(h as i32);
            let band = s.to_interleaved(Rect::new(0, y, w as i32, y1));
            if opaque {
                for px in band.chunks_exact(ch * bps) {
                    data.extend_from_slice(&px[..keep]);
                }
            } else {
                data.extend_from_slice(&band);
            }
            y = y1;
        }
        Image::from_raw(w, h, layout, csample(fmt.sample), data)?
    } else {
        let count = doc.layer_count();
        warnings.push(format!("{count} layer(s) flattened; layers, masks and blend modes are not kept"));
        // The compositor works in RGB; write RGB/gray.
        let gray = fmt.mode == ColorMode::Grayscale;
        let cs = csample(fmt.sample);
        let colors = if gray { 1 } else { 3 };
        // Rendered and quantised in bands (no full-size float composite), with alpha; the alpha
        // is dropped afterwards, in place, when every pixel turned out opaque.
        let mut data = try_buffer(n, (colors + 1) * cs.bytes())?;
        let mut opaque = true;
        let _ = photocraft_compose::render_bands(doc, canvas, 0, |band| -> Result<(), ()> {
            opaque &= band.px.iter().all(|p| p[3] >= 1.0);
            let parts = crate::pixels::par_map(crate::pixels::bands(band.px.len()), |range| {
                let mut out = Vec::with_capacity(range.len() * (colors + 1) * cs.bytes());
                let mut put = |v: f32| {
                    let v = if v.is_nan() { 0.0 } else { v };
                    match cs {
                        CSample::U8 => out.push((v.clamp(0.0, 1.0) * 255.0).round() as u8),
                        CSample::U16 => out.extend_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_ne_bytes()),
                        CSample::F16 => out.extend_from_slice(&half::f16::from_f32(v).to_ne_bytes()),
                        CSample::F32 => out.extend_from_slice(&v.to_ne_bytes()),
                    }
                };
                for p in &band.px[range] {
                    if gray {
                        put(photocraft_color::convert::rgb_to_gray([p[0], p[1], p[2]]));
                    } else {
                        put(p[0]);
                        put(p[1]);
                        put(p[2]);
                    }
                    put(p[3]);
                }
                out
            });
            for part in parts {
                data.extend_from_slice(&part);
            }
            Ok(())
        });
        if opaque {
            drop_alpha(&mut data, colors * cs.bytes(), cs.bytes());
        }
        let layout = layout_for(if gray { ColorMode::Grayscale } else { ColorMode::Rgb }, !opaque);
        Image::from_raw(w, h, layout, cs, data)?
    };
    let meta = codecs::Metadata {
        exif: doc.metadata.exif.as_ref().map(|e| e.to_vec()),
        xmp: doc.metadata.xmp.clone(),
        dpi: Some((doc.resolution_dpi, doc.resolution_dpi)),
        ..Default::default()
    };
    Ok(img.with_icc(icc).with_meta(meta))
}

/// An empty buffer with room for `pixels × bytes_per_pixel` bytes, or an error (not an abort)
/// when that much memory can't be had.
pub(crate) fn try_buffer(pixels: usize, bytes_per_pixel: usize) -> Result<Vec<u8>, IoError> {
    let len = pixels.checked_mul(bytes_per_pixel).ok_or_else(|| IoError::Unsupported("image too large".into()))?;
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| IoError::Unsupported(format!("not enough memory for a {} MB image", len >> 20)))?;
    Ok(v)
}

/// Remove the trailing `alpha` bytes of every `color + alpha`-byte pixel, in place.
fn drop_alpha(data: &mut Vec<u8>, color: usize, alpha: usize) {
    let stride = color + alpha;
    let n = data.len() / stride;
    for i in 0..n {
        data.copy_within(i * stride..i * stride + color, i * color);
    }
    data.truncate(n * color);
    data.shrink_to_fit();
}

/// Whether every pixel of `s` over `r` is fully opaque (read from the tiles, no copy).
fn opaque_surface(s: &Surface, r: Rect) -> bool {
    let fmt = s.format();
    let (ch, bps) = (fmt.channels(), fmt.sample.bytes());
    let a = ch - 1;
    let alpha_ok = |px: &[u8]| photocraft_color::read_sample(px, fmt.sample, a) >= 1.0;
    let mut default = vec![0u8; ch * bps];
    photocraft_raster::encode_pixel(&fmt, &s.default_pixel(), &mut default);
    r.tiles().all(|tc| {
        let tr = tc.rect().intersect(&r);
        match s.tile(tc) {
            None => alpha_ok(&default),
            Some(t) => (tr.y0..tr.y1).all(|y| {
                let row = ((y - tc.rect().y0) as usize * TILE_SIZE as usize + (tr.x0 - tc.rect().x0) as usize) * ch * bps;
                t.bytes()[row..row + tr.width() as usize * ch * bps].chunks_exact(ch * bps).all(alpha_ok)
            }),
        }
    })
}

/// Flattens and encodes as `format`.
pub fn export_flat(doc: &Document, format: Format, opts: &ExportOptions) -> Result<ExportResult, IoError> {
    use photocraft_cms::{Builtin, Intent};
    let mut warnings = Vec::new();
    let mut img = document_to_image(doc, &mut warnings)?;
    if opts.xmp == XmpEmbed::None {
        // Export As's Metadata: None: the packet lists the text of every type layer and one id
        // per placed document (#647).
        img.meta.xmp = None;
    }
    if img.layout().has_alpha() && !format.caps().alpha {
        // Flattened over white, as saving a transparent document without transparency does.
        img = matte_over_white(&img)?;
        warnings.push(format!("transparency composited over white for {format:?}"));
    }
    if let Some(encoded) = encode_linear(&img, doc, format, &mut warnings)? {
        img = encoded;
    }
    if matches!(format, Format::OpenExr | Format::Hdr) {
        // OpenEXR and Radiance HDR store linear light (read back as linear sRGB, see [`import_flat`]).
        if let Some(linear) = convert_rgb(&img, Builtin::LinearSrgb.profile(), Intent::RelativeColorimetric, false, CSample::F32)? {
            img = linear;
        }
    } else if !format.caps().icc {
        // Untagged files read back as sRGB: convert to it (as Quick Export does) rather than
        // write values that only mean something under the dropped profile.
        if let Some(srgb) = convert_rgb(&img, Builtin::Srgb.profile(), Intent::Perceptual, true, img.sample_type())? {
            img = srgb;
            warnings.push(format!("colours converted to sRGB; {format:?} can't embed the document's colour profile"));
        }
    }
    for w in codecs::fidelity_warnings_with(&img, format, &opts.encode) {
        if w.is_fatal() {
            return Err(IoError::Unsupported(w.to_string()));
        }
        warnings.push(w.to_string());
    }
    let bytes = codecs::encode(&img, format, &opts.encode)?;
    Ok(ExportResult { bytes, warnings })
}

/// `img` with every band of rows mapped by `f` (normalized samples in, normalized `layout`
/// samples out) and stored as `sample`, so no full-size float copy is made. Profile and metadata
/// are kept.
fn map_bands(img: &Image, layout: ChannelLayout, sample: CSample, f: impl Fn(Vec<f32>) -> Vec<f32>) -> Result<Image, IoError> {
    let (w, h) = img.dimensions();
    let row = img.data().len() / (h.max(1) as usize);
    let band = row.max(1) * (BAND_BYTES / row.max(1)).max(1);
    let mut data = try_buffer(img.pixel_count(), layout.channels() * sample.bytes())?;
    for rows in img.data().chunks(band) {
        let n = (rows.len() / row.max(1)) as u32;
        let vals = f(Image::from_raw(w, n, img.layout(), img.sample_type(), rows.to_vec())?.to_normalized());
        data.extend_from_slice(Image::from_normalized(w, n, layout, sample, &vals)?.data());
    }
    Ok(Image::from_raw(w, h, layout, sample, data)?.with_icc(img.icc.clone()).with_meta(img.meta.clone()))
}

/// Straight-alpha pixels composited over white, without the alpha channel.
fn matte_over_white(img: &Image) -> Result<Image, IoError> {
    let layout = img.layout();
    let white = 1.0;
    map_bands(img, layout.without_alpha(), img.sample_type(), |vals| {
        let mut out = Vec::with_capacity(vals.len() / layout.channels() * layout.color_channels());
        for px in vals.chunks_exact(layout.channels()) {
            if let Some((&a, color)) = px.split_last() {
                let a = a.clamp(0.0, 1.0);
                out.extend(color.iter().map(|&c| c * a + white * (1.0 - a)));
            }
        }
        out
    })
}

/// RGB pixels converted from their profile (sRGB when untagged) to `dst`, unclamped, untagged and
/// stored as `sample`. `None` when the image isn't RGB or already holds `dst`'s colours.
fn convert_rgb(img: &Image, dst: &photocraft_cms::Profile, intent: photocraft_cms::Intent, bpc: bool, sample: CSample) -> Result<Option<Image>, IoError> {
    use photocraft_cms::{Builtin, ColorSpace, Profile, Transform};
    if !img.layout().is_rgb() {
        return Ok(None);
    }
    let src =
        img.icc.as_ref().and_then(|b| Profile::parse(b).ok()).filter(|p| p.color_space == ColorSpace::Rgb).unwrap_or_else(|| Builtin::Srgb.profile().clone());
    if src.same_colors(dst) {
        return Ok(None);
    }
    let t = Transform::new(&src, dst, intent, bpc).map_err(|e| IoError::Unsupported(e.to_string()))?;
    let stride = img.layout().channels();
    let out = map_bands(img, img.layout(), sample, |mut vals| {
        t.apply(&mut vals, stride);
        vals
    })?;
    Ok(Some(out.with_icc(None)))
}

/// A linear float document (linear sRGB or linear gray) written to an integer format, or to
/// any non-HDR format when it came from an integer file: encoded to sRGB / sGray and stored at
/// the source file's depth (8-bit for documents created here), so opening and saving a PNG, JPEG
/// or TIFF gives back the same kind of file. Untagged formats get the values untagged (what an
/// untagged file means). `None` when nothing needs encoding.
fn encode_linear(img: &Image, doc: &Document, format: Format, warnings: &mut Vec<String>) -> Result<Option<Image>, IoError> {
    use photocraft_cms::{Builtin, Intent, Profile, Transform};
    if matches!(format, Format::OpenExr | Format::Hdr) || !img.sample_type().is_float() {
        return Ok(None);
    }
    let (linear, dst) = if img.layout().is_rgb() {
        (Builtin::LinearSrgb.profile(), Builtin::Srgb.profile())
    } else if img.layout().is_gray() {
        (Builtin::LinearGray.profile(), Builtin::SGray.profile())
    } else {
        return Ok(None);
    };
    let Some(src) = img.icc.as_ref().and_then(|b| Profile::parse(b).ok()) else { return Ok(None) };
    if src.color_space != linear.color_space || !src.same_colors(linear) {
        return Ok(None);
    }
    let depths = format.caps().depths;
    let integers: Vec<CSample> = depths.iter().copied().filter(|s| !s.is_float()).collect();
    if integers.is_empty() || (doc.source_depth.is_none() && depths.iter().any(|s| s.is_float())) {
        return Ok(None);
    }
    let want = csample(doc.source_depth.unwrap_or(SampleType::U8));
    let sample = if integers.contains(&want) {
        want
    } else if integers.contains(&CSample::U16) {
        CSample::U16
    } else {
        integers[0]
    };
    let t = Transform::new(&src, dst, Intent::RelativeColorimetric, false).map_err(|e| IoError::Unsupported(e.to_string()))?;
    let stride = img.layout().channels();
    let out = map_bands(img, img.layout(), sample, |mut vals| {
        t.apply(&mut vals, stride);
        vals
    })?;
    if !format.caps().icc {
        warnings.push(format!("colours converted to sRGB; {format:?} can't embed the document's colour profile"));
        return Ok(Some(out.with_icc(None)));
    }
    Ok(Some(out.with_icc(Some(dst.to_bytes().to_vec()))))
}

/// Colour-managed CMYK → sRGB for formats that cannot store CMYK (the document's embedded
/// CMYK profile when it parses, else the built-in coated CMYK; relative colorimetric + BPC).
fn cmyk_image_to_srgb(img: &Image) -> Result<Image, IoError> {
    use photocraft_cms::{Builtin, ColorSpace, Intent, Profile, Transform};
    let src = img
        .icc
        .as_ref()
        .and_then(|b| Profile::parse(b).ok())
        .filter(|p| p.color_space == ColorSpace::Cmyk)
        .unwrap_or_else(|| Builtin::CoatedCmyk.profile().clone());
    let dst = Builtin::Srgb.profile();
    let t = Transform::new(&src, dst, Intent::RelativeColorimetric, true).map_err(|e| IoError::Unsupported(e.to_string()))?;
    let alpha = img.layout().has_alpha();
    let (ss, ds) = (if alpha { 5 } else { 4 }, if alpha { 4 } else { 3 });
    let vals = img.to_normalized();
    let mut out = vec![0.0f32; img.pixel_count() * ds];
    t.convert_f32(&vals, ss, &mut out, ds, true);
    let (w, h) = img.dimensions();
    let layout = if alpha { ChannelLayout::Rgba } else { ChannelLayout::Rgb };
    let sample = match img.sample_type() {
        CSample::F16 => CSample::F32,
        s => s,
    };
    Ok(Image::from_normalized(w, h, layout, sample, &out)?.with_icc(Some(dst.to_bytes().to_vec())).with_meta(img.meta.clone()))
}
