// src/boxdraw.rs
//
// Box-drawing (U+2500-U+257F) and block-element (U+2580-U+259F) characters
// drawn as rectangles instead of font glyphs. Font glyphs for these rarely
// fill the cell exactly -- with any line height above 1.0 the vertical
// lines of a TUI border come out dashed and corners don't meet. Drawing
// them from the cell geometry makes every border join seamlessly,
// whatever the font or line height (Kitty, WezTerm and Ghostty do the same).

/// A rectangle in pixels plus coverage alpha (block shades are partial).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub alpha: f32,
}

/// Line weights (up, right, down, left) for U+2500..=U+257F: 0 none,
/// 1 light, 2 heavy, 3 double, `....` drawn by the font (diagonals).
/// Dashed variants are drawn solid.
const LINES: &str = "\
0101 0202 1010 2020 0101 0202 1010 2020 0101 0202 1010 2020 0110 0210 0120 0220 \
0011 0012 0021 0022 1100 1200 2100 2200 1001 1002 2001 2002 1110 1210 2110 1120 \
2120 2210 1220 2220 1011 1012 2011 1021 2021 2012 1022 2022 0111 0112 0211 0212 \
0121 0122 0221 0222 1101 1102 1201 1202 2101 2102 2201 2202 1111 1112 1211 1212 \
2111 1121 2121 2112 2211 1122 1221 2212 1222 2122 2221 2222 0101 0202 1010 2020 \
0303 3030 0310 0130 0330 0013 0031 0033 1300 3100 3300 1003 3001 3003 1310 3130 \
3330 1013 3031 3033 0313 0131 0333 1303 3101 3303 1313 3131 3333 0110 0011 1001 \
1100 .... .... .... 0001 1000 0100 0010 0002 2000 0200 0020 0210 1020 0102 2010";

fn line_weights(ch: char) -> Option<[u8; 4]> {
    if !('\u{2500}'..='\u{257F}').contains(&ch) {
        return None;
    }
    let index = (ch as u32 - 0x2500) as usize;
    let entry = LINES.split(' ').nth(index)?;
    if entry.starts_with('.') {
        return None;
    }
    let b = entry.as_bytes();
    Some([b[0] - b'0', b[1] - b'0', b[2] - b'0', b[3] - b'0'])
}

/// Whether `ch` is drawn here rather than by the font.
pub fn is_drawn(ch: char) -> bool {
    matches!(ch as u32, 0x2580..=0x259F) || line_weights(ch).is_some()
}

/// The rectangles for `ch` in the cell at (x, y) sized w x h, or `None`
/// if the font should draw it. `light` is the light line thickness.
pub fn pieces(ch: char, x: f32, y: f32, w: f32, h: f32, light: f32) -> Option<Vec<Piece>> {
    if let Some(weights) = line_weights(ch) {
        return Some(lines(weights, x, y, w, h, light));
    }
    blocks(ch, x, y, w, h)
}

fn lines(weights: [u8; 4], x: f32, y: f32, w: f32, h: f32, light: f32) -> Vec<Piece> {
    let mut out = Vec::new();
    let cx = (x + w / 2.0).round();
    let cy = (y + h / 2.0).round();
    let (x0, x1, y0, y1) = (x.round(), (x + w).round(), y.round(), (y + h).round());
    let mut rect = |ax: f32, ay: f32, bx: f32, by: f32| {
        let (l, r) = (ax.min(bx), ax.max(bx));
        let (t, b) = (ay.min(by), ay.max(by));
        out.push(Piece {
            x: l,
            y: t,
            w: r - l,
            h: b - t,
            alpha: 1.0,
        });
    };
    let [up, right, down, left] = weights;
    // Each half-line runs from the cell edge to just past the centre, so
    // arms of different weights overlap cleanly at the junction.
    let thickness = |wt: u8| if wt == 2 { light * 2.0 } else { light };
    for (dir, wt) in [(0, up), (1, right), (2, down), (3, left)] {
        if wt == 0 {
            continue;
        }
        let t = thickness(wt);
        // Double lines: two light strokes with a light-width gap.
        let offsets: &[f32] = if wt == 3 { &[-1.5, 0.5] } else { &[-0.5] };
        let reach = if wt == 3 { 1.5 * light } else { t / 2.0 };
        for off in offsets {
            let o = (off * t).round();
            match dir {
                0 => rect(cx + o, y0, cx + o + t, cy + reach),
                1 => rect(cx - reach, cy + o, x1, cy + o + t),
                2 => rect(cx + o, cy - reach, cx + o + t, y1),
                _ => rect(x0, cy + o, cx + reach, cy + o + t),
            }
        }
    }
    out
}

fn blocks(ch: char, x: f32, y: f32, w: f32, h: f32) -> Option<Vec<Piece>> {
    // Fractions of the cell: (left, top, width, height).
    let frac = |l: f32, t: f32, fw: f32, fh: f32, alpha: f32| {
        let (ax, ay) = ((x + l * w).round(), (y + t * h).round());
        let (bx, by) = ((x + (l + fw) * w).round(), (y + (t + fh) * h).round());
        Piece {
            x: ax,
            y: ay,
            w: bx - ax,
            h: by - ay,
            alpha,
        }
    };
    let code = ch as u32;
    let quads = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut v = Vec::new();
        for (on, l, t) in [
            (ul, 0.0, 0.0),
            (ur, 0.5, 0.0),
            (ll, 0.0, 0.5),
            (lr, 0.5, 0.5),
        ] {
            if on {
                v.push(frac(l, t, 0.5, 0.5, 1.0));
            }
        }
        v
    };
    Some(match code {
        0x2580 => vec![frac(0.0, 0.0, 1.0, 0.5, 1.0)],
        0x2581..=0x2588 => {
            let n = (code - 0x2580) as f32 / 8.0;
            vec![frac(0.0, 1.0 - n, 1.0, n, 1.0)]
        }
        0x2589..=0x258F => {
            let n = (0x2590 - code) as f32 / 8.0;
            vec![frac(0.0, 0.0, n, 1.0, 1.0)]
        }
        0x2590 => vec![frac(0.5, 0.0, 0.5, 1.0, 1.0)],
        0x2591 => vec![frac(0.0, 0.0, 1.0, 1.0, 0.25)],
        0x2592 => vec![frac(0.0, 0.0, 1.0, 1.0, 0.5)],
        0x2593 => vec![frac(0.0, 0.0, 1.0, 1.0, 0.75)],
        0x2594 => vec![frac(0.0, 0.0, 1.0, 0.125, 1.0)],
        0x2595 => vec![frac(0.875, 0.0, 0.125, 1.0, 1.0)],
        0x2596 => quads(false, false, true, false),
        0x2597 => quads(false, false, false, true),
        0x2598 => quads(true, false, false, false),
        0x2599 => quads(true, false, true, true),
        0x259A => quads(true, false, false, true),
        0x259B => quads(true, true, true, false),
        0x259C => quads(true, true, false, true),
        0x259D => quads(false, true, false, false),
        0x259E => quads(false, true, true, false),
        0x259F => quads(false, true, true, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_the_whole_box_drawing_block() {
        assert_eq!(LINES.split(' ').count(), 0x80);
        assert_eq!(line_weights('─'), Some([0, 1, 0, 1]));
        assert_eq!(line_weights('┼'), Some([1, 1, 1, 1]));
        assert_eq!(line_weights('╋'), Some([2, 2, 2, 2]));
        assert_eq!(line_weights('╔'), Some([0, 3, 3, 0]));
        assert_eq!(line_weights('╬'), Some([3, 3, 3, 3]));
        assert_eq!(line_weights('╭'), Some([0, 1, 1, 0]));
        assert_eq!(line_weights('╿'), Some([2, 0, 1, 0]));
        assert_eq!(line_weights('╱'), None);
        assert!(!is_drawn('╱'));
        assert!(!is_drawn('a'));
        assert!(is_drawn('█'));
    }

    #[test]
    fn horizontal_lines_span_the_full_cell_width() {
        let p = pieces('─', 10.0, 20.0, 8.0, 16.0, 1.0).unwrap();
        let left = p.iter().map(|r| r.x).fold(f32::MAX, f32::min);
        let right = p.iter().map(|r| r.x + r.w).fold(f32::MIN, f32::max);
        assert_eq!((left, right), (10.0, 18.0));
        assert!(p.iter().all(|r| r.h == 1.0));
    }

    #[test]
    fn vertical_lines_span_the_full_cell_height() {
        let p = pieces('│', 0.0, 16.0, 8.0, 16.0, 1.0).unwrap();
        let top = p.iter().map(|r| r.y).fold(f32::MAX, f32::min);
        let bottom = p.iter().map(|r| r.y + r.h).fold(f32::MIN, f32::max);
        assert_eq!((top, bottom), (16.0, 32.0));
    }

    #[test]
    fn double_lines_have_two_strokes() {
        assert_eq!(pieces('═', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap().len(), 4);
    }

    #[test]
    fn blocks_and_shades() {
        let full = pieces('█', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap();
        assert_eq!(
            full,
            vec![Piece {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
                alpha: 1.0
            }]
        );
        let lower = pieces('▄', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap();
        assert_eq!((lower[0].y, lower[0].h), (8.0, 8.0));
        let left = pieces('▌', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap();
        assert_eq!(left[0].w, 4.0);
        assert_eq!(pieces('▒', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap()[0].alpha, 0.5);
        assert_eq!(pieces('▚', 0.0, 0.0, 8.0, 16.0, 1.0).unwrap().len(), 2);
    }
}
