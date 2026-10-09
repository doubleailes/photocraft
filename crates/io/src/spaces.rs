//! Colour spaces of files (`docs/ocio-migration.md`, phase 3): an embedded ICC profile becomes
//! the document's named space (`photocraft_color::space`) on import, and a document's space
//! becomes the ICC profile embedded on export.

use std::sync::Arc;

use photocraft_cms::{ColorSpace, Profile};
use photocraft_color::{ColorMode, space};
use photocraft_doc::Document;

/// Tag `doc`, as read from a file, with the space of its embedded profile `icc`.
///
/// * A profile with a named space sets `color_space` (the pixels' encoding) and `source_space`.
/// * Float files hold linear values whatever profile they embed (Photoshop's 32-bit documents
///   carry the working profile): `color_space` is linear, `source_space` the profile's space.
/// * An integer file's profile with no named space is kept in `metadata.icc`, so the engine
///   converts the pixels from it when the document is opened.
/// * No profile, or one for another colour model: untagged (the working space).
pub(crate) fn tag_from_icc(doc: &mut Document, icc: Option<&[u8]>, warnings: &mut Vec<String>) {
    doc.metadata.icc = None;
    let parsed = icc.map(Profile::parse);
    let want = match doc.mode {
        ColorMode::Rgb => ColorSpace::Rgb,
        ColorMode::Grayscale => ColorSpace::Gray,
    };
    let profile = match parsed {
        Some(Ok(p)) if p.color_space == want => Some(p),
        Some(Ok(p)) => {
            warnings.push(format!("the embedded profile “{}” is for {:?} images, not {:?}: ignored", p.description, p.color_space, want));
            None
        }
        Some(Err(e)) => {
            warnings.push(format!("the embedded ICC profile can't be read ({e}): ignored"));
            None
        }
        None => None,
    };
    let name = profile.as_ref().and_then(space::name_for_profile);
    if let Some(n) = name {
        doc.source_space = n.to_string();
    }
    if doc.depth.is_float() {
        if profile.is_some() || doc.color_space.is_empty() && icc.is_some() {
            doc.color_space = space::LINEAR.to_string();
        }
        return;
    }
    match (name, profile, icc) {
        (Some(n), _, _) => doc.color_space = n.to_string(),
        (None, Some(p), Some(bytes)) => {
            doc.metadata.icc = Some(Arc::new(bytes.to_vec()));
            warnings.push(format!(
                "the embedded profile “{}” is not one of PhotoCraft's colour spaces: the image is converted from it when opened, and saves as sRGB",
                p.description
            ));
        }
        _ => {}
    }
}

/// The ICC profile to embed for `doc`'s pixels: its unnamed embedded profile, else its named
/// space's built-in profile; `None` when untagged.
pub(crate) fn embedded_icc(doc: &Document) -> Option<Vec<u8>> {
    if let Some(icc) = &doc.metadata.icc {
        return Some(icc.to_vec());
    }
    let p = space::profile(&doc.color_space, doc.mode)?;
    Some(p.to_bytes().to_vec())
}

/// The profile of `doc`'s pixels, when tagged (see [`embedded_icc`]).
pub(crate) fn pixel_profile(doc: &Document) -> Option<Profile> {
    if let Some(icc) = &doc.metadata.icc {
        return Profile::parse(icc).ok();
    }
    space::profile(&doc.color_space, doc.mode).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_cms::Builtin;
    use photocraft_color::SampleType;
    use photocraft_doc::Size;

    fn doc(mode: ColorMode, depth: SampleType) -> Document {
        Document::new("t", Size::new(1, 1), mode, depth)
    }

    #[test]
    fn named_unnamed_float_and_mismatched_profiles() {
        let mut w = Vec::new();
        let mut d = doc(ColorMode::Rgb, SampleType::U8);
        tag_from_icc(&mut d, Some(&Builtin::DisplayP3.profile().to_bytes()), &mut w);
        assert_eq!((d.color_space.as_str(), d.source_space.as_str()), ("sRGB Encoded P3-D65", "sRGB Encoded P3-D65"));
        assert!(d.metadata.icc.is_none() && w.is_empty());
        assert_eq!(embedded_icc(&d).map(|b| Profile::parse(&b).unwrap().content_hash()), Some(Builtin::DisplayP3.profile().content_hash()));
        // Gray: the space of its curve.
        let mut g = doc(ColorMode::Grayscale, SampleType::U16);
        tag_from_icc(&mut g, Some(&Builtin::SGray.profile().to_bytes()), &mut w);
        assert_eq!(g.color_space, space::SRGB);
        // Float: linear pixels, the profile's space as the source.
        let mut f = doc(ColorMode::Rgb, SampleType::F32);
        tag_from_icc(&mut f, Some(&Builtin::AdobeRgbCompat.profile().to_bytes()), &mut w);
        assert_eq!((f.color_space.as_str(), f.source_space.as_str()), (space::LINEAR, "Gamma 2.2 Encoded AdobeRGB"));
        // An RGB profile with no named space is kept for the engine to convert from.
        let mut odd = Builtin::DisplayP3.profile().clone();
        odd.trc = Some([photocraft_cms::Curve::Gamma(1.5), photocraft_cms::Curve::Gamma(1.5), photocraft_cms::Curve::Gamma(1.5)]);
        let odd = odd.with_encoded_bytes();
        let mut u = doc(ColorMode::Rgb, SampleType::U8);
        tag_from_icc(&mut u, Some(&odd.to_bytes()), &mut w);
        assert!(u.color_space.is_empty() && u.metadata.icc.is_some() && w.len() == 1, "{w:?}");
        assert!(pixel_profile(&u).unwrap().same_colors(&odd));
        // CMYK profile on an RGB image, garbage: ignored with a warning.
        let mut m = doc(ColorMode::Rgb, SampleType::U8);
        tag_from_icc(&mut m, Some(&Builtin::CoatedCmyk.profile().to_bytes()), &mut w);
        tag_from_icc(&mut m, Some(&[1, 2, 3]), &mut w);
        assert!(m.color_space.is_empty() && m.metadata.icc.is_none() && w.len() == 3);
        assert!(embedded_icc(&m).is_none());
    }
}
