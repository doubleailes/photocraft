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
| Modes | RGB and Grayscale. CMYK, Lab, Indexed, Bitmap, Duotone and Multichannel are deleted (done, see step 3a); files in those modes are converted when opened. |
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
     * a. **Done.** Remove the non-RGB/gray modes (phase 8's mode removal, pulled forward; details below).
     * b. **Done.** No integer document in a session (details below).
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
  Shape layers re-render after the conversion, as any edit would. Text and smart-object pixels are
  converted, since they may be Photoshop's own rendering. Smart-object contents and library patterns
  are linearised when they enter a float RGB/gray document (render, stack modes, Edit Contents,
  unpack, fills, overlays).
  Float files (EXR, HDR, 32-bit PSD) keep their values and depth. Files in other colour models are
  converted to RGB or gray before they reach the door (step 3a).
* **New documents.** RGB/gray `file.new` makes 16-bit half float (or 32-bit float when asked), tagged linear.
  The New dialog offers 16/32-bit float. Image › Mode › 16 Bits/Channel (`image.mode.bits16`) converts
  an open document to half float, 32 Bits/Channel to float (step 3b).
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
* **Display and output paths that bypass the view transform.** The channel view encodes a linear
  document's channels for the screen. Print encodes to untagged sRGB/sGray. Camera Raw (engine and
  dialog) develops float documents sRGB-encoded, as its pipeline expects, and decodes the result.
  Mode conversions of float documents land in the target mode's linear profile.
* **Clipboard.** `Clip.icc_profile` records the copied pixels' profile. Paste converts into the
  target document, and the OS clipboard gets working-space 8-bit RGBA.
* **Not converted yet** (follow-ups): adjustment-layer colours (Photo Filter, Black & White tint),
  character/paragraph style presets, the Info panel and colour samplers (they show document
  values), and `gradientFill.get`, which reports stops as document values.

### Half-float step 3a: RGB and grayscale only

`photocraft_color::ColorMode` is `{Grayscale, Rgb}`. Everything that existed for the other models is gone:
the Image › Mode commands (CMYK, Lab, Indexed Color, Color Table, Bitmap, Duotone, Multichannel), Image ›
Trap, `Document.color_table`/`duotone`, the CMYK/Lab compositing paths (CPU and GPU), `CmykSpace`,
`ToneSpace` (Levels/Curves are the composite plus red/green/blue; old `space`/`black` fields in saved
files are ignored), CMYK/Lab file export and the `channel.merge` CMYK/Lab targets.

* **Import** (`crates/io/src/native.rs`). Layered PSD/PSB and flat files convert their samples as they
  are read, at the file's depth, then go through the linear door like any integer file:
  * CMYK through the embedded CMYK profile, else the built-in coated CMYK (relative colorimetric + BPC), to sRGB.
  * Lab through the D50 formulas to sRGB (16-bit Lab chroma as Photoshop stores it).
  * Indexed via its palette to RGB; Bitmap and Duotone (the gray channel; inks not interpreted) to Gray.
  * Multichannel: the inks are kept as spot channels and printed (`print_inks`) into one locked
    Background RGB layer.
  * Descriptor colours (CMYK, Lab) become RGB; CMYK/Lab Levels/Curves keep the composite only;
    CMYK/Lab patterns convert like layers. Each conversion adds an import warning.
* **CMYK stays a proof and print target.** Proof Setup, Proof Colors, Gamut Warning and the plate
  previews still use the working CMYK space; File › Print with PhotoCraft-managed colour converts the
  flattened composite straight to the printer profile (CMYK included). `edit.convertToProfile` and
  `edit.assignProfile` refuse CMYK and Lab profiles.
* `file.new`, the New dialog, Contact Sheet and Conditional Mode Change offer RGB and Grayscale only.
  The perf scenario P33 (CMYK brush dab) is retired.

### Half-float step 3b: no integer documents in a session

* `Session::add_document` converts an integer document to linear half float, so every door (and
  every scratch session a command makes) holds linear float documents only. `open_document` still
  applies the Color Settings policy first.
* Image › Mode offers 16 Bits/Channel (half float, `image.mode.bits16`) and 32 Bits/Channel;
  8 Bits/Channel and the interim `image.mode.bits16f` are gone. Depth changes keep documents linear.
* `linear_doc::legacy_new` (the test-only integer File › New) is gone; the tests that used it run on
  linear documents and expect linear values.
* Picked colours entering a layer mask or a channel are data, not colours: 50% gray is 0.5 coverage
  (`Session::data_target`, set while a command targets a mask or channel).
* Integer depths stay where they are data or files: codec I/O, `Document.source_depth` exports,
  PSD/TIFF reading and the Photoshop-matching integer paths of `compose`/`algo`, which `photocraft_io`
  uses for files as read (PSD oracle composites) before they reach a session.
* Parity: 8 Bits/Channel is a Photoshop menu item this fork drops (floor 619 → 618).

### Half-float step 4: before and after (measured 2026-10-08)

Benchmark: `crates/ui-egui/examples/large_image_bench.rs`, release, `--cpu` (no GPU on the
measuring machine: 4 cores, 15 GB RAM, Linux). Before = `24c04a5` (last commit before any half-float
code), after = `138a80a` (steps 1–3b), fixed = the open-path work below. Synthetic 8-bit RGB photo,
24 MP (6000×4000) and 36 MP (7360×4912), one operation per process. Times are the bench's own
single-run figures, so treat differences under about 10% as noise. Peak RSS is the kernel's
high-water mark (`VmHWM`) for that process.

| Scenario | 24 MP before | after | fixed | 36 MP before | after | fixed |
|---|---|---|---|---|---|---|
| Open (decode + document) | 0.52 s | 5.2–5.5 s | **0.82–1.07 s** | 0.73 s | 8.1–8.4 s | **1.36–1.46 s** |
| First refresh (CPU) | 234–251 ms | 233–266 ms | 255–276 ms | 313–335 ms | 356–411 ms | 375–407 ms |
| Full refresh (3 runs) | 210–255 ms | 228–259 ms | 265–267 ms | 317–345 ms | 388–427 ms | 395–433 ms |
| Gaussian Blur r 10 | 458 ms | 672 ms | 732 ms | 688 ms | 1126 ms | 1102 ms |
| Brush, 40 dabs (size 120) | 12–13 ms | 15–19 ms | 16–18 ms | 12–16 ms | 15–20 ms | 16–19 ms |
| Peak RSS, open / refresh | 482–492 MB | 1267 MB | **582–584 MB** | 721–729 MB | 1902 MB | **1012 MB** |
| Peak RSS, filter | 707 MB | 1267 MB | 889 MB | 971 MB | 1902 MB | 1405 MB |

What made open slow, and the fixes:

* **Serial depth conversion.** `Surface::convert` walked the tiles one by one and allocated a
  `Vec` per pixel; the f32 widen and the f16 narrow both went through it. It now converts tiles in
  parallel and a same-model conversion allocates nothing per pixel (5.4 s → 3.2 s at 24 MP).
* **Full-size float copies.** `to_linear` cloned the document twice, and the widen, the colour
  transform's staging buffer and its output were three full-size f32 copies alive at once (the
  1.3 GB peak). `to_linear` now works in place, and `linearize_surface` takes each tile source →
  f32 → transform → f16 on its own through `Surface::map_tiles_into` (3.2 s → 1.5 s; peak RSS
  1342 → 590 MB).
* **Exact pipeline per pixel.** The precise float path evaluated the TRC (`powf`) for every pixel.
  8-bit sources now use `Transform::convert_u8_to_f32`, which reads the input curves from the
  256-entry tables already computed exactly, when the transform is curves plus a matrix with no
  output curves (a profile to its linear version); it matches the exact path to 1e-5, far below
  f16 precision (1.5 s → 0.8 s).

Where this leaves step 4:

* **Memory is what half float costs.** RGBA f16 is 2× the bytes of RGBA8: the open/refresh peak is
  1.2× before at 24 MP and 1.4× at 36 MP, not 2.6×.
* **Open is 1.6–2× before.** What remains is the f16 encode and the transform itself, about
  0.3 s at 24 MP on 4 cores (the JPEG decode is unchanged, about 0.5 s). 16-bit sources still go
  through the exact pipeline per pixel (not measured here); a 65 536-entry table would do for them
  what `curves8` does for 8-bit.
* **Interactive paths are unchanged by the fix and hold up:** refresh within about 10–20% of
  before, a brush dab in the tens of ms at 36 MP. Filters are the slowest hit (+60% at both
  sizes), since they now work on 2× the bytes.

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
   * Delete `crates/cms`, Proof Setup/Proof Colors/Gamut Warning, `proof_sim.rs` and `profileMismatch`. (The CMYK/Lab/Indexed/Bitmap/Duotone/Multichannel modes are already gone: half-float step 3a.)
   * Update `docs/parity.md`, the parity floor and the scorecard: the fork removes those menu items on purpose.
   * Update `AGENTS.md` rule 2 to say: colour goes through `photocraft-ocio`, never ICC.

## Open questions

* Grayscale: kept as a mode (single-channel half) for now; drop it (RGB only) later?
* Gradient and brush mixing space: linear always, or offer a perceptual option (the `color_picking` role)?
* 8-bit PNG export default: `sRGB - Display` encoding via the default view, or the plain `sRGB - Texture` inverse?
