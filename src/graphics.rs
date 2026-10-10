// src/graphics.rs
//
// Inline images: the kitty graphics protocol (used by `kitty +kitten
// icat`, timg, chafa, viu, yazi, ranger, matplotlib backends, ...).
//
// `GraphicsTap` filters the output stream like the OSC tap: it takes the
// protocol's APC sequences (`ESC _ G <keys> ; <payload> ESC \`) out,
// decodes the image, answers the program, and in place of a displayed
// image writes one *marker* character per image row into the stream.
// Markers are characters from Unicode's private plane 16 that encode a
// placement slot and the image row, so the image's position lives in the
// grid like any text: it scrolls, survives in scrollback, appears in
// rewind and in daemon panes, and goes away when overwritten. The renderer
// draws each marker as a strip of the image across the image's width.
//
// It also answers plain DA1 (`CSI c`) itself, with `CSI ? 62 ; 22 c`:
// alacritty_terminal's own answer (`CSI ? 6 c`) is too short for kitty's
// tools, which check for the graphics replies and then wait for a DA1
// answer longer than three characters -- with the short one they hang
// until their detection timeout.
//
// Supported: transmit (a=t), transmit+display (a=T), display (a=p), query
// (a=q), delete (a=d, by all / id); formats PNG (f=100), RGB (24), RGBA
// (32); zlib compression (o=z); chunked transfers (m=1); direct data
// (t=d) and files (t=f, t=t). Not supported: shared memory (t=s),
// animation, Unicode-placeholder placements, z-index (images are drawn
// over text).

// `as_chunks` needs Rust 1.88; `chunks_exact` keeps older toolchains working.
#![allow(clippy::chunks_exact_to_as_chunks)]

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;

/// Markers: U+100000 + slot * 256 + row.
const MARKER_BASE: u32 = 0x10_0000;
pub const SLOTS: usize = 255;
pub const MAX_ROWS: u16 = 256;
const MAX_COLS: u16 = 1000;
/// Largest APC (encoded) accepted, and total decoded image bytes kept per
/// pane.
const MAX_APC: usize = 128 * 1024 * 1024;
const MAX_STORE: usize = 320 * 1024 * 1024;
const MAX_SIDE: u32 = 10_000;

pub fn marker(slot: u8, row: u16) -> char {
    char::from_u32(MARKER_BASE + slot as u32 * 256 + row as u32).expect("plane 16 is valid")
}

/// (slot, row) if `c` is a marker.
pub fn decode_marker(c: char) -> Option<(u8, u16)> {
    let v = (c as u32).checked_sub(MARKER_BASE)?;
    let slot = v / 256;
    (slot < SLOTS as u32).then_some((slot as u8, (v % 256) as u16))
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// A decoded image (RGBA8).
pub struct Image {
    /// Unique across the process, for the renderer's texture cache.
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// An image shown at a spot: its size in cells, and how much of the last
/// column/row it fills (fx, fy in 0..=1), so it scales with the font.
#[derive(Clone)]
pub struct Placement {
    pub image: Arc<Image>,
    pub image_id: u32,
    pub cols: u16,
    pub rows: u16,
    pub fx: f32,
    pub fy: f32,
}

/// One pane's images.
pub struct Images {
    by_id: HashMap<u32, Arc<Image>>,
    placements: Vec<Option<Placement>>,
    next_slot: usize,
    /// Cell size in pixels, for sizing images in cells.
    pub cell: (f32, f32),
}

impl Default for Images {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            placements: vec![None; SLOTS],
            next_slot: 0,
            cell: (8.0, 16.0),
        }
    }
}

impl Images {
    pub fn placement(&self, slot: u8) -> Option<&Placement> {
        self.placements.get(slot as usize)?.as_ref()
    }

    fn place(&mut self, p: Placement) -> u8 {
        let slot = self.next_slot;
        self.next_slot = (self.next_slot + 1) % SLOTS;
        self.placements[slot] = Some(p);
        slot as u8
    }

    fn store(&mut self, id: u32, image: Arc<Image>) {
        self.by_id.insert(id, image);
        // Keep memory bounded: drop images nothing shows, oldest first.
        let total = |m: &HashMap<u32, Arc<Image>>| m.values().map(|i| i.rgba.len()).sum::<usize>();
        while total(&self.by_id) > MAX_STORE {
            let shown: Vec<u64> = self
                .placements
                .iter()
                .flatten()
                .map(|p| p.image.key)
                .collect();
            let victim = self
                .by_id
                .iter()
                .filter(|(_, i)| !shown.contains(&i.key))
                .min_by_key(|(_, i)| i.key)
                .map(|(id, _)| *id);
            match victim {
                Some(id) => {
                    self.by_id.remove(&id);
                }
                None => break,
            }
        }
    }

    fn delete(&mut self, what: u8, id: u32) {
        match what {
            b'i' | b'I' => {
                for p in self.placements.iter_mut() {
                    if p.as_ref().is_some_and(|p| p.image_id == id) {
                        *p = None;
                    }
                }
                if what == b'I' {
                    self.by_id.remove(&id);
                }
            }
            _ => {
                // Everything else (all, at cursor, in a cell range...) clears
                // all placements; uppercase also frees the image data.
                self.placements.iter_mut().for_each(|p| *p = None);
                if what.is_ascii_uppercase() {
                    self.by_id.clear();
                }
            }
        }
    }
}

/// Writes a reply back to the program (the PTY, or the daemon).
pub type Responder = Box<dyn FnMut(&[u8]) + Send>;

#[derive(Clone, Debug, Default)]
struct Control {
    action: u8,
    format: u32,
    medium: u8,
    width: u32,
    height: u32,
    compressed: bool,
    id: u32,
    number: u32,
    placement: u32,
    cols: u32,
    rows: u32,
    stay: bool,
    quiet: u8,
    more: bool,
    delete: u8,
    virtual_place: bool,
}

fn parse_control(text: &str) -> Control {
    let mut c = Control {
        action: b't',
        format: 32,
        medium: b'd',
        delete: b'a',
        ..Control::default()
    };
    for kv in text.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let num = || v.parse::<u32>().unwrap_or(0);
        let ch = || v.bytes().next().unwrap_or(0);
        match k {
            "a" => c.action = ch(),
            "f" => c.format = num(),
            "t" => c.medium = ch(),
            "s" => c.width = num(),
            "v" => c.height = num(),
            "o" => c.compressed = v == "z",
            "i" => c.id = num(),
            "I" => c.number = num(),
            "p" => c.placement = num(),
            "c" => c.cols = num(),
            "r" => c.rows = num(),
            "C" => c.stay = v == "1",
            "q" => c.quiet = num().min(2) as u8,
            "m" => c.more = v == "1",
            "d" => c.delete = ch(),
            "U" => c.virtual_place = v == "1",
            _ => {}
        }
    }
    c
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Esc,
    /// Saw `ESC [`: a DA1 request, or any other CSI (passed through).
    Csi,
    /// Saw `ESC _`: graphics if the next byte is `G`.
    ApcStart,
    Collect,
    CollectEsc,
}

pub struct GraphicsTap {
    state: State,
    apc: Vec<u8>,
    csi: Vec<u8>,
    /// None: pass the graphics protocol through untouched (the daemon,
    /// whose windows decode it).
    images: Option<Arc<Mutex<Images>>>,
    /// Answer DA1 ourselves (see the module notes). Only where the real
    /// terminal state lives -- the window for local panes, the daemon for
    /// its panes -- so a request is answered once.
    answer_da1: bool,
    respond: Option<Responder>,
    /// A chunked transfer in progress (m=1): its first chunk's keys and
    /// the base64 so far.
    pending: Option<(Control, Vec<u8>)>,
    /// Ids handed out for `I=` (image number) transmissions.
    next_auto_id: u32,
}

impl GraphicsTap {
    pub fn new(
        images: Option<Arc<Mutex<Images>>>,
        answer_da1: bool,
        respond: Option<Responder>,
    ) -> Self {
        Self {
            state: State::Ground,
            apc: Vec::new(),
            csi: Vec::new(),
            images,
            answer_da1,
            respond,
            pending: None,
            next_auto_id: 1 << 30,
        }
    }

    /// Filters `input` into `out` (everything but graphics APCs, which
    /// become markers).
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        // Fast path: nothing that could start an APC.
        if self.state == State::Ground && !input.contains(&0x1b) {
            out.extend_from_slice(input);
            return;
        }
        for &b in input {
            self.step(b, out);
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => {
                if b == 0x1b {
                    self.state = State::Esc;
                } else {
                    out.push(b);
                }
            }
            State::Esc => {
                if b == b'_' && self.images.is_some() {
                    self.state = State::ApcStart;
                } else if b == b'[' && self.answer_da1 {
                    self.csi.clear();
                    self.state = State::Csi;
                } else {
                    out.push(0x1b);
                    self.state = State::Ground;
                    self.step(b, out);
                }
            }
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    let params = std::mem::take(&mut self.csi);
                    self.state = State::Ground;
                    if b == b'c' && (params.is_empty() || params == b"0") {
                        if let Some(respond) = &mut self.respond {
                            respond(b"\x1b[?62;22c");
                        }
                    } else {
                        out.extend_from_slice(b"\x1b[");
                        out.extend_from_slice(&params);
                        out.push(b);
                    }
                } else if self.csi.len() < 32 && (0x20..=0x3f).contains(&b) {
                    self.csi.push(b);
                } else {
                    // Not a well-formed CSI: hand everything on as is.
                    out.extend_from_slice(b"\x1b[");
                    out.append(&mut self.csi);
                    self.state = State::Ground;
                    self.step(b, out);
                }
            }
            State::ApcStart => {
                if b == b'G' {
                    self.apc.clear();
                    self.state = State::Collect;
                } else {
                    // Some other APC: pass it on (the parser ignores it).
                    out.extend_from_slice(b"\x1b_");
                    self.state = State::Ground;
                    self.step(b, out);
                }
            }
            State::Collect => match b {
                0x1b => self.state = State::CollectEsc,
                _ if self.apc.len() >= MAX_APC => {
                    // Absurdly large: drop it.
                    self.apc = Vec::new();
                    self.state = State::Ground;
                }
                _ => self.apc.push(b),
            },
            State::CollectEsc => {
                let apc = std::mem::take(&mut self.apc);
                self.state = State::Ground;
                self.handle(&apc, out);
                if b != b'\\' {
                    self.step(0x1b, out);
                    self.step(b, out);
                }
            }
        }
    }

    fn store(&self) -> parking_lot::MutexGuard<'_, Images> {
        self.images
            .as_ref()
            .expect("only collecting with a store")
            .lock()
    }

    fn handle(&mut self, apc: &[u8], out: &mut Vec<u8>) {
        let split = apc.iter().position(|&b| b == b';').unwrap_or(apc.len());
        let control = parse_control(&String::from_utf8_lossy(&apc[..split]));
        let payload = apc.get(split + 1..).unwrap_or_default();

        if let Some((first, mut data)) = self.pending.take() {
            data.extend_from_slice(payload);
            if control.more {
                self.pending = Some((first, data));
            } else {
                let mut first = first;
                first.quiet = first.quiet.max(control.quiet);
                self.finish(first, &data, out);
            }
            return;
        }
        if control.more {
            self.pending = Some((control, payload.to_vec()));
        } else {
            self.finish(control, payload, out);
        }
    }

    fn finish(&mut self, mut c: Control, data: &[u8], out: &mut Vec<u8>) {
        match c.action {
            b'q' => {
                let result = load(&c, data).map(|_| ());
                self.reply(&c, result);
            }
            b't' | b'T' => {
                let image = match load(&c, data) {
                    Ok(img) => Arc::new(img),
                    Err(e) => return self.reply(&c, Err(e)),
                };
                if c.id == 0 && c.number != 0 {
                    c.id = self.next_auto_id;
                    self.next_auto_id += 1;
                }
                self.store().store(c.id, image.clone());
                if c.action == b'T' && !c.virtual_place {
                    self.place(&c, image, out);
                }
                self.reply(&c, Ok(()));
            }
            b'p' => {
                let image = self.store().by_id.get(&c.id).cloned();
                match image {
                    Some(image) => {
                        if !c.virtual_place {
                            self.place(&c, image, out);
                        }
                        self.reply(&c, Ok(()));
                    }
                    None => self.reply(&c, Err("ENOENT:no such image".into())),
                }
            }
            b'd' => self.store().delete(c.delete, c.id),
            _ => {}
        }
    }

    /// Writes the markers for `image` at the cursor (see the module notes).
    fn place(&mut self, c: &Control, image: Arc<Image>, out: &mut Vec<u8>) {
        let (cw, ch) = self.store().cell;
        let (w, h) = (image.width as f32, image.height as f32);
        let (dw, dh) = match (c.cols, c.rows) {
            (0, 0) => (w, h),
            (cols, 0) => (cols as f32 * cw, h * cols as f32 * cw / w),
            (0, rows) => (w * rows as f32 * ch / h, rows as f32 * ch),
            (cols, rows) => (cols as f32 * cw, rows as f32 * ch),
        };
        let cols = ((dw / cw).ceil() as u16).clamp(1, MAX_COLS);
        let rows = ((dh / ch).ceil() as u16).clamp(1, MAX_ROWS);
        let placement = Placement {
            image,
            image_id: c.id,
            cols,
            rows,
            fx: (dw / (cols as f32 * cw)).clamp(0.01, 1.0),
            fy: (dh / (rows as f32 * ch)).clamp(0.01, 1.0),
        };
        let slot = self.store().place(placement);

        if c.stay {
            out.extend_from_slice(b"\x1b7");
        }
        let mut utf8 = [0u8; 4];
        for row in 0..rows {
            out.extend_from_slice(marker(slot, row).encode_utf8(&mut utf8).as_bytes());
            if row + 1 < rows {
                // Down a line (scrolling at the bottom), back to the column.
                out.extend_from_slice(b"\n\x1b[D");
            }
        }
        // Like kitty: the cursor ends after the image's last row.
        if cols > 1 {
            out.extend_from_slice(format!("\x1b[{}C", cols - 1).as_bytes());
        }
        if c.stay {
            out.extend_from_slice(b"\x1b8");
        }
    }

    fn reply(&mut self, c: &Control, result: Result<(), String>) {
        if c.id == 0 && c.number == 0 {
            return;
        }
        let message = match &result {
            Ok(()) if c.quiet >= 1 => return,
            Err(_) if c.quiet >= 2 => return,
            Ok(()) => "OK".to_string(),
            Err(e) => e.clone(),
        };
        let mut keys = format!("i={}", c.id);
        if c.number != 0 {
            keys.push_str(&format!(",I={}", c.number));
        }
        if c.placement != 0 {
            keys.push_str(&format!(",p={}", c.placement));
        }
        let reply = format!("\x1b_G{keys};{message}\x1b\\");
        if let Some(respond) = &mut self.respond {
            respond(reply.as_bytes());
        }
    }
}

/// Decodes a transmission into an image.
fn load(c: &Control, data: &[u8]) -> Result<Image, String> {
    let b64 = |d: &[u8]| {
        base64::engine::general_purpose::STANDARD
            .decode(
                d.iter()
                    .copied()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| "EINVAL:bad base64".to_string())
    };
    let mut bytes = match c.medium {
        b'd' => b64(data)?,
        b'f' | b't' => {
            let path = String::from_utf8(b64(data)?).map_err(|_| "EINVAL:bad path")?;
            read_file(&path, c.medium == b't')?
        }
        _ => return Err("EINVAL:transmission medium not supported".into()),
    };
    if c.compressed {
        bytes = miniz_oxide::inflate::decompress_to_vec_zlib(&bytes)
            .map_err(|_| "EINVAL:bad zlib data".to_string())?;
    }
    let (width, height, rgba) = match c.format {
        100 => decode_png(&bytes)?,
        24 | 32 => {
            let (w, h) = (c.width, c.height);
            if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
                return Err("EINVAL:bad size".into());
            }
            let px = (w * h) as usize;
            let bpp = if c.format == 24 { 3 } else { 4 };
            if bytes.len() < px * bpp {
                return Err("ENODATA:not enough pixel data".into());
            }
            let rgba = if bpp == 4 {
                bytes.truncate(px * 4);
                bytes
            } else {
                bytes[..px * 3]
                    .chunks_exact(3)
                    .flat_map(|p| [p[0], p[1], p[2], 255])
                    .collect()
            };
            (w, h, rgba)
        }
        _ => return Err("EINVAL:unsupported format".into()),
    };
    Ok(Image {
        key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
        width,
        height,
        rgba,
    })
}

fn read_file(path: &str, temporary: bool) -> Result<Vec<u8>, String> {
    let p = std::path::Path::new(path);
    let bad = ["/proc/", "/sys/", "/dev/"];
    let shm = path.starts_with("/dev/shm/");
    if !p.is_absolute() || (!shm && bad.iter().any(|b| path.starts_with(b))) {
        return Err("EPERM:path not allowed".into());
    }
    let meta = std::fs::metadata(p).map_err(|_| "ENOENT:no such file".to_string())?;
    if !meta.is_file() || meta.len() > MAX_APC as u64 {
        return Err("EINVAL:not a regular file of a sane size".into());
    }
    let bytes = std::fs::read(p).map_err(|e| format!("EIO:{e}"))?;
    // Temporary files are removed once read, but only ones that are
    // clearly meant for this (as kitty requires).
    if temporary && path.contains("tty-graphics-protocol") {
        let tmp = std::env::temp_dir();
        if p.starts_with(&tmp) || path.starts_with("/tmp/") || path.starts_with("/dev/shm/") {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(bytes)
}

fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let err = |_| "EINVAL:bad PNG".to_string();
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(err)?;
    let (w, h) = reader.info().size();
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err("EINVAL:image too large".into());
    }
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("EINVAL:image too large")?
    ];
    let info = reader.next_frame(&mut buf).map_err(err)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("EINVAL:palette PNG not expanded".into()),
    };
    Ok((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer
                .write_image_data(&vec![200; (w * h * 3) as usize])
                .unwrap();
        }
        out
    }

    fn b64(d: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(d)
    }

    type Replies = Arc<Mutex<Vec<u8>>>;

    fn tap() -> (GraphicsTap, Arc<Mutex<Images>>, Replies) {
        let images = Arc::new(Mutex::new(Images::default()));
        images.lock().cell = (10.0, 20.0);
        let replies = Arc::new(Mutex::new(Vec::new()));
        let r = replies.clone();
        let tap = GraphicsTap::new(
            Some(images.clone()),
            true,
            Some(Box::new(move |b| r.lock().extend_from_slice(b))),
        );
        (tap, images, replies)
    }

    fn markers(out: &[u8]) -> Vec<(u8, u16)> {
        String::from_utf8_lossy(out)
            .chars()
            .filter_map(decode_marker)
            .collect()
    }

    #[test]
    fn markers_round_trip_and_ignore_other_chars() {
        assert_eq!(decode_marker(marker(7, 3)), Some((7, 3)));
        assert_eq!(decode_marker(marker(254, 255)), Some((254, 255)));
        assert_eq!(decode_marker('a'), None);
        assert_eq!(decode_marker('\u{10FFFD}'), None);
    }

    #[test]
    fn transmit_and_display_png_writes_one_marker_per_row() {
        let (mut t, images, replies) = tap();
        let apc = format!(
            "text\x1b_Ga=T,f=100,i=5;{}\x1b\\after",
            b64(&png_bytes(25, 50))
        );
        let mut out = Vec::new();
        // Split mid-sequence, as reads do.
        let (a, b) = apc.as_bytes().split_at(20);
        t.feed(a, &mut out);
        t.feed(b, &mut out);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with("text") && text.ends_with("after"),
            "{text:?}"
        );
        assert!(!text.contains("_G"));
        // 25x50 px at 10x20 cells: 3 cols, 3 rows (the last partly filled).
        assert_eq!(markers(&out), vec![(0, 0), (0, 1), (0, 2)]);
        assert!(text.contains("\x1b[2C"));
        let p = images.lock().placement(0).cloned().unwrap();
        assert_eq!((p.cols, p.rows), (3, 3));
        assert!((p.fx - 25.0 / 30.0).abs() < 0.01 && (p.fy - 50.0 / 60.0).abs() < 0.01);
        assert_eq!(
            String::from_utf8_lossy(&replies.lock()),
            "\x1b_Gi=5;OK\x1b\\"
        );
    }

    #[test]
    fn chunked_raw_rgba_and_explicit_cells() {
        let (mut t, images, _) = tap();
        let data = b64(&[255u8; 4 * 4 * 4]);
        let (first, rest) = data.split_at(8);
        let mut out = Vec::new();
        t.feed(
            format!("\x1b_Ga=T,f=32,s=4,v=4,c=2,r=1,m=1;{first}\x1b\\").as_bytes(),
            &mut out,
        );
        assert!(markers(&out).is_empty());
        t.feed(format!("\x1b_Gm=0;{rest}\x1b\\").as_bytes(), &mut out);
        assert_eq!(markers(&out), vec![(0, 0)]);
        let p = images.lock().placement(0).cloned().unwrap();
        assert_eq!((p.cols, p.rows, p.fx, p.fy), (2, 1, 1.0, 1.0));
    }

    #[test]
    fn queries_and_errors_get_replies_and_quiet_suppresses_them() {
        let (mut t, _, replies) = tap();
        let mut out = Vec::new();
        t.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", &mut out);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&replies.lock()),
            "\x1b_Gi=31;OK\x1b\\"
        );
        replies.lock().clear();
        t.feed(b"\x1b_Ga=p,i=99\x1b\\", &mut out);
        assert!(String::from_utf8_lossy(&replies.lock()).contains("ENOENT"));
        replies.lock().clear();
        t.feed(b"\x1b_Ga=p,i=99,q=2\x1b\\", &mut out);
        assert!(replies.lock().is_empty());
        t.feed(b"\x1b_Ga=t,t=s,i=3;AAAA\x1b\\", &mut out);
        assert!(String::from_utf8_lossy(&replies.lock()).contains("EINVAL"));
    }

    #[test]
    fn put_again_delete_and_pass_through() {
        let (mut t, images, _) = tap();
        let mut out = Vec::new();
        t.feed(
            format!("\x1b_Ga=t,f=100,i=7,q=1;{}\x1b\\", b64(&png_bytes(10, 20))).as_bytes(),
            &mut out,
        );
        assert!(markers(&out).is_empty(), "transmit only");
        t.feed(b"\x1b_Ga=p,i=7,C=1\x1b\\", &mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.starts_with("\x1b7") && text.ends_with("\x1b8"));
        assert!(images.lock().placement(0).is_some());
        t.feed(b"\x1b_Ga=d,d=i,i=7\x1b\\", &mut out);
        assert!(images.lock().placement(0).is_none());
        // DA1 is answered here, DA2 and other CSIs go through.
        let replies = {
            let (mut t, _, replies) = tap();
            let mut out = Vec::new();
            t.feed(b"a\x1b[cb\x1b[0c\x1b[>c\x1b[31m", &mut out);
            assert_eq!(out, b"ab\x1b[>c\x1b[31m".to_vec());
            replies
        };
        assert_eq!(&*replies.lock(), b"\x1b[?62;22c\x1b[?62;22c");
        // Other escapes go through untouched.
        let mut plain = Vec::new();
        t.feed(b"\x1b[31mred\x1b_Xother\x1b\\\x1b]0;t\x07", &mut plain);
        assert_eq!(plain, b"\x1b[31mred\x1b_Xother\x1b\\\x1b]0;t\x07".to_vec());
    }

    #[test]
    fn file_transmission_reads_and_removes_temp_files() {
        let (mut t, images, _) = tap();
        let path = std::env::temp_dir().join(format!(
            "tty-graphics-protocol-test-{}.png",
            std::process::id()
        ));
        std::fs::write(&path, png_bytes(4, 4)).unwrap();
        let mut out = Vec::new();
        let apc = format!(
            "\x1b_Ga=T,f=100,t=t,i=1;{}\x1b\\",
            b64(path.to_str().unwrap().as_bytes())
        );
        t.feed(apc.as_bytes(), &mut out);
        assert_eq!(markers(&out).len(), 1);
        assert!(!path.exists(), "temp file removed");
        assert!(images.lock().placement(0).is_some());
        let mut out = Vec::new();
        t.feed(
            format!(
                "\x1b_Ga=T,f=100,t=f,i=2;{}\x1b\\",
                b64(b"/proc/self/environ")
            )
            .as_bytes(),
            &mut out,
        );
        assert!(markers(&out).is_empty());
    }
}
