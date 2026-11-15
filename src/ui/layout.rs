//! Geometry shared by painting, hit testing and keyboard navigation (DIPs).

use std::ops::Range;

use crate::scene::Rect;

use super::ViewMode;

pub const TAB_H: f32 = 40.0;
pub const TAB_TOP: f32 = 8.0;
pub const TAB_MAX_W: f32 = 240.0;
pub const TAB_MIN_W: f32 = 80.0;
pub const CAPTION_W: f32 = 46.0;
pub const ADDR_H: f32 = 48.0;
pub const CMD_H: f32 = 48.0;
pub const TOOLBAR_B: f32 = TAB_H + ADDR_H + CMD_H;
pub const STATUS_H: f32 = 24.0;
pub const HEADER_H: f32 = 32.0;
pub const SCROLL_W: f32 = 12.0;
pub const NAV_MIN: f32 = 120.0;
pub const NAV_MAX: f32 = 600.0;
pub const BUTTON: f32 = 32.0;

pub fn row_h(compact: bool) -> f32 {
    if compact {
        22.0
    } else {
        30.0
    }
}

pub fn nav_row_h(compact: bool) -> f32 {
    if compact {
        24.0
    } else {
        30.0
    }
}

/// Item placement for a view mode inside the file area.
#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub mode: ViewMode,
    /// Viewport for items (below the details header, left of the scrollbar).
    pub area: Rect,
    pub cell_w: f32,
    pub cell_h: f32,
    /// Items per row (vertical scrolling) or per column (List view, horizontal scrolling).
    pub per_line: usize,
    pub n: usize,
    pub horizontal: bool,
    pub pad_x: f32,
    pub pad_y: f32,
}

impl Grid {
    pub fn new(mode: ViewMode, area: Rect, n: usize, compact: bool, details_w: f32) -> Grid {
        let rh = row_h(compact);
        let (cell_w, cell_h, pad_x, pad_y) = match mode {
            // Rows end at the last column: the space to the right is background (folder menu, rubber band).
            ViewMode::Details => (details_w, rh, 0.0, 0.0),
            ViewMode::List => (260.0, rh, 8.0, 4.0),
            ViewMode::Small => (230.0, rh, 8.0, 4.0),
            ViewMode::Medium => (92.0, 104.0, 8.0, 6.0),
            ViewMode::Large => (124.0, 152.0, 8.0, 6.0),
            ViewMode::ExtraLarge => (276.0, 310.0, 8.0, 6.0),
            ViewMode::Tiles => (272.0, 68.0, 8.0, 6.0),
        };
        let horizontal = mode == ViewMode::List;
        let per_line = if mode == ViewMode::Details {
            1
        } else if horizontal {
            (((area.h() - pad_y * 2.0) / cell_h).floor() as usize).max(1)
        } else {
            (((area.w() - pad_x * 2.0) / cell_w).floor() as usize).max(1)
        };
        Grid {
            mode,
            area,
            cell_w,
            cell_h,
            per_line,
            n,
            horizontal,
            pad_x,
            pad_y,
        }
    }

    pub fn lines(&self) -> usize {
        self.n.div_ceil(self.per_line)
    }

    /// Cell of item `pos` in content coordinates (scroll 0, relative to the area's origin).
    pub fn cell(&self, pos: usize) -> Rect {
        let (line, within) = (pos / self.per_line, pos % self.per_line);
        if self.horizontal {
            Rect::xywh(
                self.pad_x + line as f32 * self.cell_w,
                self.pad_y + within as f32 * self.cell_h,
                self.cell_w,
                self.cell_h,
            )
        } else {
            Rect::xywh(
                self.pad_x + within as f32 * self.cell_w,
                self.pad_y + line as f32 * self.cell_h,
                self.cell_w,
                self.cell_h,
            )
        }
    }

    /// Cell of item `pos` on screen for a scroll offset.
    pub fn screen(&self, pos: usize, scroll: f32) -> Rect {
        let c = self.cell(pos);
        if self.horizontal {
            c.offset(self.area.l - scroll, self.area.t)
        } else {
            c.offset(self.area.l, self.area.t - scroll)
        }
    }

    pub fn extent(&self) -> f32 {
        let pad = if self.horizontal { self.pad_x } else { self.pad_y };
        let len = if self.horizontal { self.cell_w } else { self.cell_h };
        pad * 2.0 + self.lines() as f32 * len
    }

    pub fn viewport(&self) -> f32 {
        if self.horizontal {
            self.area.w()
        } else {
            self.area.h()
        }
    }

    pub fn max_scroll(&self) -> f32 {
        (self.extent() - self.viewport()).max(0.0)
    }

    pub fn visible(&self, scroll: f32) -> Range<usize> {
        let len = if self.horizontal { self.cell_w } else { self.cell_h };
        let pad = if self.horizontal { self.pad_x } else { self.pad_y };
        let first = (((scroll - pad) / len).floor().max(0.0)) as usize;
        let last = (((scroll + self.viewport() - pad) / len).ceil().max(0.0)) as usize + 1;
        (first * self.per_line).min(self.n)..(last * self.per_line).min(self.n)
    }

    /// Item under a screen point.
    pub fn at(&self, x: f32, y: f32, scroll: f32) -> Option<usize> {
        if !self.area.contains(x, y) {
            return None;
        }
        let (cx, cy) = if self.horizontal {
            (x - self.area.l + scroll - self.pad_x, y - self.area.t - self.pad_y)
        } else {
            (x - self.area.l - self.pad_x, y - self.area.t + scroll - self.pad_y)
        };
        if cx < 0.0 || cy < 0.0 {
            return None;
        }
        let (col, row) = ((cx / self.cell_w) as usize, (cy / self.cell_h) as usize);
        let pos = if self.horizontal {
            if row >= self.per_line {
                return None;
            }
            col * self.per_line + row
        } else {
            if col >= self.per_line {
                return None;
            }
            row * self.per_line + col
        };
        (pos < self.n).then_some(pos)
    }

    /// Items whose cells intersect a content-space rectangle.
    pub fn intersecting(&self, r: Rect) -> Vec<usize> {
        let mut out = Vec::new();
        let (along0, along1) = if self.horizontal { (r.l, r.r) } else { (r.t, r.b) };
        let len = if self.horizontal { self.cell_w } else { self.cell_h };
        let pad = if self.horizontal { self.pad_x } else { self.pad_y };
        let first = (((along0 - pad) / len).floor().max(0.0)) as usize;
        let last = (((along1 - pad) / len).ceil().max(0.0)) as usize;
        for line in first..=last.min(self.lines()) {
            for k in 0..self.per_line {
                let pos = line * self.per_line + k;
                if pos >= self.n {
                    break;
                }
                if self.hit_rect(pos).intersects(&r) {
                    out.push(pos);
                }
            }
        }
        out
    }

    /// The part of a cell that counts as the item (selection/hit), in content coordinates.
    pub fn hit_rect(&self, pos: usize) -> Rect {
        let c = self.cell(pos);
        match self.mode {
            ViewMode::Details => c,
            _ => c.inset(2.0, 1.0),
        }
    }

    /// Keyboard navigation target.
    pub fn step(&self, pos: usize, vk: u32, page_items: usize) -> usize {
        let n = self.n.max(1);
        let pl = self.per_line;
        let p = pos as isize;
        let target = match (vk, self.horizontal) {
            (0x25, false) => p - 1,
            (0x27, false) => p + 1,
            (0x26, false) => p - pl as isize,
            (0x28, false) => p + pl as isize,
            (0x26, true) => p - 1,
            (0x28, true) => p + 1,
            (0x25, true) => p - pl as isize,
            (0x27, true) => p + pl as isize,
            (0x21, _) => p - page_items as isize,
            (0x22, _) => p + page_items as isize,
            (0x24, _) => 0,
            (0x23, _) => n as isize - 1,
            _ => p,
        };
        // Moving down past the last full row lands on the last item, like Explorer.
        target.clamp(0, n as isize - 1) as usize
    }

    pub fn page_items(&self) -> usize {
        let len = if self.horizontal { self.cell_w } else { self.cell_h };
        ((self.viewport() / len).floor() as usize).max(1) * self.per_line
    }

    /// Scroll offset that brings `pos` fully into view.
    pub fn scroll_to(&self, pos: usize, scroll: f32) -> f32 {
        let c = self.cell(pos);
        let (a, b) = if self.horizontal { (c.l, c.r) } else { (c.t, c.b) };
        let vp = self.viewport();
        let s = if a - self.pad_y.min(4.0) < scroll {
            a - if self.horizontal { self.pad_x } else { self.pad_y }
        } else if b > scroll + vp {
            b - vp + if self.horizontal { self.pad_x } else { self.pad_y }
        } else {
            scroll
        };
        s.clamp(0.0, self.max_scroll())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_roundtrip() {
        let area = Rect::new(0.0, 0.0, 500.0, 400.0);
        for mode in ViewMode::ALL {
            let g = Grid::new(mode, area, 1000, false, 600.0);
            for pos in [0usize, 1, 7, 33, 999] {
                let r = g.screen(pos, 0.0);
                let s = g.scroll_to(pos, 0.0);
                let r2 = g.screen(pos, s);
                let (cx, cy) = (r2.cx(), r2.cy());
                if g.area.contains(cx, cy) {
                    assert_eq!(g.at(cx, cy, s), Some(pos), "{mode:?} {pos}");
                }
                let _ = r;
            }
            assert_eq!(g.step(0, 0x24, 10), 0);
            assert_eq!(g.step(5, 0x23, 10), 999);
        }
    }
}
