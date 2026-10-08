# OCIO migration (fork: doubleailes/photocraft)

Replace ICC colour management with OpenColorIO, using the pure-Rust
[`ocio`](https://github.com/doubleailes/ocio-rs) crate. Target: an animation/VFX tool
with a scene-linear pipeline, like Nuke or Mari. This fork diverges from upstream
Photoshop parity on purpose: CMYK, Lab, ICC proofing and the profile menus go away.

## Decisions

| Topic | Decision |
|---|---|
| ICC | Removed as a colour-management system. `crates/cms` is deleted. |
| ICC tags on export | A tiny **write-only** matrix/TRC ICC tagger (sRGB, Display P3, Rec.709, Rec.2020, linear variants) so PNG/JPEG/TIFF exports are read correctly by browsers and review tools. No ICC transforms. |
| Embedded ICC on import | Mapped to a named config colour space (primaries/white/TRC match), else an anonymous transform, via ocio-rs [#8](https://github.com/doubleailes/ocio-rs/issues/8). |
| Pixel storage | **Every document is RGBA half float (f16) in the config's `scene_linear` role.** 8/16-bit integer sources are converted on import. 32-bit float documents stay f32. |
| Compositing | Linear, f32 working precision, in `scene_linear`. Blend modes behave like Nuke, not Photoshop. |
| Modes | RGB only (+ Grayscale as RGB with a flag, TBD). CMYK, Lab and Indexed are deleted. |
| Config | Default `ocio://cg-config-latest` (ACES 2.0, OCIO 2.5). `$OCIO` overrides it. A Color Settings override (file path) wins over both. |
| Viewer | Display / View / Look pickers + exposure/gamma before the view transform. Baked LUT (shaper + 64³ Rgba16Float) now, analytic WGSL once ocio-rs [#9](https://github.com/doubleailes/ocio-rs/issues/9) lands. |

## ocio-rs prerequisites

* [#7](https://github.com/doubleailes/ocio-rs/issues/7): never panic, plus fuzzing (**blocking**; until then every call goes through `catch_unwind`).
* [#8](https://github.com/doubleailes/ocio-rs/issues/8): IO proxy and ICC from bytes (**blocking** for the web build and for ICC import).
* [#9](https://github.com/doubleailes/ocio-rs/issues/9): WGSL shaders (nice to have).

## Phases

Each phase leaves the tree green. Half floats go first: once every document is linear f16,
the later phases only convert at import and export and never branch on depth.

### Half-float scope (measured 2026-10-08)

* There are about 750 `SampleType::` references outside `codecs`: 386 `U8`, 166 `U16`, 199 `F32`. About 360 of them are in test files.
* By crate: io 40, engine 28, ui-egui 26, algo 17, compose 9, format 6, gpu 5, and a few each elsewhere.
* `codecs` already decodes and encodes `F16`, and `gpu` already depends on `half`.
* Strategy:
  1. **Done.** Add `F16` to `photocraft_color::SampleType`, with tile storage, the compose read/write path and GPU upload. All behind tests at every depth.
  2. **Done.** Make import and new documents produce f16 linear only. Convert U8/U16 sources at the door (details below).
  3. Delete the U8/U16 document paths and their tests. Keep integer types only for codec I/O and masks.
  4. Benchmark at 24–36 MP before and after: tile memory, composite time, brush latency.

### Half-float step 2 (`crates/engine/src/linear_doc.rs`)

Until the OCIO phases land, "linear" means the ICC engine's linear profiles: linear sRGB
(`Builtin::LinearSrgb`) and linear gray (`Builtin::LinearGray`, added for this). Float documents
were already treated as linear everywhere (adjustment transfer, Photo Filter, 32-bit preview), so
half-float documents follow the same path.

* **Doors.** Integer RGB and grayscale documents are converted to linear half float, through f32 so 16-bit
  sources keep their precision. The doors are: File › Open (sync, background job, automation), Revert, New, New from Clipboard and
  paste into an empty session, Load Files into Stack, batch processing, Photomerge results and tone-mapped Merge to HDR Pro results.
  Wide-gamut sources keep their colours (unclamped, out-of-range values in linear sRGB).
  Float files (EXR, HDR, 32-bit PSD) keep their values and depth. CMYK, Lab, Indexed, Bitmap,
  Duotone and Multichannel keep their depth and encoding until phase 8 removes them.
* **New documents.** RGB/gray `file.new` makes 16-bit half float (or 32-bit float when asked), tagged linear.
  The New dialog offers 16/32-bit float for RGB and gray. `image.mode.bits16f` converts an open
  document; Image › Mode › 16 Bits/Channel shows checked for half float.
* **Source depth.** `Document.source_depth` (saved in `.pcraft`) records the integer depth of
  the file a document came from. Saving a linear document to a format without float (PNG, JPEG,
  GIF, …), or to any non-HDR format when it came from an integer file, encodes it to sRGB/sGray at that depth (8-bit for
  documents made here). Every 8-bit value survives open → save as PNG. EXR/HDR stay linear float. PSD
  stays 32-bit float for now (phase 6 encodes it).
* **Colours.** Tool colours are picked in the working RGB space. `Session::to_doc_color`,
  `fg()`, `bg()` and `ColorConv` convert them where they enter pixels or document data. That covers brushes,
  pencil, erasers, bucket, gradients (tool, fill layers, presets, explicit stops), Fill, Stroke,
  Clear/Cut, canvas extension, rotate, shapes, path fill/stroke, text, Solid/Gradient fill layers, layer styles (commands,
  dialog, style presets), Color Range, Replace Color, render filters, lens edge colour and artboard
  backgrounds. Filter params record `colorsInDocument` so replays don't convert twice.
  The Eyedropper (`canvas::composite_color`) and the Layer Style dialog convert document values back.
* **Clipboard.** `Clip.icc_profile` records the copied pixels' profile. Paste converts into the
  target document, and the OS clipboard gets working-space 8-bit RGBA.
* **Not converted yet** (follow-ups): adjustment-layer colours (Photo Filter, Black & White tint),
  character/paragraph style presets, the Info panel and colour samplers (they show document
  values), and `gradientFill.get`, which reports stops as document values.

1. **Half-float documents** (see the scope above).
2. **`crates/ocio` wrapper (L0, new).**
   * Owns config loading in this order: Color Settings path, then `$OCIO`, then `ocio://cg-config-latest`.
   * A processor cache that replaces `photocraft_cms::transform::cached`.
   * Role helpers (`scene_linear`, `color_picking`, `texture_paint`, `data`).
   * A panic guard, and a `bake_display_lut(display, view, look, size)` with a shaper.
   * Register it in `xtask/src/layers.rs`.
3. **Document model.**
   * `Document.icc_profile` becomes `color_space: String`. Pixels are always `scene_linear`, so this records the *source/intent* space for round-trip export. Add `ocio_config: String` (URI or path, informational).
   * `.pcraft`: new fields get `#[serde(default)]`; old files with ICC bytes are linearised on load.
4. **Linear compositor.**
   * Remove the display-RGB assumptions in `crates/compose` (including text coverage gamma) and match the GPU compositor.
   * The colour picker, swatches and the foreground/background colours go through `color_picking`, so picked values are shown in the picking space and stored linear.
   * Brushes and gradients interpolate in linear (optionally in `color_picking` for gradients, a TBD setting).
   * Update the CPU/GPU parity tests.
5. **Viewer.**
   * `display_color.rs` and `gpu_canvas.rs`: the display/view/look state lives in `ui-egui/src/state.rs`, set through engine commands (`view.ocio.display`, `view.ocio.view`, `view.ocio.look`, `view.exposure`, `view.gamma`).
   * The LUT texture becomes Rgba16Float with a log shaper input.
   * The 32-bit preview options are replaced by exposure/gamma.
6. **File I/O.**
   * EXR: read `chromaticities` and map to a config space, else use file rules.
   * PNG/JPEG/TIFF/PSD: use the embedded ICC (via #8), else file rules (`default` → `sRGB - Texture`).
   * Export: pick an output colour space or display/view (like Nuke's Write node). Integer formats get encoded plus an ICC tag from the tiny writer. EXR stays linear half/float.
   * PSD export: encode to a chosen space at 16-bit integer (or 32-bit float) and tag it.
7. **Commands.**
   * `edit.assignProfile` becomes `edit.assignColorSpace` (re-interpret the source). `edit.convertToProfile` becomes `edit.convertColorSpace` (change the intent space and re-encode the pixels).
   * `edit.colorSettings` becomes the OCIO config, working-space info and the file-rules view.
   * The Color Lookup adjustment loads any OCIO `FileTransform` (cube, CLF/CTF, CDL, …) and gains an OCIO "Look" adjustment layer.
8. **Removal.**
   * Delete `crates/cms`, CMYK/Lab/Indexed modes, Proof Setup/Proof Colors/Gamut Warning, `proof_sim.rs` and `profileMismatch`.
   * Update `docs/parity.md`, the parity floor and the scorecard: the fork removes those menu items on purpose.
   * Update `AGENTS.md` rule 2 to say: colour goes through `photocraft-ocio`, never ICC.

## Open questions

* Grayscale: keep it as a mode (single-channel half) or drop it (RGB only)?
* Gradient and brush mixing space: linear always, or offer a perceptual option (the `color_picking` role)?
* 8-bit PNG export default: `sRGB - Display` encoding via the default view, or the plain `sRGB - Texture` inverse?
