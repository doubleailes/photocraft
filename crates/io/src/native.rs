//! Files in colour models PhotoCraft documents don't use are converted as they are read
//! (`docs/ocio-migration.md`): CMYK through the file's CMYK profile (else the built-in coated
//! CMYK) and Lab through the D50 formulas, both to sRGB at the file's depth. Indexed becomes RGB,
//! Bitmap and Duotone grayscale (their composite), Multichannel the RGB rendering of its inks.
//! The engine then opens the result like any other integer file (linear half float).

use std::sync::Arc;

use photocraft_cms::{Builtin, ColorSpace, Intent, Profile, Transform};
use photocraft_color::{ColorMode, SampleType, read_sample};
use photocraft_psd::ColorMode as PsdMode;

/// How samples of a file's colour model become RGB.
pub(crate) enum Native {
    /// Ink amounts (1 = solid) C, M, Y, K through this transform to sRGB.
    Cmyk(Arc<Transform>),
    /// L, a, b stored 0..=1 (L / 100, (a + 128) / 255, (b + 128) / 255).
    Lab,
}

impl Native {
    /// The converter for a layered file in `mode` (`None`: RGB and grayscale need none).
    /// `icc` is the file's embedded profile.
    pub(crate) fn for_psd(mode: PsdMode, icc: Option<&[u8]>) -> Option<Native> {
        match mode {
            PsdMode::Cmyk => Some(Native::cmyk(icc)),
            PsdMode::Lab => Some(Native::Lab),
            _ => None,
        }
    }

    /// CMYK through the embedded profile when it is a CMYK one, else the built-in coated CMYK.
    pub(crate) fn cmyk(icc: Option<&[u8]>) -> Native {
        let src = icc.and_then(|b| Profile::parse(b).ok()).filter(|p| p.color_space == ColorSpace::Cmyk);
        let src = src.as_ref().unwrap_or_else(|| Builtin::CoatedCmyk.profile());
        match Transform::new(src, Builtin::Srgb.profile(), Intent::RelativeColorimetric, true)
            .or_else(|_| Transform::new(Builtin::CoatedCmyk.profile(), Builtin::Srgb.profile(), Intent::RelativeColorimetric, true))
        {
            Ok(t) => Native::Cmyk(Arc::new(t)),
            // The built-in profiles always build a transform; Lab formulas are the last resort.
            Err(_) => Native::Lab,
        }
    }

    /// Colour channels of the file's model.
    pub(crate) fn channels(&self) -> usize {
        match self {
            Native::Cmyk(_) => 4,
            Native::Lab => 3,
        }
    }

    /// Native-endian interleaved samples (`channels()` colour channels plus alpha when `alpha`)
    /// as interleaved RGB(A) floats.
    pub(crate) fn to_rgb(&self, bytes: &[u8], sample: SampleType, alpha: bool) -> Vec<f32> {
        let cc = self.channels();
        let stride = cc + usize::from(alpha);
        let n = bytes.len() / sample.bytes() / stride.max(1);
        let vals: Vec<f32> = (0..n * stride).map(|i| read_sample(bytes, sample, i)).collect();
        let out_stride = 3 + usize::from(alpha);
        let mut out = vec![0.0f32; n * out_stride];
        match self {
            Native::Cmyk(t) => t.convert_f32(&vals, stride, &mut out, out_stride, true),
            Native::Lab => {
                for (s, d) in vals.chunks_exact(stride).zip(out.chunks_exact_mut(out_stride)) {
                    let rgb = photocraft_color::convert::lab_to_srgb([s[0] * 100.0, s[1] * 255.0 - 128.0, s[2] * 255.0 - 128.0]);
                    d[..3].copy_from_slice(&rgb);
                    if alpha {
                        d[3] = s[3];
                    }
                }
            }
        }
        out
    }
}

/// The document model a file in `mode` opens as.
pub(crate) fn document_mode(mode: PsdMode) -> Option<ColorMode> {
    Some(match mode {
        PsdMode::Grayscale | PsdMode::Bitmap | PsdMode::Duotone => ColorMode::Grayscale,
        PsdMode::Rgb | PsdMode::Indexed | PsdMode::Cmyk | PsdMode::Lab | PsdMode::Multichannel => ColorMode::Rgb,
        PsdMode::Unknown(_) => return None,
    })
}

/// Ink channels (`v`, 1 = solid; display colour `c`; solidity `s`) printed over white paper:
/// each turns the colour underneath `p` into `(1 − s·v)·p·(1 − v + v·c) + s·v·c`, transparent inks
/// multiplying like overprinted process inks, opaque ones covering what is below (how Photoshop
/// previews spot channels). `inks` holds one plane of `n` values per ink.
pub(crate) fn print_inks(inks: &[(Vec<f32>, [f32; 3], f32)], n: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[1.0f32, 1.0, 1.0, 1.0]; n];
    for (plane, c, solidity) in inks {
        let s = solidity.clamp(0.0, 1.0);
        for (p, v) in out.iter_mut().zip(plane) {
            let v = v.clamp(0.0, 1.0);
            if v <= 0.0 {
                continue;
            }
            let keep = 1.0 - s * v;
            for j in 0..3 {
                p[j] = keep * p[j] * (1.0 - v + v * c[j]) + s * v * c[j];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_u8(v: &[u8]) -> Vec<u8> {
        v.to_vec()
    }

    #[test]
    fn cmyk_white_and_black_become_srgb() {
        let n = Native::cmyk(None);
        let rgb = n.to_rgb(&bytes_u8(&[0, 0, 0, 0, 255, 255, 255, 255, 255, 255]), SampleType::U8, true);
        assert!(rgb[..3].iter().all(|v| (*v - 1.0).abs() < 0.01), "no ink is paper white: {rgb:?}");
        assert_eq!(rgb[3], 1.0, "alpha kept");
        assert!(rgb[4..7].iter().all(|v| *v < 0.1), "full ink is near black: {rgb:?}");
    }

    #[test]
    fn lab_neutrals_and_red() {
        // L 50, a 0, b 0 → mid grey; L 54, a 81, b 70 → sRGB red.
        let lab = |l: f32, a: f32, b: f32| [(l / 100.0 * 255.0).round() as u8, (a + 128.0).round() as u8, (b + 128.0).round() as u8];
        let mut bytes = lab(50.0, 0.0, 0.0).to_vec();
        bytes.extend(lab(54.3, 80.8, 69.9));
        let rgb = Native::Lab.to_rgb(&bytes, SampleType::U8, false);
        assert!((rgb[0] - rgb[1]).abs() < 0.01 && (rgb[0] - 0.466).abs() < 0.02, "{rgb:?}");
        assert!(rgb[3] > 0.95 && rgb[4] < 0.1 && rgb[5] < 0.1, "{rgb:?}");
    }

    #[test]
    fn inks_multiply_over_paper() {
        let out = print_inks(&[(vec![0.0, 1.0], [1.0, 0.0, 0.0], 0.0), (vec![0.0, 1.0], [0.0, 0.0, 1.0], 0.0)], 2);
        assert_eq!(out[0], [1.0; 4]);
        assert!(out[1][0] < 0.01 && out[1][2] < 0.01, "red over blue overprints to black: {:?}", out[1]);
        assert_eq!(document_mode(PsdMode::Duotone), Some(ColorMode::Grayscale));
        assert_eq!(document_mode(PsdMode::Cmyk), Some(ColorMode::Rgb));
    }
}
