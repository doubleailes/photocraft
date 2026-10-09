//! ICC profiles through import/export: byte-exact PSD round trips with real profiles, PNG/JPEG
//! embedding and extraction, colour-managed CMYK → RGB for formats without CMYK.

mod common;

use std::sync::Arc;

use common::*;
use photocraft_cms::{Builtin, Profile};
use photocraft_color::{ColorMode, SampleType};
use photocraft_io::*;

fn single(mode: ColorMode, depth: SampleType, alpha: bool) -> photocraft_doc::Document {
    let mut d = photocraft_doc::Document::new("s", photocraft_geom::Size::new(9, 6), mode, depth);
    let fmt = d.pixel_format();
    d.layers.push(raster("Background", fmt, d.bounds(), 3, alpha));
    d
}

/// An RGB profile with no named space (Display P3 primaries, gamma 1.5 curves).
fn unnamed_profile() -> Profile {
    let mut p = Builtin::DisplayP3.profile().clone();
    p.trc = Some([photocraft_cms::Curve::Gamma(1.5), photocraft_cms::Curve::Gamma(1.5), photocraft_cms::Curve::Gamma(1.5)]);
    p.description = "Odd gamma 1.5 P3".into();
    p.with_encoded_bytes()
}

#[test]
fn psd_keeps_named_spaces_and_unnamed_profiles() {
    for sp in photocraft_color::space::SPACES {
        for (mode, has) in [(ColorMode::Rgb, sp.rgb.is_some()), (ColorMode::Grayscale, sp.gray.is_some())] {
            if !has {
                continue;
            }
            let mut d = gen_doc(mode, SampleType::U8, Features::PIXELS);
            d.color_space = sp.name.into();
            let r = export(&d, "x.psd", &ExportOptions::default()).unwrap();
            let back = import("x.psd", &r.bytes).unwrap().document;
            assert_eq!(back.color_space, sp.name, "{mode:?}");
            assert_eq!(back.source_space, sp.name, "{mode:?}");
        }
    }
    // A profile with no named space travels byte for byte (the engine converts from it).
    let odd = unnamed_profile().to_bytes().to_vec();
    let mut d = gen_doc(ColorMode::Rgb, SampleType::U16, Features::PIXELS);
    d.metadata.icc = Some(Arc::new(odd.clone()));
    let r = export(&d, "x.psd", &ExportOptions::default()).unwrap();
    let back = import("x.psd", &r.bytes).unwrap();
    assert_eq!(back.document.metadata.icc.as_deref(), Some(&odd));
    assert!(back.document.color_space.is_empty());
    assert!(back.warnings.iter().any(|w| w.contains("Odd gamma 1.5 P3")), "{:?}", back.warnings);
    // The macOS system profiles, when present, map by colour.
    if let Ok(b) = std::fs::read("/System/Library/ColorSync/Profiles/AdobeRGB1998.icc") {
        let mut d = gen_doc(ColorMode::Rgb, SampleType::U8, Features::PIXELS);
        d.metadata.icc = Some(Arc::new(b));
        let r = export(&d, "x.psd", &ExportOptions::default()).unwrap();
        assert_eq!(import("x.psd", &r.bytes).unwrap().document.color_space, "Gamma 2.2 Encoded AdobeRGB");
    }
}

#[test]
fn png_and_jpeg_embed_and_extract_spaces() {
    for b in [Builtin::Srgb, Builtin::DisplayP3, Builtin::AdobeRgbCompat, Builtin::ProPhotoCompat, Builtin::Rec2020] {
        let name = photocraft_color::space::name_for_profile(b.profile()).unwrap();
        for (file, alpha) in [("x.png", true), ("x.jpg", false)] {
            let mut d = single(ColorMode::Rgb, SampleType::U8, alpha);
            d.color_space = name.into();
            let r = export(&d, file, &ExportOptions::default()).unwrap();
            let back = import(file, &r.bytes).unwrap().document;
            assert_eq!(back.color_space, name, "{b:?} {file}");
            // The file embeds the built-in profile of the space.
            let icc = photocraft_codecs::decode(&r.bytes).unwrap().icc.unwrap();
            assert_eq!(Profile::parse(&icc).unwrap().description, b.description());
        }
    }
    // Gray PNG in a gray-only space.
    let mut d = single(ColorMode::Grayscale, SampleType::U16, false);
    d.color_space = "Gamma 2.2 Encoded Rec.709".into();
    let r = export(&d, "g.png", &ExportOptions::default()).unwrap();
    assert_eq!(import("g.png", &r.bytes).unwrap().document.color_space, "Gamma 2.2 Encoded Rec.709");
    // Untagged stays untagged.
    let d = single(ColorMode::Rgb, SampleType::U8, false);
    let r = export(&d, "u.png", &ExportOptions::default()).unwrap();
    assert!(photocraft_codecs::decode(&r.bytes).unwrap().icc.is_none());
    assert!(import("u.png", &r.bytes).unwrap().document.color_space.is_empty());
}

#[test]
fn cmyk_files_open_as_rgb_through_their_profile() {
    // A CMYK TIFF tagged with the coated CMYK profile opens as RGB, untagged sRGB values.
    let cmyk = [0u8, 0, 0, 0, 0, 255, 255, 0];
    let img = photocraft_codecs::Image::from_raw(2, 1, photocraft_codecs::ChannelLayout::Cmyk, photocraft_codecs::SampleType::U8, cmyk.to_vec())
        .unwrap()
        .with_icc(Some(Builtin::CoatedCmyk.profile().to_bytes().to_vec()));
    let bytes = photocraft_codecs::encode(&img, photocraft_codecs::Format::Tiff, &Default::default()).unwrap();
    let r = import("c.tif", &bytes).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("converted to RGB")), "{:?}", r.warnings);
    let d = r.document;
    assert_eq!(d.mode, ColorMode::Rgb);
    assert_eq!(d.color_space, photocraft_color::space::SRGB);
    let s = d.layers[0].surface().unwrap();
    assert!(s.pixel(0, 0)[..3].iter().all(|v| *v > 0.97), "no ink is white: {:?}", s.pixel(0, 0));
    let red = s.pixel(1, 0);
    assert!(red[0] > 0.8 && red[1] < 0.3 && red[2] < 0.3, "magenta + yellow is red: {red:?}");
}
