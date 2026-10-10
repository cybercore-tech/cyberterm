// src/app/graphics.rs
//
// Turns the image markers in a pane's frame (src/graphics.rs) into image
// strips for the renderer, and blanks the markers so no glyph is drawn
// for them. A marker whose placement is gone (deleted, or lost with a
// daemon reattach) just stays blank.

use super::*;
use crate::graphics::decode_marker;
use crate::renderer_images::ImageDraw;

impl App {
    pub(super) fn collect_images(
        &self,
        pane: &Pane,
        frame: &mut Frame,
        rect: Rect,
        cell: (f32, f32),
        out: &mut Vec<ImageDraw>,
    ) {
        let (cw, ch) = cell;
        let images = pane.session.images.lock();
        let right = rect.x + rect.w;
        let bottom = rect.y + rect.h;
        for row in 0..frame.rows {
            for col in 0..frame.cols {
                let cell = &mut frame.cells[row * frame.cols + col];
                let Some((slot, image_row)) = decode_marker(cell.ch) else {
                    continue;
                };
                cell.ch = ' ';
                cell.zerowidth = None;
                let Some(p) = images.placement(slot) else {
                    continue;
                };
                // The image's size in cells, and this row's slice of it.
                let w_cells = p.cols as f32 * p.fx;
                let h_cells = p.rows as f32 * p.fy;
                let y0 = image_row as f32;
                if y0 >= h_cells {
                    continue;
                }
                let y1 = (y0 + 1.0).min(h_cells);
                let mut dst = Rect {
                    x: rect.x + col as f32 * cw,
                    y: rect.y + row as f32 * ch,
                    w: w_cells * cw,
                    h: (y1 - y0) * ch,
                };
                let mut uv = [0.0, y0 / h_cells, 1.0, y1 / h_cells];
                // Clip to the pane.
                if dst.x + dst.w > right {
                    let keep = ((right - dst.x) / dst.w).clamp(0.0, 1.0);
                    dst.w *= keep;
                    uv[2] = keep;
                }
                if dst.y + dst.h > bottom {
                    let keep = ((bottom - dst.y) / dst.h).clamp(0.0, 1.0);
                    uv[3] = uv[1] + (uv[3] - uv[1]) * keep;
                    dst.h *= keep;
                }
                if dst.w > 0.5 && dst.h > 0.5 {
                    out.push(ImageDraw {
                        image: p.image.clone(),
                        dst,
                        uv,
                    });
                }
            }
        }
    }
}
