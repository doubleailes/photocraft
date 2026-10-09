//! The OCIO viewer (`docs/ocio-migration.md`, phase 5): how the canvas shows scene-linear
//! documents, like Nuke's viewer.
//!
//! * Session-wide [`ViewerSettings`] (`ColorState::viewer`): the OCIO display, view and look,
//!   plus exposure (stops, scene-linear gain before the view) and gamma (on display values,
//!   after the view, like OCIO's display CC). Exposure and gamma apply with or without OCIO;
//!   they replace Photoshop's per-document 32-bit Preview Options.
//! * With `ocio` on, the canvas texture values (the linear composite) go through
//!   [`photocraft_ocio::ViewerLut`] (`Linear Rec.709 (sRGB)` → look → display/view, baked
//!   behind a log shaper) instead of the ICC monitor transform; Proof Colors and Gamut Warning
//!   don't apply then. Off (the default), the ICC display path is unchanged.
//! * The config comes from Color Settings › OCIO config (`ocioConfig`), else `$OCIO`, else
//!   `ocio://cg-config-latest`. Configs and baked LUTs are cached here.
//!
//! Commands: `view.viewerOptions` (all at once; the UI's dialog), `view.ocio.display`,
//! `view.ocio.view`, `view.ocio.look`, `view.exposure`, `view.gamma`. Each answers with the
//! viewer state and the config's choices; `{}` changes nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use photocraft_ocio::Ocio;
pub use photocraft_ocio::{Shaper, ViewerLut};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::color_cmds::ColorState;
use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// LUT edge of the baked viewer (64³ `Rgba16Float` is 2 MB).
pub const VIEWER_LUT: usize = 64;

/// What document pixels are in, by name, until documents carry an OCIO colour space (phase 3):
/// linear sRGB primaries. The first name the config has wins, else its `scene_linear` role.
pub const DOC_SPACES: &[&str] = &["Linear Rec.709 (sRGB)", "lin_rec709_srgb", "lin_rec709", "Utility - Linear - sRGB", "lin_srgb"];

pub const EXPOSURE_RANGE: (f32, f32) = (-20.0, 20.0);
pub const GAMMA_RANGE: (f32, f32) = (0.1, 10.0);

/// View state of the canvas (session-wide, not saved).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ViewerSettings {
    /// Show documents through the OCIO display/view (else the ICC monitor transform).
    pub ocio: bool,
    /// OCIO display; empty = the config's default.
    pub display: String,
    /// View of `display`; empty = its default.
    pub view: String,
    /// Look applied before the view; empty = none.
    pub look: String,
    /// Stops of scene-linear gain before the view.
    pub exposure: f32,
    /// Display gamma after the view (1 = none).
    pub gamma: f32,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self { ocio: false, display: String::new(), view: String::new(), look: String::new(), exposure: 0.0, gamma: 1.0 }
    }
}

impl ViewerSettings {
    /// Exposure and gamma leave values alone.
    pub fn is_neutral(&self) -> bool {
        self.exposure == 0.0 && self.gamma == 1.0
    }

    /// `[exposure, gamma]` for the canvas shader, `None` when neutral.
    pub fn grade(&self) -> Option<[f32; 2]> {
        (!self.is_neutral()).then_some([self.exposure, self.gamma])
    }

    /// Linear gain of the exposure.
    pub fn gain(&self) -> f32 {
        self.exposure.clamp(EXPOSURE_RANGE.0, EXPOSURE_RANGE.1).exp2()
    }

    /// 1 / gamma.
    pub fn inv_gamma(&self) -> f32 {
        1.0 / self.gamma.clamp(GAMMA_RANGE.0, GAMMA_RANGE.1)
    }

    /// What the preferences keep: OCIO on/off, display, view and look. Exposure and gamma are
    /// per sitting (like Nuke's viewer) and start neutral.
    pub fn saved(&self) -> Value {
        json!({"ocio": self.ocio, "display": self.display, "view": self.view, "look": self.look})
    }

    /// Restore [`ViewerSettings::saved`]. Unknown or ill-typed keys are ignored; names aren't
    /// checked against the config here (a viewer the config can't show falls back to the ICC
    /// display, and the viewer commands say why).
    pub fn load_saved(&mut self, v: &Value) {
        if let Some(b) = v.get("ocio").and_then(Value::as_bool) {
            self.ocio = b;
        }
        for (k, slot) in [("display", &mut self.display), ("view", &mut self.view), ("look", &mut self.look)] {
            if let Some(s) = v.get(k).and_then(Value::as_str) {
                *slot = s.chars().take(256).collect();
            }
        }
    }
}

/// The viewer's display, view and look as resolved against the config.
#[derive(Clone, Debug)]
pub struct ResolvedViewer {
    pub config: Arc<Ocio>,
    /// Colour space of the canvas values (see [`DOC_SPACES`]).
    pub source: String,
    pub display: String,
    pub view: String,
    pub look: String,
}

type LutKey = (String, String, String, String, String);
/// (settings spec, `$OCIO`) a config was loaded for, and the outcome.
type LoadedConfig = ((String, String), std::result::Result<Arc<Ocio>, String>);

#[derive(Default)]
struct CacheInner {
    config: Option<LoadedConfig>,
    luts: HashMap<LutKey, (Arc<ViewerLut>, u64)>,
}

/// Loaded configs and baked viewer LUTs (`ColorState::ocio`).
#[derive(Default)]
pub struct OcioCache {
    inner: Mutex<CacheInner>,
}

fn hash_of(v: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn ocio_err(e: photocraft_ocio::OcioError) -> EngineError {
    EngineError::Other(e.to_string())
}

impl ColorState {
    /// The OCIO config: Color Settings › OCIO config, else `$OCIO`, else the built-in ACES CG
    /// config. Loaded once per (setting, `$OCIO`); a failure is remembered too.
    pub fn ocio(&self) -> Result<Arc<Ocio>> {
        let key = (self.settings.ocio_config.trim().to_string(), std::env::var("OCIO").unwrap_or_default());
        let mut c = self.ocio.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, r)) = &c.config
            && *k == key
        {
            return r.clone().map_err(EngineError::Other);
        }
        let r = Ocio::load(&key.0).map(Arc::new).map_err(|e| e.to_string());
        c.luts.clear();
        c.config = Some((key, r.clone()));
        r.map_err(EngineError::Other)
    }

    /// The viewer resolved against the config: empty names become the config's defaults, and
    /// names the config doesn't have are errors. `None` when the OCIO viewer is off.
    pub fn viewer_resolved(&self) -> Result<Option<ResolvedViewer>> {
        if !self.viewer.ocio {
            return Ok(None);
        }
        resolve(&self.ocio()?, &self.viewer).map(Some)
    }

    /// The baked viewer LUT and a key that changes with it; `None` when the OCIO viewer is off.
    pub fn viewer_lut(&self) -> Result<Option<(Arc<ViewerLut>, u64)>> {
        let Some(r) = self.viewer_resolved()? else { return Ok(None) };
        let key: LutKey = (r.config.source.clone(), r.source.clone(), r.display.clone(), r.view.clone(), r.look.clone());
        if let Some(hit) = self.ocio.inner.lock().unwrap_or_else(|e| e.into_inner()).luts.get(&key) {
            return Ok(Some(hit.clone()));
        }
        let lut = Arc::new(r.config.bake_viewer(&r.source, &r.display, &r.view, &r.look, VIEWER_LUT).map_err(ocio_err)?);
        let entry = (lut, hash_of(&key));
        let mut c = self.ocio.inner.lock().unwrap_or_else(|e| e.into_inner());
        if c.luts.len() > 16 {
            c.luts.clear();
        }
        c.luts.insert(key, entry.clone());
        Ok(Some(entry))
    }

    /// The viewer state and the config's choices (the commands' answer).
    pub fn viewer_report(&self) -> Value {
        let v = &self.viewer;
        let mut r = json!({"ocio": v.ocio, "display": v.display, "view": v.view, "look": v.look, "exposure": v.exposure, "gamma": v.gamma});
        match self.ocio() {
            Ok(cfg) => {
                let display = resolve_display(&cfg, &v.display);
                let view = if v.view.is_empty() { cfg.default_view(&display) } else { v.view.clone() };
                r["config"] = json!({"source": cfg.source, "origin": cfg.origin.id(), "name": cfg.name()});
                r["display"] = json!(display);
                r["view"] = json!(view);
                r["displays"] = json!(cfg.displays());
                r["views"] = json!(cfg.views(&display));
                r["looks"] = json!(cfg.looks());
                r["source"] = json!(cfg.find_space(DOC_SPACES));
                r["roles"] = Value::Object(photocraft_ocio::Role::ALL.iter().map(|ro| (ro.name().to_string(), json!(cfg.role_space(*ro)))).collect());
            }
            Err(e) => r["config"] = json!({"error": e.to_string()}),
        }
        r
    }
}

fn resolve_display(cfg: &Ocio, display: &str) -> String {
    if display.is_empty() { cfg.default_display() } else { display.to_string() }
}

fn resolve(cfg: &Arc<Ocio>, v: &ViewerSettings) -> Result<ResolvedViewer> {
    let display = resolve_display(cfg, &v.display);
    if !cfg.displays().contains(&display) {
        return Err(EngineError::Other(format!("OCIO: no display `{display}` in {}", cfg.source)));
    }
    let view = if v.view.is_empty() { cfg.default_view(&display) } else { v.view.clone() };
    if !cfg.views(&display).contains(&view) {
        return Err(EngineError::Other(format!("OCIO: display `{display}` has no view `{view}`")));
    }
    if !v.look.is_empty() && !cfg.looks().contains(&v.look) {
        return Err(EngineError::Other(format!("OCIO: no look `{}` in {}", v.look, cfg.source)));
    }
    let source = cfg.find_space(DOC_SPACES).ok_or_else(|| EngineError::Other(format!("OCIO: {} has no linear sRGB space and no scene_linear role", cfg.source)))?;
    Ok(ResolvedViewer { config: cfg.clone(), source, display, view, look: v.look.clone() })
}

// ------------------------------------------------------------------ commands

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn str_param<'a>(cmd: &str, p: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(o) => Err(bad(cmd, format!("`{key}` must be a string, got {o}"))),
    }
}

fn num_param(cmd: &str, p: &Value, key: &str, (lo, hi): (f32, f32)) -> Result<Option<f32>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(x) if x.is_finite() => Ok(Some((x as f32).clamp(lo, hi))),
            _ => Err(bad(cmd, format!("`{key}` must be a number ({lo}..{hi}), got {v}"))),
        },
    }
}

/// Apply `p` to a copy of the viewer, check it against the config, then commit it.
fn apply(s: &mut Session, cmd: &str, p: &Value) -> Result<Value> {
    if !p.is_object() && !p.is_null() {
        return Err(bad(cmd, "params must be an object"));
    }
    let mut v = s.color.viewer.clone();
    match p.get("ocio") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => v.ocio = *b,
        Some(o) => return Err(bad(cmd, format!("`ocio` must be true or false, got {o}"))),
    }
    if let Some(d) = str_param(cmd, p, "display")? {
        if d == "none" {
            v.ocio = false;
        } else {
            let cfg = s.color.ocio()?;
            let d = resolve_display(&cfg, d);
            if !cfg.displays().contains(&d) {
                return Err(bad(cmd, format!("no display `{d}` (one of: {})", cfg.displays().join(", "))));
            }
            // Keep the view when the new display has it, like Nuke.
            if !v.view.is_empty() && !cfg.views(&d).contains(&v.view) {
                v.view.clear();
            }
            v.display = d;
            v.ocio = true;
        }
    }
    if let Some(view) = str_param(cmd, p, "view")? {
        let cfg = s.color.ocio()?;
        let d = resolve_display(&cfg, &v.display);
        let views = cfg.views(&d);
        if !view.is_empty() && !views.iter().any(|x| x == view) {
            return Err(bad(cmd, format!("display `{d}` has no view `{view}` (one of: {})", views.join(", "))));
        }
        v.view = view.to_string();
        v.ocio = true;
    }
    if let Some(look) = str_param(cmd, p, "look")? {
        let look = if look == "none" { "" } else { look };
        let cfg = s.color.ocio()?;
        if !look.is_empty() && !cfg.looks().iter().any(|l| l == look) {
            return Err(bad(cmd, format!("no look `{look}` (one of: none, {})", cfg.looks().join(", "))));
        }
        v.look = look.to_string();
    }
    if let Some(e) = num_param(cmd, p, "exposure", EXPOSURE_RANGE)? {
        v.exposure = e;
    }
    if let Some(g) = num_param(cmd, p, "gamma", GAMMA_RANGE)? {
        v.gamma = g;
    }
    if v.ocio {
        resolve(&s.color.ocio()?, &v)?;
    }
    let persist = v.saved() != s.color.viewer.saved();
    s.color.viewer = v;
    if persist {
        // Saved with the preferences.
        s.prefs.edit(|_| ());
    }
    Ok(s.color.viewer_report())
}

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, run: crate::commands::Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled: crate::commands::always, run, journal: false }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "view.viewerOptions",
            "Viewer Options…",
            &["View"],
            r#"{"ocio":bool,"display":"<OCIO display>|none","view":"<view>","look":"<look>|none","exposure":-20..20=0,"gamma":0.1..10=1} (all optional; {} reports the viewer and the config's displays, views and looks)"#,
            |s, p| apply(s, "view.viewerOptions", p),
        ),
        spec("view.ocio.display", "OCIO Display", &[], r#"{"display":"<OCIO display>|none"} (turns the OCIO viewer on; none = ICC monitor display)"#, |s, p| {
            apply(s, "view.ocio.display", p)
        }),
        spec("view.ocio.view", "OCIO View", &[], r#"{"view":"<view of the display>"} ("" = the display's default; turns the OCIO viewer on)"#, |s, p| {
            apply(s, "view.ocio.view", p)
        }),
        spec("view.ocio.look", "OCIO Look", &[], r#"{"look":"<look>|none"}"#, |s, p| apply(s, "view.ocio.look", p)),
        spec("view.exposure", "Viewer Exposure", &[], r#"{"exposure":-20..20=0} (stops of scene-linear gain before the view)"#, |s, p| {
            apply(s, "view.exposure", p)
        }),
        spec("view.gamma", "Viewer Gamma", &[], r#"{"gamma":0.1..10=1} (display gamma after the view)"#, |s, p| apply(s, "view.gamma", p)),
    ]
}

#[cfg(test)]
#[path = "viewer_tests.rs"]
mod tests;
