use photocraft_cms::Builtin;
use photocraft_color::{Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer, LayerContent, Size};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::json;

use super::*;
use crate::Session;

fn srgb_decode(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// A 256×1 document holding every 8-bit (or a 16-bit ramp of) grey/colour value.
fn ramp_doc(depth: SampleType, n: u32) -> Document {
    let mut d = Document::new("ramp", Size::new(n, 1), ColorMode::Rgb, depth);
    let fmt = PixelFormat::new(ColorMode::Rgb, depth, true);
    let mut s = Surface::new(fmt);
    let px: Vec<f32> = (0..n)
        .flat_map(|i| {
            let v = i as f32 / (n - 1) as f32;
            [v, 1.0 - v, (v * 7.0).fract(), 1.0]
        })
        .collect();
    s.write_region(Rect::new(0, 0, n as i32, 1), &px);
    d.layers.push(Layer::new("Background", LayerContent::Raster(s)));
    d
}

fn px(s: &Session, x: i32, y: i32) -> Vec<f32> {
    let d = &s.active().unwrap().doc;
    d.layers.last().unwrap().surface().unwrap().pixel(x, y).to_vec()
}

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn opening_an_8_bit_file_gives_linear_half_float_and_records_the_source_depth() {
    let mut s = Session::new();
    let (_, report) = s.open_document(ramp_doc(SampleType::U8, 256), None);
    assert_eq!(report["linearized"], true);
    let d = &s.active().unwrap().doc;
    assert_eq!(d.depth, SampleType::F16);
    assert_eq!(d.source_depth, Some(SampleType::U8));
    assert!(is_linear(d));
    assert_eq!(d.layers[0].surface().unwrap().format().sample, SampleType::F16);
    for x in [0, 1, 10, 128, 200, 255] {
        let v = x as f32 / 255.0;
        assert!(near(px(&s, x, 0)[0], srgb_decode(v), 2e-3 * srgb_decode(v).max(0.01)), "x {x}: {} vs {}", px(&s, x, 0)[0], srgb_decode(v));
    }
}

#[test]
fn a_linearised_file_with_another_profile_does_not_ask_what_to_do() {
    let mut s = Session::new();
    let mut d = ramp_doc(SampleType::U8, 16);
    crate::color_cmds::tag_with_profile(&mut d, Builtin::AdobeRgbCompat.profile());
    let (_, r) = s.open_document(d, None);
    assert_eq!((r["mismatch"].as_bool(), r["ask"].as_bool(), r["linearized"].as_bool()), (Some(true), Some(false), Some(true)));
    assert!(is_linear(&s.active().unwrap().doc));
}

#[test]
fn every_8_bit_value_survives_open_and_save_as_png() {
    let original = ramp_doc(SampleType::U8, 256);
    let mut doc = original.clone();
    assert!(linearize(&mut doc).unwrap());
    let out = photocraft_io::export(&doc, "png", &Default::default()).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let back = photocraft_io::import("t.png", &out.bytes).unwrap().document;
    assert_eq!(back.depth, SampleType::U8, "saved back at the source depth");
    let a = original.layers[0].surface().unwrap();
    let b = back.layers[0].surface().unwrap();
    for x in 0..256 {
        let (p, q) = (a.pixel(x, 0), b.pixel(x, 0));
        for c in 0..3 {
            assert!(near(p[c], q[c], 0.1 / 255.0), "x {x} c {c}: {} vs {}", p[c], q[c]);
        }
    }
}

#[test]
fn a_16_bit_source_saves_back_as_16_bit_within_half_float_precision() {
    let original = ramp_doc(SampleType::U16, 4096);
    let mut doc = original.clone();
    linearize(&mut doc).unwrap();
    assert_eq!(doc.source_depth, Some(SampleType::U16));
    let out = photocraft_io::export(&doc, "png", &Default::default()).unwrap();
    let back = photocraft_io::import("t.png", &out.bytes).unwrap().document;
    assert_eq!(back.depth, SampleType::U16);
    let (a, b) = (original.layers[0].surface().unwrap(), back.layers[0].surface().unwrap());
    let worst = (0..4096).flat_map(|x| (0..3).map(move |c| (x, c))).map(|(x, c)| (a.pixel(x, 0)[c] - b.pixel(x, 0)[c]).abs()).fold(0.0f32, f32::max);
    assert!(worst < 1e-3, "worst error {worst}");
}

#[test]
fn exr_export_stays_linear_float_and_jpeg_is_encoded() {
    let mut doc = ramp_doc(SampleType::U8, 64);
    linearize(&mut doc).unwrap();
    let jpg = photocraft_io::export(&doc, "jpg", &Default::default()).unwrap();
    let back = photocraft_io::import("t.jpg", &jpg.bytes).unwrap().document;
    let mid = back.layers[0].surface().unwrap().pixel(32, 0)[0];
    assert!(near(mid, 32.0 / 63.0, 0.03), "JPEG holds sRGB-encoded values, not linear ones: {mid}");
    let exr = photocraft_io::export(&doc, "exr", &Default::default()).unwrap();
    let back = photocraft_io::import("t.exr", &exr.bytes).unwrap().document;
    assert!(back.depth.is_float());
    let mid = back.layers[0].surface().unwrap().pixel(32, 0)[0];
    assert!(near(mid, srgb_decode(32.0 / 63.0), 2e-3), "EXR keeps linear values: {mid}");
}

#[test]
fn float_files_are_left_alone() {
    let mut f32doc = ramp_doc(SampleType::F32, 8);
    crate::color_cmds::tag_with_profile(&mut f32doc, Builtin::Srgb.profile());
    assert!(!linearize(&mut f32doc).unwrap(), "32-bit files are linear whatever their tag");
}

#[test]
fn grayscale_files_become_linear_gray() {
    let mut d = Document::with_background("g", Size::new(4, 4), ColorMode::Grayscale, SampleType::U8, Color::gray(0.5));
    assert!(linearize(&mut d).unwrap());
    assert_eq!((d.mode, d.depth), (ColorMode::Grayscale, SampleType::F16));
    assert!(is_linear(&d));
    let v = d.layers[0].surface().unwrap().pixel(1, 1)[0];
    assert!(near(v, srgb_decode(0.5), 3e-3), "{v}");
}

#[test]
fn file_new_is_linear_half_float_and_converts_the_background_colour() {
    let mut s = Session::new();
    s.execute("tools.setColors", json!({"background": "#808080"})).unwrap();
    s.execute("file.new", json!({"width": 8, "height": 8, "background": "backgroundColor"})).unwrap();
    let d = &s.active().unwrap().doc;
    assert_eq!((d.depth, d.source_depth), (SampleType::F16, None));
    assert!(is_linear(d));
    assert!(near(px(&s, 1, 1)[0], srgb_decode(128.0 / 255.0), 1e-3));
    s.execute("file.new", json!({"width": 8, "height": 8, "depth": 32})).unwrap();
    let d = &s.active().unwrap().doc;
    assert_eq!(d.depth, SampleType::F32);
    assert!(is_linear(d));
    // Grayscale too; integer depths are not offered.
    s.execute("file.new", json!({"width": 8, "height": 8, "mode": "gray", "depth": 8})).unwrap();
    let d = &s.active().unwrap().doc;
    assert_eq!(d.depth, SampleType::F16);
    assert!(is_linear(d));
}

#[test]
fn picked_colours_paint_as_picked_and_the_eyedropper_reads_them_back() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 16, "height": 16, "background": "transparent"})).unwrap();
    s.execute("tools.setColors", json!({"foreground": "#808080"})).unwrap();
    s.execute("edit.fill", json!({"contents": "foreground"})).unwrap();
    let lin = srgb_decode(128.0 / 255.0);
    assert!(near(px(&s, 3, 3)[0], lin, 1e-3), "{}", px(&s, 3, 3)[0]);
    let back = s.from_doc_color([px(&s, 3, 3)[0], px(&s, 3, 3)[1], px(&s, 3, 3)[2], 1.0]);
    assert!(near(back[0], 128.0 / 255.0, 1e-3), "{back:?}");
    // An explicit colour parameter is a picked colour too.
    s.execute("edit.fill", json!({"contents": "color", "color": "#ff8000"})).unwrap();
    let p = px(&s, 3, 3);
    assert!(near(p[0], 1.0, 1e-3) && near(p[1], srgb_decode(128.0 / 255.0), 1e-3) && near(p[2], 0.0, 1e-3), "{p:?}");
}

#[test]
fn integer_documents_are_converted_as_they_enter_a_session() {
    let mut s = Session::new();
    s.add_document(Document::with_background("u8", Size::new(4, 4), ColorMode::Rgb, SampleType::U8, Color::rgb(0.5, 0.5, 0.5)), None);
    let d = &s.active().unwrap().doc;
    assert_eq!(d.depth, SampleType::F16);
    assert!(is_linear(d));
    // 0.5 stored as 8-bit level 128.
    assert!(near(px(&s, 1, 1)[0], srgb_decode(128.0 / 255.0), 1e-3), "{:?}", px(&s, 1, 1));
    // Picked colours convert into it like into any document.
    assert!(near(s.to_doc_color([0.5, 0.5, 0.5, 1.0])[0], srgb_decode(0.5), 1e-3));
    assert!(near(s.from_doc_color([srgb_decode(0.5), 0.0, 0.0, 1.0])[0], 0.5, 1e-3));
}

#[test]
fn shapes_text_fill_layers_and_styles_store_document_values() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
    let lin = srgb_decode(128.0 / 255.0);
    let layer = |s: &Session| s.active().unwrap().doc.layers.last().unwrap().clone();

    s.execute("shape.create", json!({"kind": "rect", "rect": [0, 0, 8, 8], "fill": "#808080"})).unwrap();
    let LayerContent::Shape(sh) = layer(&s).content else { panic!("shape") };
    let Some(photocraft_doc::Fill::Solid(c)) = sh.fill else { panic!("solid") };
    assert!(near(c.c[0], lin, 1e-3), "{c:?}");

    s.execute("layer.newFillLayer.solidColor", json!({"color": "#808080"})).unwrap();
    let LayerContent::Fill(photocraft_doc::Fill::Solid(c)) = layer(&s).content else { panic!("fill") };
    assert!(near(c.c[0], lin, 1e-3), "{c:?}");

    s.execute("type.create", json!({"x": 2, "y": 20, "text": "Hi", "color": "#808080"})).unwrap();
    let LayerContent::Text(t) = layer(&s).content else { panic!("text") };
    assert!(near(t.color.c[0], lin, 1e-3) || t.runs.iter().all(|r| near(r.style.color.c[0], lin, 1e-3)), "{:?}", t.color);

    s.execute("layer.layerStyle.colorOverlay", json!({"color": "#808080"})).unwrap();
    let fx = layer(&s).effects.items.clone();
    let Some(photocraft_doc::Effect::ColorOverlay { color, .. }) = fx.first() else { panic!("overlay") };
    assert!(near(color.c[0], lin, 1e-3), "{color:?}");
}

#[test]
fn filter_colours_convert_once_even_when_the_params_are_replayed() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    s.execute("tools.setColors", json!({"foreground": "#808080"})).unwrap();
    let once = crate::filters_ext::prepare(&s, "filter.render.fibers", &json!({}));
    assert_eq!(once["colorsInDocument"], true);
    let lin = srgb_decode(128.0 / 255.0);
    assert!(near(once["foreground"][0].as_f64().unwrap() as f32, lin, 1e-3), "{once}");
    let twice = crate::filters_ext::prepare(&s, "filter.render.fibers", &once);
    assert_eq!(once, twice);
}

#[test]
fn copying_from_a_linear_document_gives_other_apps_srgb_and_pastes_convert() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 4, "height": 4})).unwrap();
    s.execute("tools.setColors", json!({"foreground": "#808080"})).unwrap();
    s.execute("edit.fill", json!({"contents": "foreground"})).unwrap();
    s.execute("select.all", json!({})).unwrap();
    s.execute("edit.copy", json!({})).unwrap();
    let (_, px8) = s.clipboard_rgba8().unwrap();
    assert!(px8.iter().all(|p| p[0].abs_diff(128) <= 1), "{:?}", px8[0]);
    // Pasted into a 32-bit document, the grey is the same linear value.
    s.execute("file.new", json!({"width": 4, "height": 4, "depth": 32})).unwrap();
    s.execute("edit.paste", json!({})).unwrap();
    assert!(near(px(&s, 1, 1)[0], srgb_decode(128.0 / 255.0), 1e-3), "{:?}", px(&s, 1, 1));
}

#[test]
fn an_image_from_another_app_opens_linear_with_new_from_clipboard() {
    let mut s = Session::new();
    let r = Rect::new(0, 0, 2, 2);
    let surface = Surface::from_interleaved(PixelFormat::RGBA8, r, &[128u8, 128, 128, 255].repeat(4));
    s.clipboard = Some(crate::edit_cmds::Clip { surface, bounds: r, icc_profile: None });
    s.execute("edit.paste", json!({})).unwrap();
    let d = &s.active().unwrap().doc;
    assert_eq!((d.depth, d.source_depth), (SampleType::F16, Some(SampleType::U8)));
    assert!(near(px(&s, 0, 0)[0], srgb_decode(128.0 / 255.0), 1e-3));
}

#[test]
fn a_wide_gamut_file_keeps_its_colours() {
    let mut d = Document::with_background("p3", Size::new(2, 2), ColorMode::Rgb, SampleType::U8, Color::rgb(1.0, 0.0, 0.0));
    crate::color_cmds::tag_with_profile(&mut d, Builtin::DisplayP3.profile());
    linearize(&mut d).unwrap();
    let p = d.layers[0].surface().unwrap().pixel(0, 0).to_vec();
    // P3 red is outside sRGB: linear sRGB holds it with a red above 1 or negative green/blue.
    assert!(p[0] > 1.0 || p[1] < 0.0 || p[2] < 0.0, "{p:?}");
}

#[test]
fn a_files_own_patterns_become_linear_with_it() {
    let mut d = Document::with_background("p", Size::new(4, 4), ColorMode::Rgb, SampleType::U8, Color::WHITE);
    let mut s = Surface::new(PixelFormat::RGBA8);
    s.fill_rect(Rect::new(0, 0, 2, 2), &[0.5, 0.5, 0.5, 1.0]);
    d.patterns.push(photocraft_doc::Pattern::new("grey", s, 2, 2));
    assert!(linearize(&mut d).unwrap());
    let p = &d.patterns[0].surface;
    assert_eq!(p.format().sample, SampleType::F16);
    assert!(near(p.pixel(1, 1)[0], srgb_decode(128.0 / 255.0), 2e-3), "{:?}", p.pixel(1, 1));
    // Linearising again leaves it alone.
    assert!(!linearize(&mut d).unwrap());
    assert!(near(d.patterns[0].surface.pixel(1, 1)[0], srgb_decode(128.0 / 255.0), 2e-3));
}
