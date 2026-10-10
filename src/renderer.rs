// src/renderer.rs
//
// GPU rendering of one or more terminal panes into a wgpu surface.
//
// - Glyphs: glyphon (cosmic-text shaping + etagere atlas). Each row is cut
//   into positioned segments: runs of plain ASCII (one shaped buffer each),
//   and every other character (wide CJK, emoji, Nerd Font icons, combining
//   sequences) as its own segment placed at its exact column. A fallback
//   font with a different advance width can therefore never push the rest
//   of the line out of alignment.
// - Shaping is cached by row *content*, not row position: scrolling moves
//   rows without re-shaping them, and an unchanged screen re-shapes nothing.
// - Rectangles (cell backgrounds, underlines, cursor, scrollbar, bell flash)
//   go through a small instanced quad pipeline: backgrounds before text,
//   decorations after.
//
// This module consumes `frame::Frame`s and knows nothing about
// alacritty_terminal's grid, so the frame logic stays headlessly testable.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use alacritty_terminal::term::cell::Flags;
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphonColor, Family, FontSystem, Metrics, Resolution, Shaping,
    Style, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use wgpu::util::DeviceExt;

use crate::boxdraw;
use crate::frame::{CursorShape, Frame, RenderCell};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Font settings resolved to physical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub family: String,
    pub fallback: Vec<String>,
    pub size_px: f32,
    pub line_height: f32,
    pub ligatures: bool,
}

/// One pane to draw: its frame and where it goes on the surface.
pub struct PaneView<'a> {
    pub rect: Rect,
    pub frame: &'a Frame,
    /// 0..1 strength of the visual-bell flash.
    pub flash: f32,
    /// 0..1 how much to fade the pane toward its background (unfocused
    /// splits).
    pub dim: f32,
}

/// A plain rectangle drawn over everything (split dividers, ...).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Overlay {
    pub rect: Rect,
    pub color: [u8; 3],
    pub alpha: f32,
}

/// The output surface + clear color for one `TermRenderer::render` call.
pub struct FrameTarget<'a> {
    pub view: &'a wgpu::TextureView,
    pub width_px: u32,
    pub height_px: u32,
    pub clear_color: wgpu::Color,
    /// Alpha for cell backgrounds (window opacity, 0-1). Text is always
    /// drawn fully opaque -- only backgrounds fade, matching how
    /// Ghostty/Kitty/Alacritty do window transparency.
    pub background_alpha: f32,
    /// True when the surface's `CompositeAlphaMode` is `PreMultiplied`, in
    /// which case background RGB is pre-multiplied by alpha before writing.
    pub premultiply: bool,
}

const QUAD_SHADER: &str = r#"
struct Uniforms {
    screen_size: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

struct InstanceInput {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: InstanceInput) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];
    let px = instance.rect.x + corner.x * instance.rect.z;
    let py = instance.rect.y + corner.y * instance.rect.w;
    let ndc_x = (px / u.screen_size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (py / u.screen_size.y) * 2.0;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Instanced rectangles: 8 floats each (x, y, w, h, r, g, b, a).
#[derive(Default)]
struct Quads {
    data: Vec<f32>,
}

impl Quads {
    fn push(&mut self, x: f32, y: f32, w: f32, h: f32, rgba: [f32; 4]) {
        if w > 0.0 && h > 0.0 {
            self.data.extend_from_slice(&[x, y, w, h]);
            self.data.extend_from_slice(&rgba);
        }
    }

    /// Straight color with alpha, pre-multiplied for the overlay pipeline.
    fn push_overlay(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3], a: f32) {
        let c = rgb.map(|v| v as f32 / 255.0 * a);
        self.push(x, y, w, h, [c[0], c[1], c[2], a]);
    }

    fn count(&self) -> u32 {
        (self.data.len() / 8) as u32
    }

    fn upload(&self, device: &wgpu::Device, label: &str) -> Option<wgpu::Buffer> {
        (!self.data.is_empty()).then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: &pack_f32s(&self.data),
                usage: wgpu::BufferUsages::VERTEX,
            })
        })
    }
}

struct Segment {
    col: usize,
    buffer: Buffer,
}

struct CachedRow {
    segments: Vec<Segment>,
    last_used: u64,
}

/// Rows kept around after they leave the screen, so scrolling back and
/// forth doesn't re-shape them.
const ROW_CACHE_LIMIT: usize = 4096;

pub struct TermRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    _cache: Cache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,

    font: FontSpec,
    metrics: Metrics,
    cell_width: f32,
    cell_height: f32,

    rows: HashMap<u64, CachedRow>,
    generation: u64,
    /// Characters already checked against the loaded fonts, and whether
    /// any of them (possibly after a fontconfig lookup) has a glyph.
    coverage: HashMap<char, bool>,

    bg_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    quad_uniform_buffer: wgpu::Buffer,
    quad_bind_group: wgpu::BindGroup,
    images: crate::renderer_images::ImagePainter,
}

impl TermRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font: FontSpec,
    ) -> Self {
        let mut font_system = load_font_system(&font);
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        let (metrics, cell_width, cell_height) = measure(&mut font_system, &font);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cyberterm quad shader"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cyberterm quad bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let quad_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cyberterm quad uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cyberterm quad bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: quad_uniform_buffer.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cyberterm quad pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // Backgrounds overwrite exactly (so the window's alpha is exactly
        // the configured opacity); overlays blend pre-multiplied color.
        let bg_pipeline = quad_pipeline(device, &layout, &shader, format, None, "bg");
        let overlay_pipeline = quad_pipeline(
            device,
            &layout,
            &shader,
            format,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            "overlay",
        );

        let images = crate::renderer_images::ImagePainter::new(device, format, &bind_group_layout);

        Self {
            font_system,
            swash_cache,
            _cache: cache,
            viewport,
            atlas,
            text_renderer,
            font,
            metrics,
            cell_width,
            cell_height,
            rows: HashMap::new(),
            generation: 0,
            coverage: HashMap::new(),
            bg_pipeline,
            overlay_pipeline,
            quad_uniform_buffer,
            quad_bind_group,
            images,
        }
    }

    /// Applies a new font (size, family, fallback). Returns true when the
    /// cell size changed, meaning panes need a resize.
    pub fn set_font(&mut self, font: FontSpec) -> bool {
        if font == self.font {
            return false;
        }
        if font.family != self.font.family || font.fallback != self.font.fallback {
            self.font_system = load_font_system(&font);
            self.coverage.clear();
        }
        self.font = font;
        let old = (self.cell_width, self.cell_height);
        let (metrics, w, h) = measure(&mut self.font_system, &self.font);
        self.metrics = metrics;
        self.cell_width = w;
        self.cell_height = h;
        self.rows.clear();
        old != (w, h)
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_width, self.cell_height)
    }

    /// How many whole cells fit in a rectangle of this pixel size.
    pub fn grid_size(&self, width_px: f32, height_px: f32) -> (usize, usize) {
        let cols = (width_px / self.cell_width).floor().max(2.0) as usize;
        let rows = (height_px / self.cell_height).floor().max(1.0) as usize;
        (cols, rows)
    }

    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: FrameTarget<'_>,
        panes: &[PaneView<'_>],
        overlays: &[Overlay],
        images: &[crate::renderer_images::ImageDraw],
    ) {
        let FrameTarget {
            view,
            width_px,
            height_px,
            clear_color,
            background_alpha,
            premultiply,
        } = target;
        self.generation += 1;

        queue.write_buffer(
            &self.quad_uniform_buffer,
            0,
            &pack_f32s(&[width_px as f32, height_px as f32, 0.0, 0.0]),
        );

        let mut bg = Quads::default();
        let mut overlay = Quads::default();
        // (row cache key, left, top, clip bounds)
        let mut placements: Vec<(u64, f32, f32, TextBounds)> = Vec::new();

        for pane in panes {
            let frame = pane.frame;
            let Rect { x: ox, y: oy, .. } = pane.rect;
            let bounds = TextBounds {
                left: pane.rect.x as i32,
                top: pane.rect.y as i32,
                right: (pane.rect.x + pane.rect.w).ceil() as i32,
                bottom: (pane.rect.y + pane.rect.h).ceil() as i32,
            };
            let bg_alpha = background_alpha;
            let bg_mul = if premultiply { bg_alpha } else { 1.0 };

            for row in 0..frame.rows {
                let cells = frame.row(row);
                let y = oy + row as f32 * self.cell_height;

                // Background runs. The default background is left to the
                // clear color so padding and cells match exactly.
                let mut col = 0;
                while col < cells.len() {
                    let color = cells[col].bg;
                    let start = col;
                    while col < cells.len() && cells[col].bg == color {
                        col += 1;
                    }
                    if color != frame.bg {
                        let c = color.map(|v| v as f32 / 255.0 * bg_mul);
                        bg.push(
                            ox + start as f32 * self.cell_width,
                            y,
                            (col - start) as f32 * self.cell_width,
                            self.cell_height,
                            [c[0], c[1], c[2], bg_alpha],
                        );
                    }
                }

                self.decorations(&mut overlay, cells, ox, y);

                let key = row_key(cells);
                if !self.rows.contains_key(&key) {
                    let segments = self.shape_row(cells);
                    self.rows.insert(
                        key,
                        CachedRow {
                            segments,
                            last_used: 0,
                        },
                    );
                }
                if let Some(cached) = self.rows.get_mut(&key) {
                    cached.last_used = self.generation;
                }
                placements.push((key, ox, y, bounds));
            }

            if let Some(cursor) = frame.cursor {
                self.cursor(&mut overlay, cursor, ox, oy);
            }
            self.scrollbar(&mut overlay, pane);
            if pane.dim > 0.0 {
                overlay.push_overlay(
                    ox,
                    oy,
                    pane.rect.w,
                    pane.rect.h,
                    frame.bg,
                    pane.dim.min(1.0),
                );
            }
            if pane.flash > 0.0 {
                let fg = frame.row(0).first().map(|c| c.fg).unwrap_or([0xff; 3]);
                overlay.push_overlay(ox, oy, pane.rect.w, pane.rect.h, fg, 0.2 * pane.flash);
            }
        }

        for o in overlays {
            overlay.push_overlay(o.rect.x, o.rect.y, o.rect.w, o.rect.h, o.color, o.alpha);
        }

        let mut text_areas = Vec::new();
        for (key, left, top, bounds) in &placements {
            if let Some(cached) = self.rows.get(key) {
                for seg in &cached.segments {
                    text_areas.push(TextArea {
                        buffer: &seg.buffer,
                        left: left + seg.col as f32 * self.cell_width,
                        top: *top,
                        scale: 1.0,
                        bounds: *bounds,
                        default_color: GlyphonColor::rgb(255, 255, 255),
                        custom_glyphs: &[],
                    });
                }
            }
        }

        self.viewport.update(
            queue,
            Resolution {
                width: width_px,
                height: height_px,
            },
        );
        if let Err(e) = self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            eprintln!("cyberterm: text prepare failed: {e:?}");
        }

        let image_buf = self.images.prepare(device, queue, images);
        let bg_buf = bg.upload(device, "cyberterm bg quads");
        let overlay_buf = overlay.upload(device, "cyberterm overlay quads");
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cyberterm frame pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if let Some(buf) = &bg_buf {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_bind_group(0, &self.quad_bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..bg.count());
            }

            if let Err(e) = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
            {
                eprintln!("cyberterm: text render failed: {e:?}");
            }

            // Images over text (kitty's default z), under decorations.
            if let Some(buf) = &image_buf {
                self.images
                    .draw(&mut pass, &self.quad_bind_group, buf, images);
            }

            if let Some(buf) = &overlay_buf {
                pass.set_pipeline(&self.overlay_pipeline);
                pass.set_bind_group(0, &self.quad_bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..overlay.count());
            }
        }

        self.atlas.trim();
        self.evict_rows();
    }

    fn evict_rows(&mut self) {
        if self.rows.len() <= ROW_CACHE_LIMIT {
            return;
        }
        let mut ages: Vec<u64> = self.rows.values().map(|r| r.last_used).collect();
        ages.sort_unstable();
        let cutoff = ages[ages.len() - ROW_CACHE_LIMIT / 2];
        let current = self.generation;
        self.rows
            .retain(|_, r| r.last_used >= cutoff || r.last_used == current);
    }

    /// Makes sure some loaded font can draw `ch`. Only the configured
    /// families are loaded up front (see `load_font_system`), so the first
    /// time a character none of them covers shows up (CJK, rare symbols),
    /// fontconfig is asked for a font that has it and that font is added.
    fn ensure_coverage(&mut self, ch: char) {
        if self.coverage.contains_key(&ch) {
            return;
        }
        let ids: Vec<_> = self.font_system.db().faces().map(|f| f.id).collect();
        let mut covered = ids.into_iter().any(|id| {
            self.font_system
                .get_font(id)
                .is_some_and(|font| font.rustybuzz().glyph_index(ch).is_some())
        });
        if !covered {
            let query = format!(":charset={:x}", ch as u32);
            if let Some(path) = font_files_matching(&query).into_iter().next() {
                let already =
                    self.font_system.db().faces().any(
                        |f| matches!(&f.source, glyphon::fontdb::Source::File(p) if *p == path),
                    );
                if !already {
                    let _ = self.font_system.db_mut().load_font_file(&path);
                    covered = true;
                }
            }
        }
        self.coverage.insert(ch, covered);
    }

    /// Cuts a row into shaped, column-positioned text segments.
    fn shape_row(&mut self, cells: &[RenderCell]) -> Vec<Segment> {
        for cell in cells {
            if !cell.ch.is_ascii() && !boxdraw::is_drawn(cell.ch) {
                self.ensure_coverage(cell.ch);
            }
        }
        let shaping_for_runs = if self.font.ligatures {
            Shaping::Advanced
        } else {
            Shaping::Basic
        };
        segment_row(cells)
            .into_iter()
            .map(|seg| {
                let shaping = if seg.plain_ascii {
                    shaping_for_runs
                } else {
                    Shaping::Advanced
                };
                Segment {
                    col: seg.col,
                    buffer: make_buffer(
                        &mut self.font_system,
                        self.metrics,
                        Family::Monospace,
                        &seg.text,
                        seg.style,
                        shaping,
                    ),
                }
            })
            .collect()
    }

    fn decorations(&self, quads: &mut Quads, cells: &[RenderCell], ox: f32, y: f32) {
        let cw = self.cell_width;
        let font_px = self.font.size_px;
        let thick = (font_px / 14.0).round().max(1.0);
        let pad = (self.cell_height - font_px) / 2.0;
        let underline_y = (y + pad + font_px * 0.82 + thick).round();
        let strike_y = (y + pad + font_px * 0.5).round();

        for (col, cell) in cells.iter().enumerate() {
            let x = ox + col as f32 * cw;
            if let Some(pieces) = boxdraw::pieces(cell.ch, x, y, cw, self.cell_height, thick) {
                for p in pieces {
                    quads.push_overlay(p.x, p.y, p.w, p.h, cell.fg, p.alpha);
                }
            }
            let color = cell.underline_color.unwrap_or(cell.fg);
            let f = cell.flags;
            if f.contains(Flags::UNDERLINE) || cell.link_hover {
                quads.push_overlay(
                    x,
                    underline_y,
                    cw,
                    thick,
                    if cell.link_hover { cell.fg } else { color },
                    1.0,
                );
            }
            if f.contains(Flags::DOUBLE_UNDERLINE) {
                quads.push_overlay(x, underline_y - thick, cw, thick, color, 1.0);
                quads.push_overlay(x, underline_y + thick, cw, thick, color, 1.0);
            }
            if f.contains(Flags::UNDERCURL) {
                // A small triangle wave, four steps per cell.
                let step = cw / 4.0;
                for (i, lift) in [0.0, 1.0, 2.0, 1.0].iter().enumerate() {
                    quads.push_overlay(
                        x + i as f32 * step,
                        underline_y - lift * thick,
                        step,
                        thick,
                        color,
                        1.0,
                    );
                }
            }
            if f.contains(Flags::DOTTED_UNDERLINE) {
                let mut dx = 0.0;
                while dx < cw {
                    quads.push_overlay(x + dx, underline_y, thick.min(cw - dx), thick, color, 1.0);
                    dx += thick * 2.0;
                }
            }
            if f.contains(Flags::DASHED_UNDERLINE) {
                quads.push_overlay(x, underline_y, cw * 0.6, thick, color, 1.0);
            }
            if f.contains(Flags::STRIKEOUT) {
                quads.push_overlay(x, strike_y, cw, thick, cell.fg, 1.0);
            }
        }
    }

    fn cursor(&self, quads: &mut Quads, cursor: crate::frame::CursorDraw, ox: f32, oy: f32) {
        let (cw, ch) = (self.cell_width, self.cell_height);
        let x = ox + cursor.col as f32 * cw;
        let y = oy + cursor.row as f32 * ch;
        let w = if cursor.wide { cw * 2.0 } else { cw };
        let thick = (cw / 8.0).round().max(1.0);
        match cursor.shape {
            // Drawn by inverting the cell itself (frame.rs).
            CursorShape::Block => {}
            CursorShape::Beam => quads.push_overlay(x, y, thick, ch, cursor.color, 1.0),
            CursorShape::Underline => {
                quads.push_overlay(x, y + ch - thick, w, thick, cursor.color, 1.0)
            }
            CursorShape::Hollow => {
                quads.push_overlay(x, y, w, 1.0, cursor.color, 1.0);
                quads.push_overlay(x, y + ch - 1.0, w, 1.0, cursor.color, 1.0);
                quads.push_overlay(x, y, 1.0, ch, cursor.color, 1.0);
                quads.push_overlay(x + w - 1.0, y, 1.0, ch, cursor.color, 1.0);
            }
        }
    }

    /// A thin position indicator while scrolled back into history.
    fn scrollbar(&self, quads: &mut Quads, pane: &PaneView<'_>) {
        let frame = pane.frame;
        if frame.display_offset == 0 || frame.history == 0 {
            return;
        }
        let total = (frame.history + frame.rows) as f32;
        let h = (pane.rect.h * frame.rows as f32 / total).max(16.0);
        // 0 at the top of the scrollback, 1 at the live screen.
        let position = (frame.history - frame.display_offset) as f32 / frame.history as f32;
        let y = pane.rect.y + (pane.rect.h - h) * position;
        let w = (self.cell_width / 2.0).max(3.0);
        let fg = frame.row(0).first().map(|c| c.fg).unwrap_or([0xff; 3]);
        quads.push_overlay(pane.rect.x + pane.rect.w - w, y, w, h, fg, 0.45);
    }
}

/// The text attributes that change shaping or glyph color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Style8 {
    fg: [u8; 3],
    bold: bool,
    italic: bool,
}

impl Style8 {
    fn of(cell: &RenderCell) -> Self {
        Self {
            fg: cell.fg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
        }
    }
}

fn row_key(cells: &[RenderCell]) -> u64 {
    let mut h = DefaultHasher::new();
    for cell in cells {
        cell.ch.hash(&mut h);
        cell.zerowidth.hash(&mut h);
        Style8::of(cell).hash(&mut h);
        cell.is_spacer().hash(&mut h);
    }
    h.finish()
}

fn make_buffer(
    fs: &mut FontSystem,
    metrics: Metrics,
    family: Family<'_>,
    text: &str,
    style: Style8,
    shaping: Shaping,
) -> Buffer {
    let mut buffer = Buffer::new(fs, metrics);
    buffer.set_size(fs, None, None);
    let attrs = Attrs::new()
        .family(family)
        .weight(if style.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        })
        .style(if style.italic {
            Style::Italic
        } else {
            Style::Normal
        })
        .color(GlyphonColor::rgb(style.fg[0], style.fg[1], style.fg[2]));
    buffer.set_text(fs, text, attrs, shaping);
    buffer.shape_until_scroll(fs, false);
    buffer
}

/// One piece of a row to shape: a run of same-style plain ASCII, or a
/// single other character (wide, emoji, icon, combining sequence).
#[derive(Debug, PartialEq)]
struct TextSegment {
    col: usize,
    text: String,
    style: Style8,
    plain_ascii: bool,
}

/// Splits a row into segments. Every segment has exactly one style:
/// cosmic-text's Basic shaping applies the *first* span's color and
/// weight to a whole multi-span buffer, so styles must never be mixed
/// within one buffer.
fn segment_row(cells: &[RenderCell]) -> Vec<TextSegment> {
    let mut out: Vec<TextSegment> = Vec::new();
    let mut run: Option<TextSegment> = None;
    let flush = |run: &mut Option<TextSegment>, out: &mut Vec<TextSegment>| {
        if let Some(mut seg) = run.take() {
            let trimmed = seg.text.trim_end().len();
            seg.text.truncate(trimmed);
            if !seg.text.is_empty() {
                out.push(seg);
            }
        }
    };
    for (col, cell) in cells.iter().enumerate() {
        if cell.is_spacer() {
            continue;
        }
        if boxdraw::is_drawn(cell.ch) {
            // Drawn as rectangles in `decorations`.
            flush(&mut run, &mut out);
            continue;
        }
        let style = Style8::of(cell);
        let plain = cell.ch.is_ascii()
            && !cell.ch.is_ascii_control()
            && cell.zerowidth.is_none()
            && !cell.flags.contains(Flags::WIDE_CHAR);
        if plain {
            if let Some(seg) = &mut run {
                if seg.style == style {
                    seg.text.push(cell.ch);
                    continue;
                }
                // A space can join any run: its color is invisible.
                if cell.ch == ' ' {
                    seg.text.push(' ');
                    continue;
                }
            }
            flush(&mut run, &mut out);
            if cell.ch != ' ' {
                run = Some(TextSegment {
                    col,
                    text: cell.ch.to_string(),
                    style,
                    plain_ascii: true,
                });
            }
        } else {
            flush(&mut run, &mut out);
            let mut text = cell.ch.to_string();
            if let Some(extra) = &cell.zerowidth {
                text.extend(extra.iter());
            }
            if text.trim().is_empty() || cell.ch.is_control() {
                continue;
            }
            out.push(TextSegment {
                col,
                text,
                style,
                plain_ascii: false,
            });
        }
    }
    flush(&mut run, &mut out);
    out
}

fn quad_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&format!("cyberterm {label} quad pipeline")),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: 32,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &[
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 16,
                        shader_location: 1,
                    },
                ],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

/// Builds a `FontSystem` around only the configured families instead of
/// `FontSystem::new()`'s full system scan, which parses every installed
/// font file -- several seconds on a machine with ~10,000 font files, the
/// dominant cost of a startup delay easily mistaken for a hang.
/// fontconfig's own cache resolves each family to its files in milliseconds.
///
/// The defaults matter: the Nerd Font variant of JetBrains Mono for prompt
/// icons, "Symbols Nerd Font Mono" for icons a given patch set lacks, and
/// "Noto Color Emoji" for real color emoji (glyphon has a color atlas).
fn load_font_system(font: &FontSpec) -> FontSystem {
    let mut db = glyphon::fontdb::Database::new();
    let mut primary_name = None;

    for (i, family) in std::iter::once(&font.family)
        .chain(font.fallback.iter())
        .enumerate()
    {
        let before: HashSet<_> = db.faces().map(|f| f.id).collect();
        for path in font_files(family) {
            let _ = db.load_font_file(&path);
        }
        if i == 0 {
            // The name *inside* the font, which can differ from what the
            // user typed ("JetBrains Mono" vs "JetBrainsMono Nerd Font").
            primary_name = db
                .faces()
                .find(|f| !before.contains(&f.id))
                .and_then(|f| f.families.first().map(|(name, _)| name.clone()));
        }
    }

    if db.faces().next().is_none() {
        // fontconfig missing or found nothing -- fall back to the full,
        // slower system scan rather than rendering with zero fonts.
        return FontSystem::new();
    }
    db.set_monospace_family(primary_name.unwrap_or_else(|| font.family.clone()));
    FontSystem::new_with_locale_and_db("en-US".to_string(), db)
}

/// Every file of a family (regular, bold, italic, ...) via `fc-list`,
/// falling back to `fc-match`'s single best file for fuzzy names.
fn font_files(family: &str) -> Vec<std::path::PathBuf> {
    let run = |args: &[&str]| -> Vec<std::path::PathBuf> {
        std::process::Command::new(args[0])
            .args(&args[1..])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let files = run(&["fc-list", family, "-f", "%{file}\n"]);
    if !files.is_empty() {
        return files;
    }
    font_files_matching(family)
}

/// fontconfig's single best file for a pattern (`"JetBrains Mono"`,
/// `":charset=65e5"`).
fn font_files_matching(pattern: &str) -> Vec<std::path::PathBuf> {
    std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
        .map(|p| vec![std::path::PathBuf::from(p)])
        .unwrap_or_default()
}

/// Cell size from the font's real advance width, so column alignment is
/// right for whatever monospace font is installed.
fn measure(font_system: &mut FontSystem, font: &FontSpec) -> (Metrics, f32, f32) {
    let line_height = (font.size_px * font.line_height).round().max(1.0);
    let metrics = Metrics::new(font.size_px, line_height);
    let mut probe = Buffer::new(font_system, metrics);
    probe.set_size(font_system, None, None);
    probe.set_text(
        font_system,
        "0000000000",
        Attrs::new().family(Family::Monospace),
        Shaping::Basic,
    );
    probe.shape_until_scroll(font_system, false);
    let width = probe
        .layout_runs()
        .next()
        .map(|run| run.line_w / 10.0)
        .filter(|w| *w > 0.0)
        .unwrap_or(font.size_px * 0.6);
    (metrics, width, line_height)
}

/// Packs an f32 slice into little-endian bytes for a GPU buffer write,
/// without pulling in `bytemuck` for a handful of floats.
fn pack_f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char, fg: u8) -> RenderCell {
        RenderCell {
            fg: [fg; 3],
            ch,
            ..RenderCell::blank([0; 3], [0; 3])
        }
    }

    fn texts(cells: &[RenderCell]) -> Vec<(usize, String, u8)> {
        segment_row(cells)
            .into_iter()
            .map(|s| (s.col, s.text, s.style.fg[0]))
            .collect()
    }

    #[test]
    fn style_changes_start_new_segments() {
        // "Copy  Ctrl" with the hint in a different color.
        let mut row: Vec<RenderCell> = "Copy  ".chars().map(|c| cell(c, 200)).collect();
        row.extend("Ctrl".chars().map(|c| cell(c, 70)));
        assert_eq!(
            texts(&row),
            vec![(0, "Copy".into(), 200), (6, "Ctrl".into(), 70)]
        );
    }

    #[test]
    fn spaces_join_runs_and_trailing_spaces_are_dropped() {
        let row: Vec<RenderCell> = "  ab cd   ".chars().map(|c| cell(c, 1)).collect();
        assert_eq!(texts(&row), vec![(2, "ab cd".into(), 1)]);
    }

    #[test]
    fn wide_and_box_characters_are_separate() {
        let mut row = vec![cell('a', 1), cell('界', 1)];
        row[1].flags = Flags::WIDE_CHAR;
        let mut spacer = cell(' ', 1);
        spacer.flags = Flags::WIDE_CHAR_SPACER;
        row.push(spacer);
        row.extend([cell('b', 1), cell('─', 1), cell('c', 1)]);
        assert_eq!(
            texts(&row),
            vec![
                (0, "a".into(), 1),
                (1, "界".into(), 1),
                (3, "b".into(), 1),
                (5, "c".into(), 1)
            ]
        );
    }
}
