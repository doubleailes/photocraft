//! Multichannel PSDs (colour mode 7) open as RGB.
//!
//! A Multichannel file has no layers: its image is a set of ink planes, stored the way Photoshop
//! shows them in the Channels panel (0 = solid ink, max = paper). They open as spot channels of an
//! RGB document (ink density as the value, 1 = solid, like every spot channel), and the image is
//! the inks printed over paper (see [`crate::native::print_inks`]). Channel names come from
//! resources 1006 / 1045 and ink colours from DisplayInfo (1077), as for spot channels.

use photocraft_color::{Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{AlphaChannel, Document, Layer, LayerContent};
use photocraft_geom::Rect;
use photocraft_psd::PsdFile;
use photocraft_raster::Surface;

/// Value of sample `i` of a big-endian plane.
fn decode(plane: &[u8], i: usize, s: SampleType) -> f32 {
    match s {
        SampleType::U8 => f32::from(plane[i]) / 255.0,
        SampleType::U16 => f32::from(u16::from_be_bytes([plane[2 * i], plane[2 * i + 1]])) / 65535.0,
        SampleType::F16 => half::f16::from_be_bytes([plane[2 * i], plane[2 * i + 1]]).to_f32(),
        SampleType::F32 => f32::from_be_bytes([plane[4 * i], plane[4 * i + 1], plane[4 * i + 2], plane[4 * i + 3]]),
    }
}

/// Fills `doc` (RGB at the file depth) from the merged planes `all`: the ink channels as spot
/// channels, and their print as the Background layer.
pub(crate) fn import(file: &PsdFile, all: &[u8], doc: &mut Document, names: &[String], warnings: &mut Vec<String>) {
    let h = &file.header;
    let depth = doc.depth;
    let (w, hh) = (h.width as usize, h.height as usize);
    let n = w * hh;
    let plane = h.row_bytes() * hh;
    let canvas = Rect::new(0, 0, h.width as i32, h.height as i32);
    let fmt = PixelFormat::new(ColorMode::Grayscale, depth, false);
    for k in 0..usize::from(h.channels) {
        let Some(p) = all.get(k * plane..(k + 1) * plane) else {
            warnings.push(format!("multichannel: channel {} is missing", k + 1));
            break;
        };
        let vals: Vec<f32> = (0..n).map(|i| 1.0 - decode(p, i, depth)).collect();
        let mut s = Surface::new(fmt);
        s.write_region(canvas, &vals);
        s.prune();
        let name = names.get(k).cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| format!("Channel {}", k + 1));
        // Without DisplayInfo the channels print in black, as Photoshop shows them.
        doc.channels.push(AlphaChannel { spot: Some((Color::BLACK, 0.0)), ..AlphaChannel::new(name, s) });
    }
    if let Some(r) = file.resource(crate::channel_map::DISPLAY_INFO) {
        crate::channel_map::apply_display_info(&r.data, true, &mut doc.channels);
    } else if let Some(r) = file.resource(crate::channel_map::DISPLAY_INFO_OLD) {
        crate::channel_map::apply_display_info(&r.data, false, &mut doc.channels);
    }
    let inks: Vec<(Vec<f32>, [f32; 3], f32)> = doc
        .channels
        .iter()
        .filter_map(|ch| {
            let (ink, solidity) = ch.spot?;
            Some((ch.surface.read_region(canvas), ink.to_rgb(), solidity))
        })
        .collect();
    let printed = crate::native::print_inks(&inks, n);
    let fmt = doc.pixel_format();
    let mut bg = Surface::new(fmt);
    let vals: Vec<f32> = printed.iter().flat_map(|p| photocraft_raster::from_rgba(&fmt, *p)).collect();
    bg.write_region(canvas, &vals);
    let mut layer = Layer::new("Background", LayerContent::Raster(bg));
    layer.locks.transparency = true;
    layer.locks.position = true;
    doc.layers.push(layer);
    warnings.push("Multichannel document converted to RGB: the inks are kept as spot channels".into());
}
