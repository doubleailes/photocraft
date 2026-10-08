//! Linear half-float documents (`docs/ocio-migration.md`, half-float step 2).
//!
//! Every RGB or grayscale document that a file, File › New or the clipboard brings in is RGBA
//! half float in a linear version of the working space (linear sRGB, linear gray). Integer
//! sources are converted at the door; float sources (EXR, HDR, 32-bit PSD) are already linear
//! and keep their depth. CMYK, Lab, Indexed, Bitmap, Duotone and Multichannel documents keep
//! their own depth and encoding until those modes are removed.
//!
//! Tool colours (foreground, background, swatches, colour parameters) are picked in the working
//! RGB space, encoded as usual. [`Session::to_doc_color`] converts one into the active
//! document's space where it enters pixels or document data, and [`Session::from_doc_color`]
//! converts a document colour back for the picker (eyedropper, samplers).

use std::sync::Arc;

use photocraft_cms::{Builtin, ColorSpace, Intent, Profile, Transform, TransformOptions};
use photocraft_color::{Color, ColorMode, SampleType};
use photocraft_doc::{Document, Effect, Fill, FxPaint};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::color_cmds::{convert_document, convert_surface, document_profile, mode_space, profile_from_bytes};
use crate::edit_cmds::Clip;
use crate::{Result, Session};

/// The linear profile documents of `mode` are stored in, when that mode is linearised.
pub fn linear_profile(mode: ColorMode) -> Option<&'static Profile> {
    match mode_space(mode) {
        ColorSpace::Rgb => Some(Builtin::LinearSrgb.profile()),
        ColorSpace::Gray => Some(Builtin::LinearGray.profile()),
        _ => None,
    }
}

/// Is `doc` a linear document (tagged with its mode's linear profile)?
pub fn is_linear(doc: &Document) -> bool {
    let Some(lin) = linear_profile(doc.mode) else { return false };
    let p = document_profile(doc);
    p.content_hash() == lin.content_hash() || p.same_colors(lin)
}

/// Sets the sample type of every surface (layers, masks, alpha channels, quick mask).
pub(crate) fn set_document_depth(doc: &mut Document, depth: SampleType) {
    crate::image_cmds::convert_layers_depth(&mut doc.layers, depth);
    for ch in doc.channels.iter_mut().chain(doc.quick_mask.as_mut()) {
        let f = ch.surface.format().with_sample(depth);
        ch.surface = ch.surface.convert(f);
    }
    doc.depth = depth;
}

/// Converts an integer RGB or grayscale document to linear half float: pixels and the
/// colours of fills, text, shapes and effects go from the document's profile to its mode's
/// linear profile. Returns whether the document changed. Float documents and other modes are
/// left as they are: float files hold linear values whatever profile they carry (Photoshop's
/// 32-bit documents embed the working profile).
pub fn linearize(doc: &mut Document) -> Result<bool> {
    if doc.depth.is_float() {
        return Ok(false);
    }
    to_linear(doc)
}

/// Converts any RGB or grayscale document whose values are encoded in a non-linear profile
/// (an integer file, or the float result of a command that works on encoded values) to linear
/// half float; 32-bit float documents stay 32-bit.
pub(crate) fn to_linear(doc: &mut Document) -> Result<bool> {
    let Some(lin) = linear_profile(doc.mode) else { return Ok(false) };
    if is_linear(doc) {
        return Ok(false);
    }
    let depth = if doc.depth == SampleType::F32 { SampleType::F32 } else { SampleType::F16 };
    // Through 32-bit float, so 16-bit sources keep their precision until the final rounding.
    let mut work = doc.clone();
    if !doc.depth.is_float() {
        work.source_depth = Some(doc.depth);
    }
    set_document_depth(&mut work, SampleType::F32);
    convert_document(&mut work, lin, Intent::RelativeColorimetric, false)?;
    set_document_depth(&mut work, depth);
    *doc = work;
    Ok(true)
}

/// Re-encodes a linear document in `dst` (its depth kept), for algorithms that work on
/// display-encoded values (Merge to HDR Pro's response curve, Photomerge's blending).
pub(crate) fn to_encoded(doc: &mut Document, dst: &Profile) -> Result<bool> {
    if !is_linear(doc) {
        return Ok(false);
    }
    convert_document(doc, dst, Intent::RelativeColorimetric, false)?;
    Ok(true)
}

/// [`linearize`] for a document arriving from outside (a file, the clipboard, an automation
/// client). A failed conversion keeps the document as it was; the report says why.
pub fn linearize_import(doc: &mut Document) -> Value {
    match linearize(doc) {
        Ok(changed) => json!({"linearized": changed}),
        Err(e) => json!({"linearized": false, "linearizeError": e.to_string()}),
    }
}

/// The RGB profile tool colours are converted into for `doc`, or `None` to use them as they
/// are (non-linear gray documents and modes without an RGB composite).
fn tool_space(doc: &Document) -> Option<Arc<Profile>> {
    match mode_space(doc.mode) {
        ColorSpace::Rgb => Some(document_profile(doc)),
        // Gray pixels take the luminance of an RGB colour, so linear gray paints linear RGB.
        ColorSpace::Gray if is_linear(doc) => Some(Arc::new(Builtin::LinearSrgb.profile().clone())),
        _ => None,
    }
}

/// Converts colours between the picker and one document's pixel values (identity for
/// documents whose space is the picker's). Cheap to clone, so edit closures can carry it.
#[derive(Clone, Default)]
pub struct ColorConv(Option<Arc<Transform>>);

impl ColorConv {
    fn between(src: &Profile, dst: &Profile) -> Self {
        if src.content_hash() == dst.content_hash() {
            return ColorConv(None);
        }
        ColorConv(photocraft_cms::cached(src, dst, options()).ok())
    }

    /// Picked colours (in `picker`) to `doc`'s pixel values.
    pub fn for_doc(doc: &Document, picker: &Profile) -> Self {
        match tool_space(doc) {
            Some(dst) => Self::between(picker, &dst),
            None => ColorConv(None),
        }
    }

    /// `doc`'s pixel values to picked colours (in `picker`).
    pub fn from_doc(doc: &Document, picker: &Profile) -> Self {
        match tool_space(doc) {
            Some(src) => Self::between(&src, picker),
            None => ColorConv(None),
        }
    }

    pub fn apply(&self, c: [f32; 4]) -> [f32; 4] {
        let Some(t) = &self.0 else { return c };
        let mut out = [0.0f32; 16];
        t.eval(&c[..3], &mut out);
        [out[0], out[1], out[2], c[3]]
    }

    /// [`Self::apply`] for an RGB [`Color`] (other modes are returned as they are).
    pub fn color(&self, c: Color) -> Color {
        if c.mode != ColorMode::Rgb || self.0.is_none() {
            return c;
        }
        let [r, g, b, _] = self.apply([c.c[0], c.c[1], c.c[2], 1.0]);
        Color { c: [r, g, b, c.c[3]], ..c }
    }

    /// Converts the colours of a fill (solid colour, gradient stops) in place.
    pub fn fill(&self, f: &mut Fill) {
        match f {
            Fill::Solid(c) => *c = self.color(*c),
            Fill::Gradient { stops, .. } => stops.iter_mut().for_each(|s| s.1 = self.color(s.1)),
            Fill::Pattern { .. } => {}
        }
    }

    /// Converts every colour of a layer effect in place.
    pub fn effect(&self, e: &mut Effect) {
        if self.0.is_none() {
            return;
        }
        match e {
            Effect::DropShadow(s) | Effect::InnerShadow(s) => s.color = self.color(s.color),
            Effect::OuterGlow(g) | Effect::InnerGlow(g) => self.paint(&mut g.paint),
            Effect::Stroke(s) => self.paint(&mut s.paint),
            Effect::ColorOverlay { color, .. } => *color = self.color(*color),
            Effect::GradientOverlay { gradient, .. } => gradient.stops.iter_mut().for_each(|s| s.1 = self.color(s.1)),
            Effect::Satin(s) => s.color = self.color(s.color),
            Effect::BevelEmboss(b) => {
                b.highlight_color = self.color(b.highlight_color);
                b.shadow_color = self.color(b.shadow_color);
            }
            Effect::PatternOverlay { .. } => {}
        }
    }

    /// Converts the colours of an effect paint (colour, gradient stops) in place.
    pub fn paint(&self, p: &mut FxPaint) {
        match p {
            FxPaint::Color(c) => *c = self.color(*c),
            FxPaint::Gradient(g) => g.stops.iter_mut().for_each(|s| s.1 = self.color(s.1)),
            FxPaint::Pattern { .. } => {}
        }
    }
}

fn convert_color(src: &Profile, dst: &Profile, c: [f32; 4]) -> [f32; 4] {
    ColorConv::between(src, dst).apply(c)
}

/// `c`, picked in `picker`, in the pixel values of a new linear document of `mode` (`c` itself
/// for modes that are not linearised).
pub fn to_linear_color(mode: ColorMode, picker: &Profile, c: [f32; 4]) -> [f32; 4] {
    match linear_profile(mode) {
        Some(_) => convert_color(picker, Builtin::LinearSrgb.profile(), c),
        None => c,
    }
}

/// `c`, picked in `picker` (the working RGB space), in `doc`'s pixel values.
pub fn to_doc_color_for(doc: &Document, picker: &Profile, c: [f32; 4]) -> [f32; 4] {
    match tool_space(doc) {
        Some(dst) => convert_color(picker, &dst, c),
        None => c,
    }
}

/// `c`, a pixel value of `doc`, in `picker` (the working RGB space).
pub fn from_doc_color_for(doc: &Document, picker: &Profile, c: [f32; 4]) -> [f32; 4] {
    match tool_space(doc) {
        Some(src) => {
            let [r, g, b, a] = convert_color(&src, picker, c);
            [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0), a]
        }
        None => c,
    }
}

fn options() -> TransformOptions {
    TransformOptions { intent: Intent::RelativeColorimetric, bpc: false, precise_float: true }
}

/// `surf` (pixels in `src`) in `dst`, as 32-bit float so no precision is lost on the way to a
/// linear space. Unchanged when the profiles are the same or describe another colour space
/// than the surface's mode.
pub fn convert_pixels(surf: &Surface, src: &Profile, dst: &Profile) -> Surface {
    let fmt = surf.format();
    let space = mode_space(fmt.mode);
    if src.content_hash() == dst.content_hash() || src.color_space != space || dst.color_space != space {
        return surf.clone();
    }
    let Ok(t) = photocraft_cms::cached(src, dst, options()) else { return surf.clone() };
    let wide = if fmt.sample == SampleType::F32 { surf.clone() } else { surf.convert(fmt.with_sample(SampleType::F32)) };
    convert_surface(&wide, fmt.mode, wide.format(), &t)
}

/// A colour parameter: `"#rrggbb"`, `"#rrggbbaa"` (with or without `#`) or `[r, g, b, a?]`.
fn color_value(v: &Value) -> Option<[f32; 4]> {
    match v {
        Value::Array(a) if a.len() >= 3 => {
            let c = |i: usize, d: f64| a.get(i).and_then(Value::as_f64).unwrap_or(d) as f32;
            Some([c(0, 0.0), c(1, 0.0), c(2, 0.0), c(3, 1.0)])
        }
        Value::String(s) => {
            let h = s.trim_start_matches('#');
            let b = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).map(|v| f32::from(v) / 255.0);
            match h.len() {
                6 => Some([b(0)?, b(2)?, b(4)?, 1.0]),
                8 => Some([b(0)?, b(2)?, b(4)?, b(6)?]),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `p` with the colours under `keys` (picked ones, top level and in a `runs` list) as the
/// document's values (`[r, g, b, a]`), for parsers that store colours as they read them.
pub fn colors_in(p: &Value, keys: &[&str], to_doc: &ColorConv) -> Value {
    fn fix(v: &mut Value, keys: &[&str], to_doc: &ColorConv) {
        if let Value::Object(m) = v {
            for k in keys {
                if let Some(c) = m.get(*k).and_then(color_value) {
                    m.insert((*k).into(), json!(to_doc.apply(c)));
                }
            }
        }
    }
    let mut out = p.clone();
    if to_doc.0.is_none() {
        return out;
    }
    fix(&mut out, keys, to_doc);
    if let Some(Value::Array(runs)) = out.get_mut("runs") {
        runs.iter_mut().for_each(|r| fix(r, keys, to_doc));
    }
    out
}

impl Session {
    /// The profile of the clipboard's pixels: the one recorded at copy, else (an image from
    /// another app) the working space of its mode.
    pub fn clip_profile(&self, clip: &Clip) -> Arc<Profile> {
        let mode = clip.surface.format().mode;
        clip.icc_profile
            .as_ref()
            .and_then(|b| profile_from_bytes(b).ok())
            .filter(|p| p.color_space == mode_space(mode))
            .unwrap_or_else(|| self.color.working(mode))
    }

    /// The clipboard's pixels in `doc`'s profile (see [`convert_pixels`]), for Paste.
    pub fn clip_pixels_for(&self, clip: &Clip, surf: &Surface, doc: &Document) -> Surface {
        convert_pixels(surf, &self.clip_profile(clip), &document_profile(doc))
    }

    /// The clipboard as 8-bit RGBA in the working space, for other apps. `None` when empty.
    pub fn clipboard_rgba8(&self) -> Option<(Rect, Vec<[u8; 4]>)> {
        let clip = self.clipboard.as_ref()?;
        let b = clip.bounds;
        if b.is_empty() {
            return None;
        }
        let working = self.color.working(clip.surface.format().mode);
        let surf = convert_pixels(&clip.surface, &self.clip_profile(clip), &working);
        let mut px = vec![[0u8; 4]; b.width() as usize * b.height() as usize];
        surf.read_rgba8_into(b, &mut px);
        Some((b, px))
    }

    /// The space tool colours are picked in: the working RGB profile.
    pub fn picker_profile(&self) -> Arc<Profile> {
        self.color.working(ColorMode::Rgb)
    }

    /// The converter from picked colours to the active document's pixel values.
    pub fn to_doc(&self) -> ColorConv {
        match self.active() {
            Some(d) => ColorConv::for_doc(&d.doc, &self.picker_profile()),
            None => ColorConv(None),
        }
    }

    /// The converter from the active document's pixel values to picked colours (unclamped).
    pub fn from_doc(&self) -> ColorConv {
        match self.active() {
            Some(d) => ColorConv::from_doc(&d.doc, &self.picker_profile()),
            None => ColorConv(None),
        }
    }

    /// A picked colour (foreground, background, a colour parameter) in the active document's
    /// pixel values. Unchanged without a document.
    pub fn to_doc_color(&self, c: [f32; 4]) -> [f32; 4] {
        self.to_doc().apply(c)
    }

    /// The foreground colour in the active document's pixel values.
    pub fn fg(&self) -> [f32; 4] {
        self.to_doc_color(self.tools.foreground)
    }

    /// The background colour in the active document's pixel values.
    pub fn bg(&self) -> [f32; 4] {
        self.to_doc_color(self.tools.background)
    }

    /// [`Self::to_doc_color`] for an RGB [`Color`] (other modes are returned as they are).
    pub fn to_doc_rgb(&self, c: Color) -> Color {
        self.to_doc().color(c)
    }

    /// A pixel value of the active document as a picked colour (eyedropper, samplers).
    pub fn from_doc_color(&self, c: [f32; 4]) -> [f32; 4] {
        match self.active() {
            Some(d) => from_doc_color_for(&d.doc, &self.picker_profile(), c),
            None => c,
        }
    }
}

#[cfg(test)]
mod tests;
