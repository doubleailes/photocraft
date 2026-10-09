//! Named colour spaces (`docs/ocio-migration.md`, phase 3): the OCIO names documents record in
//! `Document::color_space` / `source_space`, and the built-in ICC profile behind each one.
//!
//! Until ICC disappears (phase 8) the conversions still run through `photocraft-cms`, so every
//! name a document may carry is one of [`SPACES`], each backed by a built-in profile for RGB and,
//! where the space has one, for grayscale (a gray document in a space is the neutral axis of it:
//! the space's transfer curve). The names are the ACES CG config's (`ocio://cg-config-latest`)
//! where it has the space (Gamma 2.2 Encoded Rec.709 is gray-only: no built-in RGB profile has
//! that curve); ProPhoto and Rec.709-encoded Rec.2020 have no CG-config equivalent
//! and keep descriptive names.
//!
//! Embedded ICC profiles map to a name by colour ([`name_for_profile`]), not by bytes, so
//! Photoshop's sRGB IEC61966-2.1 is "sRGB Encoded Rec.709 (sRGB)". A profile that matches none of
//! them has no name: importers convert such files into sRGB (until ocio-rs #8 maps ICC profiles
//! to config spaces).

use photocraft_cms::{Builtin, ColorSpace, Profile};

use crate::ColorMode;

/// Pixels of every document in a session: linear, Rec.709 primaries (linear gray for gray
/// documents). Moving them to the config's `scene_linear` role (ACEScg) is phase 4.
pub const LINEAR: &str = "Linear Rec.709 (sRGB)";
/// The default encoded space (untagged 8- and 16-bit files, exports of documents made here).
pub const SRGB: &str = "sRGB Encoded Rec.709 (sRGB)";

/// One named space: its name, other names accepted for it, and its built-in profiles.
#[derive(Debug, Clone, Copy)]
pub struct NamedSpace {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub rgb: Option<Builtin>,
    pub gray: Option<Builtin>,
}

/// Every space a document can be in or come from.
pub const SPACES: &[NamedSpace] = &[
    NamedSpace { name: SRGB, aliases: &["srgb", "srgb_texture", "srgb_tx", "sRGB - Texture"], rgb: Some(Builtin::Srgb), gray: Some(Builtin::SGray) },
    NamedSpace { name: LINEAR, aliases: &["linear-srgb", "lin_rec709_srgb", "lin_rec709", "lin_srgb"], rgb: Some(Builtin::LinearSrgb), gray: Some(Builtin::LinearGray) },
    NamedSpace { name: "Gamma 2.2 Encoded Rec.709", aliases: &["g22_rec709", "gray-gamma-2.2"], rgb: None, gray: Some(Builtin::GrayGamma22) },
    NamedSpace { name: "sRGB Encoded P3-D65", aliases: &["display-p3", "srgb_p3d65", "srgb_displayp3"], rgb: Some(Builtin::DisplayP3), gray: None },
    NamedSpace { name: "Gamma 2.2 Encoded AdobeRGB", aliases: &["adobe-rgb-compat", "adobergb", "g22_adobergb"], rgb: Some(Builtin::AdobeRgbCompat), gray: None },
    NamedSpace { name: "Gamma 1.8 Encoded ProPhoto", aliases: &["prophoto-compat", "prophoto"], rgb: Some(Builtin::ProPhotoCompat), gray: None },
    NamedSpace { name: "Rec.709 Encoded Rec.2020", aliases: &["rec2020", "bt2020"], rgb: Some(Builtin::Rec2020), gray: None },
];

/// The named space `name` (its name, an alias or a built-in profile id, case-insensitive).
pub fn find(name: &str) -> Option<&'static NamedSpace> {
    let name = name.trim();
    SPACES.iter().find(|s| s.name.eq_ignore_ascii_case(name) || s.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))).or_else(|| {
        // Built-in profile ids and descriptions ("srgb", "sRGB IEC61966-2.1", "sgray", …).
        let b = Builtin::from_id(name)?;
        SPACES.iter().find(|s| s.rgb == Some(b) || s.gray == Some(b))
    })
}

/// The canonical name of `name` (see [`find`]).
pub fn canonical(name: &str) -> Option<&'static str> {
    find(name).map(|s| s.name)
}

/// The built-in profile of space `name` for a document in `mode`; `None` when the name is
/// unknown or the space has no form in that mode.
pub fn profile(name: &str, mode: ColorMode) -> Option<&'static Profile> {
    let s = find(name)?;
    match mode {
        ColorMode::Rgb => s.rgb.map(Builtin::profile),
        ColorMode::Grayscale => s.gray.map(Builtin::profile),
    }
}

/// The name of the space whose profile has the same colours as `p` (RGB or gray), if any.
pub fn name_for_profile(p: &Profile) -> Option<&'static str> {
    let hash = p.content_hash();
    let candidates = || {
        SPACES.iter().flat_map(|s| {
            let profile = match p.color_space {
                ColorSpace::Rgb => s.rgb,
                ColorSpace::Gray => s.gray,
                _ => None,
            };
            profile.map(|b| (s.name, b.profile()))
        })
    };
    // Exact bytes first (cheap, and the usual case for files written here), then by colour.
    candidates().find(|(_, b)| b.content_hash() == hash).or_else(|| candidates().find(|(_, b)| b.same_colors(p))).map(|(n, _)| n)
}

/// Is `name` a linear space (pixel values proportional to light)?
pub fn is_linear(name: &str) -> bool {
    canonical(name) == Some(LINEAR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_aliases_and_profiles() {
        assert_eq!(canonical("srgb"), Some(SRGB));
        assert_eq!(canonical("sRGB IEC61966-2.1"), Some(SRGB));
        assert_eq!(canonical("sgray"), Some(SRGB));
        assert_eq!(canonical("linear-gray"), Some(LINEAR));
        assert_eq!(canonical("Display P3"), Some("sRGB Encoded P3-D65"));
        assert_eq!(canonical(" lin_srgb "), Some(LINEAR));
        assert_eq!(canonical("nope"), None);
        assert_eq!(canonical("lab"), None, "Lab is not a document space");
        assert_eq!(profile(SRGB, ColorMode::Grayscale).map(|p| p.content_hash()), Some(Builtin::SGray.profile().content_hash()));
        assert_eq!(profile(LINEAR, ColorMode::Grayscale).map(|p| p.content_hash()), Some(Builtin::LinearGray.profile().content_hash()));
        assert!(profile("sRGB Encoded P3-D65", ColorMode::Grayscale).is_none());
        assert!(profile("Gamma 2.2 Encoded Rec.709", ColorMode::Rgb).is_none());
        assert!(is_linear("linear-srgb") && !is_linear(SRGB));
    }

    #[test]
    fn profiles_map_back_to_names() {
        for s in SPACES {
            if let Some(r) = s.rgb {
                assert_eq!(name_for_profile(r.profile()), Some(s.name));
            }
            if let Some(g) = s.gray {
                assert_eq!(name_for_profile(g.profile()), Some(s.name));
            }
        }
        assert_eq!(name_for_profile(Builtin::CoatedCmyk.profile()), None);
        assert_eq!(name_for_profile(Builtin::LabD50.profile()), None);
    }
}
