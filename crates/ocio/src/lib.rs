//! OpenColorIO for PhotoCraft, through the pure-Rust [`ocio`] crate (ocio-rs).
//!
//! * [`Ocio::load`] finds the config: a Color Settings path, else `$OCIO`, else the built-in
//!   [`DEFAULT_CONFIG`] (ACES 2.0 CG config).
//! * Display, view and look lists for the viewer pickers, and the config's roles.
//! * [`Ocio::bake_viewer`] bakes source space → look → display/view into a [`ViewerLut`]: a
//!   log [`Shaper`] then a `size`³ 3D LUT, which the canvas samples on the GPU
//!   (`Rgba16Float`) and the CPU ([`ViewerLut::apply`]) alike.
//!
//! ocio-rs may still panic on hostile configs (ocio-rs #7), so every call into it runs under
//! [`guard`] and a panic comes back as an [`OcioError`].
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::panic::{AssertUnwindSafe, catch_unwind};

/// The config used when neither Color Settings nor `$OCIO` names one.
pub const DEFAULT_CONFIG: &str = "ocio://cg-config-latest";

/// Largest LUT edge [`Ocio::bake_viewer`] accepts (129³ RGBA f32 is 34 MB).
pub const MAX_LUT_SIZE: usize = 129;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("OCIO: {0}")]
pub struct OcioError(pub String);

pub type Result<T> = std::result::Result<T, OcioError>;

impl From<ocio::Error> for OcioError {
    fn from(e: ocio::Error) -> Self {
        Self(e.message().to_string())
    }
}

/// Run `f`, turning a panic inside ocio-rs into an error (ocio-rs #7).
pub fn guard<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            Err(OcioError(format!("{what} failed inside ocio-rs: {msg}")))
        }
    }
}

/// Where the config came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// Edit › Color Settings › OCIO config (a path or an `ocio://` URI).
    Settings,
    /// The `$OCIO` environment variable.
    Env,
    /// [`DEFAULT_CONFIG`].
    Default,
}

impl ConfigOrigin {
    pub fn id(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Env => "env",
            Self::Default => "default",
        }
    }
}

/// A loaded OCIO config.
pub struct Ocio {
    config: ocio::Config,
    /// The path or URI it was loaded from.
    pub source: String,
    pub origin: ConfigOrigin,
}

impl std::fmt::Debug for Ocio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ocio").field("source", &self.source).field("origin", &self.origin).finish_non_exhaustive()
    }
}

fn open(spec: &str) -> Result<ocio::Config> {
    guard("loading the config", || {
        if spec.starts_with("ocio://") {
            Ok(ocio::Config::create_from_builtin_config(spec)?)
        } else {
            Ok(ocio::Config::create_from_file(spec)?)
        }
    })
}

impl Ocio {
    /// Load the config: `settings` (Color Settings; empty = unset), else `$OCIO`, else
    /// [`DEFAULT_CONFIG`]. A config named by settings or `$OCIO` that fails to load is an error
    /// (the user asked for it); nothing silently falls back.
    pub fn load(settings: &str) -> Result<Self> {
        let settings = settings.trim();
        if !settings.is_empty() {
            return Self::open(settings, ConfigOrigin::Settings);
        }
        match std::env::var("OCIO") {
            Ok(env) if !env.trim().is_empty() => Self::open(env.trim(), ConfigOrigin::Env),
            _ => Self::open(DEFAULT_CONFIG, ConfigOrigin::Default),
        }
    }

    /// Load `spec` (a file path or an `ocio://` URI).
    pub fn open(spec: &str, origin: ConfigOrigin) -> Result<Self> {
        let config = open(spec).map_err(|e| OcioError(format!("can't load `{spec}`: {}", e.0)))?;
        Ok(Self { config, source: spec.to_string(), origin })
    }

    /// The config's name (may be empty).
    pub fn name(&self) -> String {
        guard("reading the name", || Ok(self.config.name().to_string())).unwrap_or_default()
    }

    /// Active displays, in config order.
    pub fn displays(&self) -> Vec<String> {
        guard("listing displays", || Ok((0..self.config.num_displays()).map(|i| self.config.display(i)).filter(|d| !d.is_empty()).collect())).unwrap_or_default()
    }

    pub fn default_display(&self) -> String {
        guard("reading the default display", || Ok(self.config.default_display())).unwrap_or_default()
    }

    /// The views of `display` (empty when it isn't a display of the config).
    pub fn views(&self, display: &str) -> Vec<String> {
        guard("listing views", || Ok((0..self.config.num_views(display)).map(|i| self.config.view(display, i)).filter(|v| !v.is_empty()).collect())).unwrap_or_default()
    }

    pub fn default_view(&self, display: &str) -> String {
        guard("reading the default view", || Ok(self.config.default_view(display))).unwrap_or_default()
    }

    /// The config's looks.
    pub fn looks(&self) -> Vec<String> {
        guard("listing looks", || Ok((0..self.config.num_looks()).map(|i| self.config.look_name_by_index(i).to_string()).filter(|l| !l.is_empty()).collect()))
            .unwrap_or_default()
    }

    /// The colour space a role names (`None` when the config doesn't define it).
    pub fn role(&self, role: &str) -> Option<String> {
        guard("reading a role", || Ok(self.config.role_color_space(role).to_string())).ok().filter(|s| !s.is_empty())
    }

    /// Is `name` a colour space (or alias) of the config?
    pub fn has_color_space(&self, name: &str) -> bool {
        guard("finding a colour space", || Ok(self.config.get_color_space(name).is_some())).unwrap_or(false)
    }

    /// The first of `names` that is a colour space of the config, else the `scene_linear` role.
    pub fn find_space(&self, names: &[&str]) -> Option<String> {
        names.iter().find(|n| self.has_color_space(n)).map(|n| n.to_string()).or_else(|| self.role(ROLE_SCENE_LINEAR))
    }

    /// Bake `src` → `look` (empty = none) → `display`/`view` into a `size`³ LUT behind
    /// [`Shaper::DEFAULT`]. Unknown names are errors.
    pub fn bake_viewer(&self, src: &str, display: &str, view: &str, look: &str, size: usize) -> Result<ViewerLut> {
        if !(2..=MAX_LUT_SIZE).contains(&size) {
            return Err(OcioError(format!("LUT size {size} (2..={MAX_LUT_SIZE})")));
        }
        if !self.has_color_space(src) {
            return Err(OcioError(format!("no colour space `{src}` in the config")));
        }
        if !self.displays().iter().any(|d| d == display) {
            return Err(OcioError(format!("no display `{display}` in the config")));
        }
        if !self.views(display).iter().any(|v| v == view) {
            return Err(OcioError(format!("display `{display}` has no view `{view}`")));
        }
        if !look.is_empty() && !self.looks().iter().any(|l| l == look) {
            return Err(OcioError(format!("no look `{look}` in the config")));
        }
        let shaper = Shaper::DEFAULT;
        guard("baking the viewer", || {
            let mut pipe = ocio::apphelpers::legacy_viewing_pipeline::LegacyViewingPipeline::new();
            pipe.set_display_view_transform(Some(&ocio::DisplayViewTransform::new(src, display, view)));
            if !look.is_empty() {
                pipe.set_looks_override_enabled(true);
                pipe.set_looks_override(look);
            }
            let cpu = pipe.get_processor(&self.config)?.default_cpu_processor();
            let n = size;
            let s = (n - 1) as f32;
            let mut data = Vec::with_capacity(n * n * n * 3);
            for b in 0..n {
                for g in 0..n {
                    for r in 0..n {
                        data.extend_from_slice(&[shaper.decode(r as f32 / s), shaper.decode(g as f32 / s), shaper.decode(b as f32 / s)]);
                    }
                }
            }
            cpu.apply_rgb_slice(&mut data);
            let data = data.chunks_exact(3).map(|c| [finite(c[0]), finite(c[1]), finite(c[2])]).collect();
            Ok(ViewerLut { size: n, shaper, data })
        })
    }
}

/// The `scene_linear` role.
pub const ROLE_SCENE_LINEAR: &str = "scene_linear";

fn finite(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-65504.0, 65504.0) } else { 0.0 }
}

/// Maps scene-linear values to the LUT's 0..1 domain on a log2 scale:
/// `t = (log2(x + 2^lo) - lo) / (log2(2^hi + 2^lo) - lo)`, so 0 → 0 exactly, `2^hi` → 1, and
/// each LUT step is a fixed fraction of a stop above `2^lo`. Values below 0 clamp to 0, above
/// `2^hi` to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shaper {
    /// log2 of the offset (the linear value where the curve turns from linear to log).
    pub lo: f32,
    /// log2 of the largest value kept.
    pub hi: f32,
}

impl Shaper {
    /// 2^-12 to 2^12 (0.000244 to 4096): 24 stops around 1.0. The canvas shader hard-codes the
    /// same numbers (`photocraft-ui-egui` checks they match).
    pub const DEFAULT: Self = Self { lo: -12.0, hi: 12.0 };

    fn span(&self) -> f32 {
        (self.hi.exp2() + self.lo.exp2()).log2() - self.lo
    }

    /// Linear → LUT coordinate in 0..=1.
    pub fn encode(&self, x: f32) -> f32 {
        let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, self.hi.exp2()) };
        ((x + self.lo.exp2()).log2() - self.lo) / self.span()
    }

    /// LUT coordinate → linear.
    pub fn decode(&self, t: f32) -> f32 {
        (self.lo + t.clamp(0.0, 1.0) * self.span()).exp2() - self.lo.exp2()
    }
}

/// A baked viewer transform: [`Shaper`], then a `size`³ RGB LUT (red fastest) of display
/// values.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerLut {
    pub size: usize,
    pub shaper: Shaper,
    pub data: Vec<[f32; 3]>,
}

impl ViewerLut {
    /// One scene-linear colour → display values (trilinear, as the GPU samples it).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        if n < 2 || self.data.len() < n * n * n {
            return rgb;
        }
        let s = (n - 1) as f32;
        let mut i0 = [0usize; 3];
        let mut f = [0.0f32; 3];
        for c in 0..3 {
            let t = self.shaper.encode(rgb[c]) * s;
            let i = (t.floor() as usize).min(n - 2);
            i0[c] = i;
            f[c] = (t - i as f32).clamp(0.0, 1.0);
        }
        let at = |r: usize, g: usize, b: usize| self.data.get(r + g * n + b * n * n).copied().unwrap_or_default();
        let mut out = [0.0f32; 3];
        for (corner, w) in (0..8usize).map(|k| {
            let (dr, dg, db) = (k & 1, (k >> 1) & 1, (k >> 2) & 1);
            let w = [dr, dg, db].iter().zip(&f).map(|(&d, &f)| if d == 1 { f } else { 1.0 - f }).product::<f32>();
            (at(i0[0] + dr, i0[1] + dg, i0[2] + db), w)
        }) {
            for c in 0..3 {
                out[c] += corner[c] * w;
            }
        }
        out
    }

    /// The LUT as `Rgba16Float` texels (alpha 1), red fastest, for a 3D texture upload.
    pub fn to_rgba16f_bytes(&self) -> Vec<u8> {
        let one = half::f16::from_f32(1.0).to_le_bytes();
        self.data.iter().flat_map(|p| {
            let [r, g, b] = p.map(|v| half::f16::from_f32(v).to_le_bytes());
            [r, g, b, one].into_iter().flatten()
        }).collect()
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
