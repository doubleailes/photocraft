//! Files in colour models PhotoCraft documents don't use open converted: Indexed and
//! Multichannel as RGB, Bitmap and Duotone as grayscale (`photocraft_io::native`).

use photocraft_color::ColorMode;
use photocraft_psd::testgen;
use photocraft_psd::{ColorMode as PsdMode, Compression, Version};

#[test]
fn flattened_modes_open_as_rgb_or_grayscale() {
    for (mode, depth, want) in [
        (PsdMode::Indexed, 8, ColorMode::Rgb),
        (PsdMode::Bitmap, 1, ColorMode::Grayscale),
        (PsdMode::Duotone, 8, ColorMode::Grayscale),
        (PsdMode::Multichannel, 8, ColorMode::Rgb),
        (PsdMode::Multichannel, 16, ColorMode::Rgb),
    ] {
        let f = testgen::merged_only(Version::Psd, mode, depth, Compression::Rle, 7, 5);
        let r = photocraft_io::import("m.psd", &f.to_bytes().unwrap()).unwrap();
        let d = &r.document;
        let what = format!("{mode:?} {depth}");
        assert_eq!(d.mode, want, "{what}");
        assert_eq!((d.size.width, d.size.height), (7, 5), "{what}");
        assert_eq!(d.layers.len(), 1, "{what}: the image is one Background layer");
        let s = d.layers[0].surface().unwrap();
        assert_eq!(s.format().mode, want, "{what}");
        assert!(s.read_region(d.bounds()).iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)), "{what}");
        if mode == PsdMode::Multichannel {
            // The inks stay as spot channels; the image is their print over paper.
            assert_eq!(d.channels.len(), 2, "{what}");
            assert!(d.channels.iter().all(|c| c.spot.is_some()), "{what}");
            assert!(r.warnings.iter().any(|w| w.contains("Multichannel")), "{what}: {:?}", r.warnings);
        }
    }
}
