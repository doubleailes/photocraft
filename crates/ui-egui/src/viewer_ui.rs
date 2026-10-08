//! The viewer controls at the right of the status bar, like Nuke's viewer bar: OCIO on/off,
//! display, view, exposure and gamma. Every change runs the viewer commands
//! (`photocraft_engine::viewer`), so the bar, View › Viewer Options… and agents stay in step.

use egui::{RichText, Sense, vec2};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;
use crate::widgets;

/// Run a viewer command; a failure goes to the status line.
fn set(app: &mut PhotocraftApp, id: &str, params: Value) {
    if let Err(e) = app.run(id, params) {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

/// Draw the controls in a right-to-left layout (rightmost first).
pub fn status_controls(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let v = app.session.color.viewer.clone();
    // Gamma, then exposure (right to left).
    let mut g = v.gamma;
    if widgets::value_field(ui, &mut g, 0.1..=10.0, "", 44.0).on_hover_text(tl!("Viewer gamma (after the view)")).changed() {
        set(app, "view.gamma", json!({"gamma": g}));
    }
    ui.label(RichText::new("γ").color(t.text_dim).size(12.0));
    let mut e = v.exposure;
    if widgets::value_field(ui, &mut e, -20.0..=20.0, "", 44.0).on_hover_text(tl!("Viewer exposure in stops (before the view)")).changed() {
        set(app, "view.exposure", json!({"exposure": e}));
    }
    ui.label(RichText::new(tl!("EV")).color(t.text_dim).size(12.0));
    if v.ocio
        && let Ok(cfg) = app.session.color.ocio()
    {
        let r = app.session.color.viewer_report();
        let display = r["display"].as_str().unwrap_or("").to_string();
        let mut view = r["view"].as_str().unwrap_or("").to_string();
        let names = cfg.views(&display);
        let views: Vec<(String, &str)> = names.iter().map(|n| (n.clone(), n.as_str())).collect();
        if widgets::dropdown(ui, "viewer-view", &mut view, &views, 150.0) {
            set(app, "view.ocio.view", json!({"view": view}));
        }
        let mut d = display.clone();
        let displays = cfg.displays();
        let opts: Vec<(String, &str)> = displays.iter().map(|n| (n.clone(), n.as_str())).collect();
        if widgets::dropdown(ui, "viewer-display", &mut d, &opts, 130.0) {
            set(app, "view.ocio.display", json!({"display": d}));
        }
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 18.0), Sense::click());
    let fill = if v.ocio { t.accent } else if resp.hovered() { t.hover } else { t.field };
    ui.painter().rect_filled(rect, t.radius_sm, fill);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "OCIO", egui::FontId::proportional(10.5), if v.ocio { t.primary_text } else { t.text_dim });
    let tip = if v.ocio { tl!("OCIO viewer on: click for the ICC monitor display") } else { tl!("Show documents through the OCIO display and view") };
    if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        set(app, "view.viewerOptions", json!({"ocio": !v.ocio}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_controls_draw_and_drive_the_viewer() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        for ocio in [false, true] {
            app.session.color.viewer.ocio = ocio;
            crate::analysis_ui::tests::render(&mut app, &ctx, |app, ctx| {
                egui::Window::new("viewer").show(ctx, |ui| ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| status_controls(app, ui)));
            });
        }
        set(&mut app, "view.ocio.view", json!({"view": "nope"}));
        assert!(app.ui.status_error && app.ui.status.contains("nope"));
        set(&mut app, "view.exposure", json!({"exposure": 1.0}));
        assert_eq!(app.session.color.viewer.exposure, 1.0);
    }
}
