use super::*;

fn session(w: u32, h: u32, depth: u64, mode: &str) -> Session {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": w, "height": h, "depth": depth, "mode": mode})).unwrap();
    paint(&mut s, |x, y| [x as f32 / w as f32, y as f32 / h as f32, 0.5, 1.0]);
    s
}

fn paint(s: &mut Session, f: impl Fn(i32, i32) -> [f32; 4]) {
    s.edit("setup", |doc, active| {
        let b = doc.bounds();
        let surf = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
        let fmt = surf.format();
        let mut data = Vec::new();
        for y in b.y0..b.y1 {
            for x in b.x0..b.x1 {
                data.extend(photocraft_raster::from_rgba(&fmt, f(x, y)));
            }
        }
        surf.write_region(b, &data);
        Ok(())
    })
    .unwrap();
}

fn doc(s: &Session) -> &photocraft_doc::Document {
    &s.active().unwrap().doc
}

#[test]
fn rotate_arbitrary_grows_canvas_and_keeps_background() {
    for depth in [8, 16, 32] {
        let mut s = session(40, 20, depth, "rgb");
        s.execute("layer.new.layer", json!({})).unwrap();
        paint(&mut s, |x, y| if (15..25).contains(&x) && (5..15).contains(&y) { [1.0, 0.0, 0.0, 1.0] } else { [0.0; 4] });
        s.execute("tools.setColors", json!({"background": "#00ff00"})).unwrap();
        let r = s.execute("image.rotation.arbitrary", json!({"angle": 90, "direction": "cw"})).unwrap();
        assert_eq!((r["width"].as_u64(), r["height"].as_u64()), (Some(20), Some(40)));
        let d = doc(&s);
        assert_eq!(d.layers[0].name, "Background");
        // The red square stays centred.
        assert!(d.layers[1].surface().unwrap().rgba(10, 20)[0] > 0.9, "{depth}");
        s.execute("edit.undo", json!({})).unwrap();
        s.execute("image.rotation.arbitrary", json!({"angle": 30, "direction": "ccw"})).unwrap();
        let d = doc(&s);
        assert_eq!(d.size, rotated_size(Size::new(40, 20), 30.0));
        assert!(d.size.width > 40 && d.size.height > 20);
        // Exposed corners take the background colour on the Background layer.
        let corner = d.layers[0].surface().unwrap().rgba(0, 0);
        assert!(corner[1] > 0.9 && corner[0] < 0.1, "{corner:?}");
    }
    let mut s = session(10, 10, 8, "rgb");
    assert!(s.execute("image.rotation.arbitrary", json!({"angle": 10, "direction": "up"})).is_err());
    let n = s.active().unwrap().history.past_len();
    s.execute("image.rotation.arbitrary", json!({"angle": 360})).unwrap();
    assert_eq!(s.active().unwrap().history.past_len(), n, "a full turn changes nothing");
}

#[test]
fn rotate_arbitrary_gray_and_rgb() {
    for mode in ["gray", "rgb"] {
        let mut s = session(16, 12, 16, mode);
        s.execute("image.rotation.arbitrary", json!({"angle": 45})).unwrap();
        assert_eq!(doc(&s).size, rotated_size(Size::new(16, 12), 45.0), "{mode}");
    }
}
