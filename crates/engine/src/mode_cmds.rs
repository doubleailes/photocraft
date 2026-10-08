//! Image › Image Rotation › Arbitrary. (Image › Mode's palette and ink modes — Indexed Color,
//! Color Table, Bitmap, Duotone — are gone: PhotoCraft documents are RGB or grayscale, see
//! `docs/ocio-migration.md`; files in those modes are converted when they are opened.)

use photocraft_algo::transform::{Homography, Interp};
use photocraft_doc::{LayerContent, Size};
use photocraft_geom::Affine;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn num(p: &Value, key: &str, default: f32) -> f32 {
    p.get(key).and_then(Value::as_f64).map_or(default, |v| v as f32)
}

fn str_or<'a>(p: &'a Value, key: &str, default: &'a str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or(default)
}

type Enabled = std::result::Result<(), String>;

fn has_doc(s: &Session) -> Enabled {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}

// ---------- Image Rotation › Arbitrary ----------

/// Canvas size after rotating `size` by `deg` (the canvas grows to fit, as in Photoshop).
pub fn rotated_size(size: Size, deg: f64) -> Size {
    let (s, c) = deg.to_radians().sin_cos();
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    let nw = (w * c.abs() + h * s.abs() - 1e-6).ceil().max(1.0);
    let nh = (w * s.abs() + h * c.abs() - 1e-6).ceil().max(1.0);
    Size::new(nw as u32, nh as u32)
}

fn rotate_arbitrary(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "image.rotation.arbitrary";
    let angle = f64::from(num(p, "angle", 0.0));
    if !angle.is_finite() || angle.abs() > 3600.0 {
        return Err(bad(CMD, "angle out of range"));
    }
    let deg = match str_or(p, "direction", "cw") {
        "cw" => angle,
        "ccw" => -angle,
        d => return Err(bad(CMD, format!("direction `{d}` (cw|ccw)"))),
    };
    if deg.rem_euclid(360.0).abs() < 1e-9 {
        return Ok(json!({"width": s.active().map(|d| d.doc.size.width), "height": s.active().map(|d| d.doc.size.height)}));
    }
    let bg = s.bg();
    let size = s.edit("Rotate Canvas", |doc, _| {
        let old = doc.size;
        let new = rotated_size(old, deg);
        let (sn, cs) = deg.to_radians().sin_cos();
        let (cx, cy) = (f64::from(old.width) / 2.0, f64::from(old.height) / 2.0);
        let (nx, ny) = (f64::from(new.width) / 2.0, f64::from(new.height) / 2.0);
        // Clockwise on screen (y down): x' = c·x − s·y, y' = s·x + c·y about the centres.
        let a = Affine { m: [cs, sn, -sn, cs, nx - (cs * cx - sn * cy), ny - (sn * cx + cs * cy)] };
        let h = Homography([a.m[0], a.m[2], a.m[4], a.m[1], a.m[3], a.m[5], 0.0, 0.0, 1.0]);
        let interp = Interp::parse(str_or(p, "interpolation", "bicubic"));
        doc.size = new;
        let canvas = doc.bounds();
        let fmt = doc.pixel_format();
        let dpi = doc.resolution_dpi;
        for l in doc.layers.iter_mut() {
            let background = l.name == "Background" && l.locks.position && matches!(l.content, LayerContent::Raster(_));
            if background && let Some(surf) = l.surface_mut() {
                // The Background stays a Background: rotated pixels over the background colour.
                let src = surf.content_bounds();
                let rotated = photocraft_algo::transform::warp_surface(surf, src, &h, interp);
                let mut base = Surface::new(fmt);
                let fill = photocraft_raster::from_rgba(&fmt, bg);
                base.write_region(canvas, &fill.repeat(canvas.width() as usize * canvas.height() as usize));
                crate::transform_cmds::composite_over(&mut base, &rotated);
                base.prune();
                *surf = base;
                if let Some(m) = l.mask.as_mut() {
                    m.surface = crate::transform_cmds::warp_gray(&m.surface, &h, interp);
                }
            } else {
                // Rotating the whole image moves locked layers too.
                let locks = l.locks;
                l.locks.position = false;
                l.locks.all = false;
                crate::transform_cmds::transform_layer(None, photocraft_doc::Locks::default(), l, &h, Some(a), interp)?;
                l.locks = locks;
            }
            // What Free Transform leaves alone: unlinked vector masks, gradient angles, artboards…
            crate::canvas_geom::transform_layer_geometry(l, &a, false, dpi);
        }
        crate::canvas_geom::transform_doc_marks(doc, &a);
        for ch in doc.channels.iter_mut().chain(doc.quick_mask.as_mut()) {
            ch.surface = crate::transform_cmds::warp_gray(&ch.surface, &h, interp);
        }
        if let Some(sel) = &doc.selection {
            doc.selection = Some(crate::transform_cmds::warp_gray(sel, &h, Interp::Bilinear)).filter(|s| !s.content_bounds().is_empty());
        }
        // Type, shapes and smart objects re-render from their new geometry.
        crate::canvas_geom::refresh(doc, crate::canvas_geom::Refresh::All);
        doc.guides = Default::default();
        Ok(new)
    })?;
    Ok(json!({"width": size.width, "height": size.height}))
}

macro_rules! spec {
    ($id:literal, $label:literal, [$($m:literal),*], $params:literal, $en:expr, $run:expr) => {
        CommandSpec { id: $id, label: $label, menu: &[$($m),*], shortcut: None, params: $params, enabled: $en, run: $run, journal: true }
    };
}

pub fn specs() -> Vec<CommandSpec> {
    vec![spec!(
        "image.rotation.arbitrary",
        "Arbitrary…",
        ["Image", "Image Rotation"],
        r##"{"angle":-359.99..359.99=0,"direction":"cw|ccw"="cw","interpolation":"bicubic|bilinear|nearest"="bicubic"}"##,
        has_doc,
        rotate_arbitrary
    )]
}

#[cfg(test)]
mod tests;
