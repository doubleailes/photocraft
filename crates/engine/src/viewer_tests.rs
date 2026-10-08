use super::*;
use photocraft_compose::Buffer;
use photocraft_geom::Rect;

fn session() -> Session {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8, "mode": "rgb", "depth": 16})).unwrap();
    s
}

/// One straight-alpha pixel of linear `v` through the CPU canvas.
fn shown(s: &Session, v: [f32; 3]) -> [u8; 4] {
    let doc = s.active().unwrap().doc.clone();
    let d = s.color.canvas_display(&doc).unwrap();
    let b = Buffer::filled(Rect::new(0, 0, 1, 1), [v[0], v[1], v[2], 1.0]);
    let img = d.to_rgba8(&b);
    [img.pixels[0], img.pixels[1], img.pixels[2], img.pixels[3]]
}

#[test]
fn defaults_leave_the_icc_display_alone() {
    let s = session();
    assert_eq!(s.color.viewer, ViewerSettings::default());
    assert!(s.color.viewer_lut().unwrap().is_none());
    let doc = s.active().unwrap().doc.clone();
    let d = s.color.canvas_display(&doc).unwrap();
    assert!(d.viewer.is_none() && d.is_neutral());
    // Linear 0.18 is sRGB 118.
    assert_eq!(shown(&s, [0.18; 3])[0], 118);
    let r = s.color.viewer_report();
    assert_eq!(r["ocio"], false);
    assert_eq!(r["display"], "sRGB - Display");
    assert_eq!(r["config"]["origin"], "default");
    assert_eq!(r["source"], "Linear Rec.709 (sRGB)");
    assert!(r["displays"].as_array().unwrap().len() > 3);
}

#[test]
fn ocio_display_view_and_look() {
    let mut s = session();
    let r = s.execute("view.ocio.display", json!({"display": "sRGB - Display"})).unwrap();
    assert_eq!((r["ocio"].as_bool(), r["view"].as_str()), (Some(true), Some("ACES 2.0 - SDR 100 nits (Rec.709)")));
    let doc = s.active().unwrap().doc.clone();
    let tonemapped = s.color.canvas_display(&doc).unwrap();
    assert!(tonemapped.viewer.is_some() && tonemapped.transform.is_none());
    // The ACES view tone-maps: linear 1.0 is well below white, 16.0 still below 255.
    let one = shown(&s, [1.0; 3]);
    assert!(one[0] > 150 && one[0] < 250, "{one:?}");
    assert!(shown(&s, [16.0; 3])[0] > one[0]);
    // Un-tone-mapped is the plain sRGB encoding (within the baked LUT's error).
    s.execute("view.ocio.view", json!({"view": "Un-tone-mapped"})).unwrap();
    let grey = shown(&s, [0.18; 3]);
    assert!((i32::from(grey[0]) - 118).abs() <= 2, "{grey:?}");
    assert_ne!(s.color.canvas_display(&doc).unwrap().lut_key, tonemapped.lut_key);
    // The view survives a display that has it, and falls back to the default otherwise.
    s.execute("view.ocio.display", json!({"display": "Display P3 - Display"})).unwrap();
    assert_eq!(s.color.viewer.view, "Un-tone-mapped");
    let look = s.color.ocio().unwrap().looks()[0].clone();
    assert_eq!(s.execute("view.ocio.look", json!({"look": look})).unwrap()["look"], look.as_str());
    assert_eq!(s.execute("view.ocio.look", json!({"look": "none"})).unwrap()["look"], "");
    // Off: back to the ICC display.
    s.execute("view.ocio.display", json!({"display": "none"})).unwrap();
    assert!(s.color.canvas_display(&doc).unwrap().viewer.is_none());
    assert_eq!(s.color.viewer.display, "Display P3 - Display", "remembered for next time");
}

#[test]
fn exposure_and_gamma_apply_with_and_without_ocio() {
    let mut s = session();
    let doc = s.active().unwrap().doc.clone();
    let sig = s.color.display_signature(&doc);
    s.execute("view.exposure", json!({"exposure": 1.0})).unwrap();
    // +1 stop: linear 0.09 shows as 0.18 (sRGB 118).
    assert_eq!(shown(&s, [0.09; 3])[0], 118);
    assert_eq!(s.color.display_signature(&doc), sig, "the GPU LUT doesn't depend on exposure");
    s.execute("view.exposure", json!({"exposure": 0.0})).unwrap();
    s.execute("view.gamma", json!({"gamma": 2.0})).unwrap();
    // Gamma on display values: sRGB 0.5 → 0.5^(1/2).
    let half = photocraft_color::convert::srgb_to_linear(0.5);
    let g = shown(&s, [half; 3])[0];
    assert!((i32::from(g) - 180).abs() <= 1, "{g}");
    s.execute("view.viewerOptions", json!({"ocio": true, "view": "Un-tone-mapped", "gamma": 1.0, "exposure": -1.0})).unwrap();
    let e = shown(&s, [0.36; 3])[0];
    assert!((i32::from(e) - 118).abs() <= 2, "{e}");
    assert_eq!(s.color.viewer.grade(), Some([-1.0, 1.0]));
    // Clamped ranges.
    let r = s.execute("view.viewerOptions", json!({"exposure": 99, "gamma": 0})).unwrap();
    assert_eq!((r["exposure"].as_f64(), r["gamma"].as_f64().map(|g| (g * 10.0).round())), (Some(20.0), Some(1.0)));
}

#[test]
fn bad_params_are_errors_and_change_nothing() {
    let mut s = session();
    let before = s.color.viewer.clone();
    for (id, p) in [
        ("view.ocio.display", json!({"display": "nope"})),
        ("view.ocio.display", json!({"display": 3})),
        ("view.ocio.view", json!({"view": "nope"})),
        ("view.ocio.look", json!({"look": "nope"})),
        ("view.exposure", json!({"exposure": "bright"})),
        ("view.gamma", json!({"gamma": [1]})),
        ("view.viewerOptions", json!({"ocio": "yes"})),
        ("view.viewerOptions", json!({"exposure": 2.0, "view": "nope"})),
        ("view.viewerOptions", json!([1, 2])),
    ] {
        assert!(s.execute(id, p.clone()).is_err(), "{id} {p}");
    }
    assert_eq!(s.color.viewer, before);
    // Queries need no document.
    let mut empty = Session::new();
    for id in ["view.viewerOptions", "view.ocio.display", "view.ocio.view", "view.ocio.look", "view.exposure", "view.gamma"] {
        assert!(empty.execute(id, json!({})).is_ok(), "{id}");
    }
}

#[test]
fn a_missing_config_is_an_error_not_a_crash() {
    let mut s = session();
    s.color.settings.ocio_config = "/no/such/config.ocio".into();
    assert!(s.execute("view.ocio.display", json!({"display": "sRGB - Display"})).is_err());
    assert!(s.color.viewer_report()["config"]["error"].is_string());
    // A viewer left on with a config that went away falls back to the ICC display.
    s.color.viewer.ocio = true;
    let doc = s.active().unwrap().doc.clone();
    assert!(s.color.canvas_display(&doc).unwrap().viewer.is_none());
    assert!(crate::color_cmds::validate_settings(&s.color.settings).is_err());
    s.color.settings.ocio_config = photocraft_ocio::DEFAULT_CONFIG.into();
    assert!(crate::color_cmds::validate_settings(&s.color.settings).is_ok());
    assert!(s.color.canvas_display(&doc).unwrap().viewer.is_some());
}
