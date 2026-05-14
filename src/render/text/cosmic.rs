//! cosmic-text / glyphon backend for [`TextEngine`].
//!
//! Owns the `FontSystem`, glyph atlas, per-row buffer pool, and the
//! `EvelynFallback` font-cascade override. All glyphon types are
//! confined to this file so the rest of the renderer doesn't depend on
//! a specific shaping library.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use anyhow::Result;
use glyphon::{
    cosmic_text::{FeatureTag, FontFeatures, fontdb, Fallback, PlatformFallback},
    Attrs, Buffer, Cache, Color as GColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use unicode_script::Script;
use wgpu::MultisampleState;

use crate::color::{Rgb, cursor_color, cursor_text_color, default_fg};
use crate::config::config as live_config;

use super::{BUNDLED_FONT_NAME, FONT_PRIMARY_BOLD_BYTES, FONT_PRIMARY_REGULAR_BYTES, PreeditMetrics, Run, TextEngine};

pub struct CosmicEngine {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// One Buffer per text run. Pool grows as needed and is reused
    /// across frames; `set_text` is called every frame so old content
    /// past `runs.len()` is harmless (just not referenced).
    row_buffers: Vec<Buffer>,
    /// Runs in the exact layout that was shaped into `row_buffers`
    /// during the most recent `shape_runs` call — buf[i] holds the
    /// shaped glyphs for `last_runs[i]`. `prepare` reads positions from
    /// this so the bold-TP split (which inserts extra runs) stays
    /// consistent between shaping and area placement.
    last_runs: Vec<Run>,
    /// Hash of the most recent `shape_runs` inputs. When the next call
    /// matches, the run list, row buffers, and shaped glyph state from
    /// the previous frame are still valid — we skip set_text / re-shape
    /// entirely. Invalidated by `set_metrics` (font/size changed) so a
    /// config or theme reload always reshapes once. Theme-driven fg/bg
    /// changes that survive a metrics reload still invalidate naturally
    /// via the `run.fg` bytes in the hash.
    last_shape_key: Option<u64>,
    /// Cursor position that was active when `last_runs` was shaped.
    /// Needed by the per-run cache so the cursor-cell's effective fg
    /// (cursor_text vs. its own fg) participates in the fingerprint of
    /// the *previous* frame consistently with the current frame's.
    last_cursor_pos: Option<(u16, u16)>,
    preedit_buffer: Buffer,
    font_size: f32,
    line_height: f32,
    cell_width: f32,
}

impl CosmicEngine {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font_size: f32,
        line_height: f32,
    ) -> Self {
        // Mirror what `FontSystem::new()` does — load system fonts into a
        // fresh db — but install our own `Fallback` so glyphs cosmic-text's
        // built-in macOS table doesn't cover (e.g. Braille for gtop
        // sparklines, or Misc-Technical symbols that should render
        // monochrome) get routed correctly.
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db.load_font_data(FONT_PRIMARY_REGULAR_BYTES.to_vec());
        db.load_font_data(FONT_PRIMARY_BOLD_BYTES.to_vec());
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
        let mut font_system =
            FontSystem::new_with_locale_and_db_and_fallback(locale, db, EvelynFallback::new());
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        let preedit_buffer = make_buffer(&mut font_system, font_size, line_height);
        let cell_width = measure_cell_width(&mut font_system, font_size, line_height);

        Self {
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            row_buffers: Vec::new(),
            last_runs: Vec::new(),
            last_shape_key: None,
            last_cursor_pos: None,
            preedit_buffer,
            font_size,
            line_height,
            cell_width,
        }
    }

    /// Number of fonts the engine ended up loading. Logged on startup so
    /// it's easy to verify the bundled font got picked up.
    pub fn font_count(&self) -> usize {
        self.font_system.db().faces().count()
    }
}

impl TextEngine for CosmicEngine {
    fn set_metrics(&mut self, font_size: f32, line_height: f32) {
        self.font_size = font_size;
        self.line_height = line_height;
        let m = Metrics::new(font_size, line_height);
        for buf in &mut self.row_buffers {
            buf.set_metrics(&mut self.font_system, m);
        }
        self.preedit_buffer.set_metrics(&mut self.font_system, m);
        self.cell_width = measure_cell_width(&mut self.font_system, font_size, line_height);
        // Metrics change invalidates both the frame-level cache key and
        // the per-run fingerprint lookup: glyph advances and line height
        // differ, so previously-shaped buffers no longer match the new
        // cell grid. Clearing `last_runs` empties the per-run lookup map
        // built on the next call, forcing a full reshape of leftover
        // buffers (which already have the new metrics set above).
        self.last_shape_key = None;
        self.last_runs.clear();
        self.last_cursor_pos = None;
    }

    fn cell_width(&self) -> f32 {
        self.cell_width
    }

    fn trim(&mut self) {
        // The old atlas is keyed by the previous font/size — drop cached
        // glyph rasters so freshly-shaped runs don't sample stale entries.
        self.atlas.trim();
    }

    fn shape_preedit(&mut self, preedit: &str, preedit_cursor: usize) -> PreeditMetrics {
        let base = font_attrs().color(rgb_to_gcolor(cursor_color()));
        self.preedit_buffer
            .set_monospace_width(&mut self.font_system, Some(self.cell_width));
        self.preedit_buffer.set_text(
            &mut self.font_system,
            preedit,
            &base,
            Shaping::Advanced,
            None,
        );
        self.preedit_buffer
            .shape_until_scroll(&mut self.font_system, false);
        let mut max_x: f32 = 0.0;
        let mut caret_x: f32 = 0.0;
        let mut caret_set = false;
        for run in self.preedit_buffer.layout_runs() {
            for g in run.glyphs.iter() {
                if !caret_set && g.start >= preedit_cursor {
                    caret_x = g.x;
                    caret_set = true;
                }
                max_x = max_x.max(g.x + g.w);
            }
        }
        if !caret_set {
            caret_x = max_x;
        }
        PreeditMetrics {
            width: max_x,
            caret_x,
        }
    }

    fn shape_runs(&mut self, runs: &[Run], cursor_pos: Option<(u16, u16)>) {
        // Frame-level early-out: if the run list and cursor position are
        // byte-identical to the previous call, `row_buffers` already hold
        // valid shaped glyphs and `last_runs` already has the right (col,
        // row) layout — `prepare` can read them as-is.
        let mut hasher = DefaultHasher::new();
        runs.len().hash(&mut hasher);
        for r in runs {
            r.col.hash(&mut hasher);
            r.row.hash(&mut hasher);
            r.bold.hash(&mut hasher);
            r.fg.0.hash(&mut hasher);
            r.fg.1.hash(&mut hasher);
            r.fg.2.hash(&mut hasher);
            r.text.hash(&mut hasher);
        }
        cursor_pos.hash(&mut hasher);
        let key = hasher.finish();
        if self.last_shape_key == Some(key) {
            return;
        }
        self.last_shape_key = Some(key);

        // Work around a cosmic-text 0.18 fallback bug: under Bold attrs
        // the matcher can't honor our `common_fallback` ordering (the
        // weight-diff filter knocks out every name in the list) and
        // falls into the global pool, which sorts emoji fonts to the
        // top via `FontMatchKey::not_emoji`. Net effect: a Bold ⏺ ends
        // up rendered by Apple Color Emoji even though Regular ⏺ is
        // correctly served by STIX Two Math. Splitting Bold runs at
        // text-presentation symbols and dropping Bold on those cells
        // keeps the matcher in the Regular path, where our chain wins.
        //
        // The split rewrites the run list — extra entries get inserted
        // wherever a Bold run mixes TP and non-TP chars — so `prepare`
        // must read positions back from this rewritten list, not the
        // caller's original. Cache it on the engine.
        let new_runs: Vec<Run> = match expand_bold_text_presentation_runs(runs) {
            Some(split) => split,
            None => runs.to_vec(),
        };

        // Build a fingerprint → previous-buffer-index lookup. Shaping
        // depends on (text, fg, bold, is_cursor) — (col, row) is purely
        // positional and applied later by `prepare`. That means a line of
        // text that just scrolled up by one row keeps its fingerprint
        // and reuses its shaped buffer with zero work. Big win for
        // scrollback browsing, `tail -f`, build logs, vim/helix scrolls.
        let prev_cursor = self.last_cursor_pos;
        let mut prev_lookup: HashMap<u64, VecDeque<usize>> = HashMap::new();
        for (i, r) in self.last_runs.iter().enumerate() {
            let was_cursor = run_is_cursor(r, prev_cursor);
            prev_lookup
                .entry(run_fingerprint(r, was_cursor))
                .or_default()
                .push_back(i);
        }

        // Move old buffers into Option slots so we can take them by
        // index without disturbing the rest of the vec.
        let mut old_slots: Vec<Option<Buffer>> = self.row_buffers.drain(..).map(Some).collect();
        let mut new_buffers: Vec<Option<Buffer>> = (0..new_runs.len()).map(|_| None).collect();

        // Pass 1 — pick up cache hits. Anything we miss is reshaped in
        // Pass 2 below, reusing leftover buffers when possible.
        let mut misses: Vec<usize> = Vec::new();
        for (j, run) in new_runs.iter().enumerate() {
            let is_cursor = run_is_cursor(run, cursor_pos);
            let fp = run_fingerprint(run, is_cursor);
            let hit = prev_lookup
                .get_mut(&fp)
                .and_then(|q| q.pop_front())
                .and_then(|i| old_slots[i].take());
            match hit {
                Some(buf) => new_buffers[j] = Some(buf),
                None => misses.push(j),
            }
        }

        // Pass 2 — reshape misses, reusing leftover buffers from
        // `old_slots` so we don't churn allocations on scroll. Anything
        // truly left over after this is dropped at the end of scope.
        let mut leftover: Vec<Buffer> = old_slots.into_iter().flatten().collect();
        let base = font_attrs();
        for j in misses {
            let run = &new_runs[j];
            let mut buf = leftover
                .pop()
                .unwrap_or_else(|| make_buffer(&mut self.font_system, self.font_size, self.line_height));
            // Unbounded width so a row run never wraps internally; a
            // stale buffer width after a window resize would otherwise
            // cause the overflow to render as a second line in the cell
            // below — which looks exactly like "the frame got newlined."
            buf.set_size(&mut self.font_system, None, Some(self.line_height));
            let is_cursor = run_is_cursor(run, cursor_pos);
            let fg = if is_cursor {
                cursor_text_color()
            } else {
                run.fg
            };
            let mut attrs = base.clone().color(rgb_to_gcolor(fg));
            if run.bold {
                attrs = attrs.weight(Weight::BOLD);
            }
            buf.set_text(
                &mut self.font_system,
                &run.text,
                &attrs,
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            new_buffers[j] = Some(buf);
        }

        // unwrap() is safe — every slot was filled by either Pass 1 or Pass 2.
        self.row_buffers = new_buffers.into_iter().map(|o| o.unwrap()).collect();
        self.last_runs = new_runs;
        self.last_cursor_pos = cursor_pos;
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_size: (u32, u32),
        cell_width: f32,
        line_height: f32,
        padding: f32,
        preedit_origin: Option<(f32, f32)>,
    ) -> Result<()> {
        self.viewport.update(
            queue,
            Resolution {
                width: surface_size.0,
                height: surface_size.1,
            },
        );

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: surface_size.0 as i32,
            bottom: surface_size.1 as i32,
        };

        let mut areas: Vec<TextArea> = self
            .last_runs
            .iter()
            .enumerate()
            .map(|(i, run)| TextArea {
                buffer: &self.row_buffers[i],
                left: run.col as f32 * cell_width + padding,
                top: run.row as f32 * line_height + padding,
                scale: 1.0,
                bounds,
                default_color: rgb_to_gcolor(default_fg()),
                custom_glyphs: &[],
            })
            .collect();
        if let Some((left, top)) = preedit_origin {
            areas.push(TextArea {
                buffer: &self.preedit_buffer,
                left,
                top,
                scale: 1.0,
                bounds,
                default_color: rgb_to_gcolor(cursor_color()),
                custom_glyphs: &[],
            });
        }

        self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        )?;
        Ok(())
    }

    fn render<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) -> Result<()> {
        self.text_renderer
            .render(&self.atlas, &self.viewport, pass)?;
        Ok(())
    }
}

fn make_buffer(font_system: &mut FontSystem, font_size: f32, line_height: f32) -> Buffer {
    Buffer::new(font_system, Metrics::new(font_size, line_height))
}

/// True when this run is exactly the cursor cell (single grapheme at
/// the cursor position). Matches the rule applied at shape time, so a
/// previous-frame run reuses its buffer only when both the old and new
/// frames agree on cursor-inversion state.
fn run_is_cursor(r: &Run, cursor_pos: Option<(u16, u16)>) -> bool {
    cursor_pos
        .map(|(cx, cy)| cx == r.col && cy == r.row && r.text.chars().count() == 1)
        .unwrap_or(false)
}

/// Fingerprint a run by exactly the inputs that shaping consumes:
/// `text`, `fg`, `bold`, and the cursor-inversion flag. `(col, row)` is
/// deliberately out — a buffer's shaped glyph stream is position-agnostic,
/// `prepare` translates it into a TextArea later — so a line that scrolls
/// up one row keeps its fingerprint and reuses its shaped buffer.
fn run_fingerprint(r: &Run, is_cursor: bool) -> u64 {
    let mut h = DefaultHasher::new();
    r.text.hash(&mut h);
    r.fg.0.hash(&mut h);
    r.fg.1.hash(&mut h);
    r.fg.2.hash(&mut h);
    r.bold.hash(&mut h);
    is_cursor.hash(&mut h);
    h.finish()
}

/// Estimate the advance width of a single monospace glyph by shaping a
/// probe string. Falls back to a fraction of the font size when shaping
/// is empty (e.g. font load failed).
fn measure_cell_width(fs: &mut FontSystem, font_size: f32, line_height: f32) -> f32 {
    const PROBE: &str = "MMMMMMMMMM";
    let mut buf = Buffer::new(fs, Metrics::new(font_size, line_height));
    buf.set_size(fs, Some(10_000.0), Some(line_height * 2.0));
    let attrs = font_attrs();
    buf.set_text(fs, PROBE, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(fs, false);
    let mut max_x: f32 = 0.0;
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            max_x = max_x.max(glyph.x + glyph.w);
        }
    }
    if max_x > 0.0 {
        max_x / PROBE.len() as f32
    } else {
        font_size * 0.6
    }
}

fn rgb_to_gcolor(c: Rgb) -> GColor {
    GColor::rgb(c.0, c.1, c.2)
}

fn font_attrs() -> Attrs<'static> {
    let cfg = live_config();
    let name = current_family_name(cfg.font.family.as_deref().unwrap_or(BUNDLED_FONT_NAME));
    let mut a = Attrs::new().family(Family::Name(name));
    if !cfg.font.ligatures {
        a = a.font_features(ligatures_off());
    }
    a
}

/// Intern the active font-family name as a `&'static str` so `Attrs<'static>`
/// stays valid across hot reloads. Double-check on the read side avoids
/// leaking on every render — only a genuine family change leaks one new
/// string. `BUNDLED_FONT_NAME` is already static so the common case never
/// allocates.
fn current_family_name(want: &str) -> &'static str {
    use std::sync::RwLock;
    static SLOT: std::sync::OnceLock<RwLock<&'static str>> = std::sync::OnceLock::new();
    let slot = SLOT.get_or_init(|| RwLock::new(BUNDLED_FONT_NAME));
    {
        let cur = slot.read().unwrap();
        if *cur == want {
            return *cur;
        }
    }
    let leaked: &'static str = if want == BUNDLED_FONT_NAME {
        BUNDLED_FONT_NAME
    } else {
        Box::leak(want.to_string().into_boxed_str())
    };
    *slot.write().unwrap() = leaked;
    leaked
}

/// Codepoints whose Unicode default is *text* presentation but which
/// also have an emoji glyph in `Apple Color Emoji`. Subset of
/// `emoji-data.txt` rows where `Emoji=Yes` and `Emoji_Presentation=No`,
/// trimmed to the symbols a terminal actually meets (media controls,
/// arrows, weather, hearts, etc.). Anything not in this list either
/// has no emoji conflict (regular text) or is a true emoji whose
/// emoji-presentation rendering is the desired outcome.
fn is_text_presentation_default(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x2122 |                    // ™
        0x2139 |                    // ℹ
        0x2194..=0x2199 |           // arrows
        0x21A9..=0x21AA |
        0x23E9..=0x23F3 |           // ⏩⏪⏫⏬⏭⏮⏯⏰⏱⏲⏳
        0x23F8..=0x23FA |           // ⏸⏹⏺
        0x25AA..=0x25AB |
        0x25B6 | 0x25C0 |
        0x25FB..=0x25FE |
        0x2600..=0x2604 |           // weather
        0x260E |                    // ☎
        0x2611 | 0x2614 | 0x2615 |
        0x2618 | 0x261D | 0x2620 |
        0x2622..=0x2623 |
        0x2626 | 0x262A |
        0x262E..=0x262F |
        0x2638..=0x263A |
        0x2640 | 0x2642 |
        0x265F..=0x2660 |
        0x2663 | 0x2665..=0x2666 |
        0x2668 | 0x267B | 0x267E..=0x267F |
        0x2692..=0x2697 |
        0x2699 | 0x269B..=0x269C |
        0x26A0..=0x26A1 | 0x26A7 |
        0x26AA..=0x26AB | 0x26B0..=0x26B1 |
        0x26BD..=0x26BE | 0x26C4..=0x26C5 |
        0x26C8 | 0x26CE..=0x26CF |
        0x26D1 | 0x26D3..=0x26D4 |
        0x26E9..=0x26EA | 0x26F0..=0x26F5 |
        0x26F7..=0x26FA | 0x26FD |
        0x2702 | 0x2708..=0x270D |
        0x270F | 0x2712 | 0x2714 | 0x2716 |
        0x271D | 0x2721 |
        0x2733..=0x2734 | 0x2744 | 0x2747 |
        0x2763..=0x2764 |
        0x27A1 | 0x2934..=0x2935 |
        0x2B05..=0x2B07 |
        0x3030 | 0x303D
    )
}

/// Walk `runs` and split any Bold run that contains a text-presentation
/// symbol into per-segment sub-runs, dropping Bold on the symbol cells.
/// Returns `None` when no run needs splitting (the common case) so the
/// caller can avoid the alloc.
///
/// `build_runs` already forces wide cells into solo runs, so multi-char
/// runs here are guaranteed all narrow — column advance per char is 1.
fn expand_bold_text_presentation_runs(runs: &[Run]) -> Option<Vec<Run>> {
    let needs_split = runs
        .iter()
        .any(|r| r.bold && r.text.chars().any(is_text_presentation_default));
    if !needs_split {
        return None;
    }
    let mut out: Vec<Run> = Vec::with_capacity(runs.len() + 4);
    for r in runs {
        if !r.bold || !r.text.chars().any(is_text_presentation_default) {
            out.push(r.clone());
            continue;
        }
        let mut col = r.col;
        let mut buf = String::new();
        let mut current_is_tp: Option<bool> = None;
        for c in r.text.chars() {
            let tp = is_text_presentation_default(c);
            match current_is_tp {
                None => current_is_tp = Some(tp),
                Some(prev) if prev != tp => {
                    let chars = buf.chars().count() as u16;
                    out.push(Run {
                        col,
                        row: r.row,
                        text: std::mem::take(&mut buf),
                        fg: r.fg,
                        bold: r.bold && !prev,
                    });
                    col += chars;
                    current_is_tp = Some(tp);
                }
                _ => {}
            }
            buf.push(c);
        }
        if let Some(tp) = current_is_tp {
            out.push(Run {
                col,
                row: r.row,
                text: buf,
                fg: r.fg,
                bold: r.bold && !tp,
            });
        }
    }
    Some(out)
}

fn ligatures_off() -> FontFeatures {
    let mut f = FontFeatures::new();
    f.disable(FeatureTag::STANDARD_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_LIGATURES);
    f.disable(FeatureTag::CONTEXTUAL_ALTERNATES);
    f.disable(FeatureTag::DISCRETIONARY_LIGATURES);
    f
}

/// Fallback wrapper that augments cosmic-text's `PlatformFallback`. Two
/// jobs:
///
/// 1. **Braille** (`U+2800-U+28FF`): gtop/btop/htop sparklines need it,
///    Geist Mono and the macOS common fallbacks (Menlo, Geneva, Arial
///    Unicode MS) all miss the block, so without `Apple Braille` the
///    graph rendered as `.notdef` boxes.
/// 2. **Common-script symbols** (`⏺ ❤ ☎ ★ ☀` …): cosmic-text's font
///    matching is "first font in the chain that has the glyph wins" —
///    it does *not* consult Unicode's emoji-presentation default. The
///    upstream macOS `common_fallback` puts `Apple Color Emoji` ahead
///    of any monochrome symbol font, so text-default symbols got
///    rasterized in color even though iTerm2/WezTerm/Alacritty render
///    them monochrome. Note: `script_fallback(Common)` is **never**
///    queried — `cosmic_text::shape` filters Common/Latin/Inherited/
///    Unknown out of its scripts list — so the fix has to live in
///    `common_fallback`. We splice `Apple Symbols` and `STIX Two Math`
///    in *before* `Apple Color Emoji`. True emoji codepoints (🚀 🍣)
///    aren't in those monochrome fonts and still fall through to
///    `Apple Color Emoji`.
struct EvelynFallback {
    inner: PlatformFallback,
}

impl EvelynFallback {
    fn new() -> Self {
        Self { inner: PlatformFallback }
    }
}

impl Fallback for EvelynFallback {
    fn common_fallback(&self) -> &[&'static str] {
        // Mirror upstream macOS order, but inject monochrome symbol fonts
        // ahead of Apple Color Emoji. STIX Two Math is the only stock
        // font that carries Unicode 6.0+ Misc Technical additions like
        // U+23FA (BLACK CIRCLE FOR RECORD); Apple Symbols handles the
        // older ❤ ☎ ★ ☀ block.
        &[
            ".SF NS",
            "Menlo",
            "Apple Symbols",
            "STIX Two Math",
            "Apple Color Emoji",
            "Geneva",
            "Arial Unicode MS",
        ]
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        self.inner.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        match script {
            Script::Braille => &["Apple Braille"],
            _ => self.inner.script_fallback(script, locale),
        }
    }
}
