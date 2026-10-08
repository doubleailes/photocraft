use super::*;

const LIN709: &str = "Linear Rec.709 (sRGB)";

fn cg() -> Ocio {
    Ocio::open(DEFAULT_CONFIG, ConfigOrigin::Default).unwrap()
}

fn srgb_encode(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

#[test]
fn default_config_lists_displays_views_and_looks() {
    let o = cg();
    assert_eq!(o.origin, ConfigOrigin::Default);
    assert_eq!(o.default_display(), "sRGB - Display");
    assert!(o.displays().len() > 3);
    let views = o.views("sRGB - Display");
    assert!(views.iter().any(|v| v == "Un-tone-mapped"), "{views:?}");
    assert_eq!(o.default_view("sRGB - Display"), views[0]);
    assert!(o.views("nope").is_empty());
    assert!(!o.looks().is_empty());
    assert_eq!(o.role("scene_linear").as_deref(), Some("ACEScg"));
    assert_eq!(o.role("nope"), None);
    assert_eq!(o.find_space(&["nope", LIN709]).as_deref(), Some(LIN709));
    assert_eq!(o.find_space(&["nope"]).as_deref(), Some("ACEScg"));
}

#[test]
fn shaper_round_trips_and_pins_the_ends() {
    let s = Shaper::DEFAULT;
    assert_eq!(s.encode(0.0), 0.0);
    assert!((s.encode(4096.0) - 1.0).abs() < 1e-6);
    assert_eq!(s.encode(-3.0), 0.0);
    assert_eq!(s.encode(f32::NAN), 0.0);
    assert_eq!(s.encode(f32::INFINITY), s.encode(1e9));
    for x in [1e-4f32, 0.01, 0.18, 1.0, 7.5, 900.0] {
        assert!((s.decode(s.encode(x)) - x).abs() <= x * 1e-4, "{x}");
    }
}

#[test]
fn untonemapped_lut_matches_the_srgb_curve() {
    // Linear Rec.709 through sRGB - Display / Un-tone-mapped is the sRGB encoding: the baked
    // LUT (shaper + trilinear) stays within a couple of 8-bit codes of it.
    let lut = cg().bake_viewer(LIN709, "sRGB - Display", "Un-tone-mapped", "", 64).unwrap();
    let mut worst = 0.0f32;
    for i in 0..=200 {
        let x = i as f32 / 200.0;
        for rgb in [[x; 3], [x, 0.5 * x, 0.1], [0.02, x, 1.0 - x]] {
            let got = lut.apply(rgb);
            for c in 0..3 {
                worst = worst.max((got[c] - srgb_encode(rgb[c])).abs());
            }
        }
    }
    assert!(worst < 2.0 / 255.0, "worst {worst}");
    assert_eq!(lut.apply([0.0; 3]), [0.0; 3]);
}

#[test]
fn aces_view_tone_maps_highlights() {
    let o = cg();
    let d = o.default_display();
    let lut = o.bake_viewer(LIN709, &d, &o.default_view(&d), "", 33).unwrap();
    let grey = lut.apply([0.18; 3]);
    let hot = lut.apply([64.0; 3]);
    assert!(grey[0] > 0.2 && grey[0] < 0.5, "{grey:?}");
    assert!(hot[0] > grey[0] && hot[0] <= 1.01, "highlights roll off: {hot:?}");
    // A look changes the result (or at least bakes).
    let look = o.looks()[0].clone();
    assert!(o.bake_viewer(LIN709, &d, &o.default_view(&d), &look, 9).is_ok());
}

#[test]
fn bad_names_and_sizes_are_errors() {
    let o = cg();
    for (src, d, v, l, n) in [
        ("nope", "sRGB - Display", "Raw", "", 9),
        (LIN709, "nope", "Raw", "", 9),
        (LIN709, "sRGB - Display", "nope", "", 9),
        (LIN709, "sRGB - Display", "Raw", "nope", 9),
        (LIN709, "sRGB - Display", "Raw", "", 1),
        (LIN709, "sRGB - Display", "Raw", "", MAX_LUT_SIZE + 1),
    ] {
        assert!(o.bake_viewer(src, d, v, l, n).is_err(), "{src} {d} {v} {l} {n}");
    }
}

#[test]
fn missing_and_garbage_configs_are_errors() {
    assert!(Ocio::open("/no/such/config.ocio", ConfigOrigin::Settings).is_err());
    assert!(Ocio::open("ocio://no-such-builtin", ConfigOrigin::Settings).is_err());
    let dir = std::env::temp_dir().join(format!("pc-ocio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("garbage.ocio");
    std::fs::write(&p, "ocio_profile_version: [[[\n\0\u{7f}").unwrap();
    assert!(Ocio::open(p.to_str().unwrap(), ConfigOrigin::Settings).is_err());
    let _ = std::fs::remove_dir_all(dir);
    // Settings win over everything else.
    let o = Ocio::load(" ocio://cg-config-latest ").unwrap();
    assert_eq!((o.origin, o.source.as_str()), (ConfigOrigin::Settings, DEFAULT_CONFIG));
}

#[test]
fn guard_turns_panics_into_errors() {
    #[allow(clippy::panic)]
    let r: Result<()> = guard("test", || panic!("boom"));
    assert!(r.unwrap_err().0.contains("boom"));
}

#[test]
fn lut_bytes_and_degenerate_luts() {
    let lut = ViewerLut { size: 2, shaper: Shaper::DEFAULT, data: vec![[0.5, 0.25, 1.0]; 8] };
    let b = lut.to_rgba16f_bytes();
    assert_eq!(b.len(), 8 * 8);
    assert_eq!(&b[..8], &[0x00, 0x38, 0x00, 0x34, 0x00, 0x3c, 0x00, 0x3c]);
    assert_eq!(lut.apply([3.0, -1.0, f32::NAN]), [0.5, 0.25, 1.0]);
    let short = ViewerLut { size: 4, shaper: Shaper::DEFAULT, data: vec![] };
    assert_eq!(short.apply([0.3; 3]), [0.3; 3]);
}
