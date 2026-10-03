#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

impl Rect {
    fn fits_horizontally(self, bounds: Rect) -> bool {
        self.x >= bounds.x && self.x + self.width <= bounds.x + bounds.width
    }

    fn fits_vertically(self, bounds: Rect) -> bool {
        self.y >= bounds.y && self.y + self.height <= bounds.y + bounds.height
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Side {
    Start,
    #[default]
    Center,
    End,
}

impl Side {
    fn flipped(self) -> Self {
        match self {
            Self::Start => Self::End,
            Self::Center => Self::Center,
            Self::End => Self::Start,
        }
    }

    fn point(self, start: i32, length: i32) -> i32 {
        match self {
            Self::Start => start,
            Self::Center => start + length / 2,
            Self::End => start + length,
        }
    }

    fn origin(self, point: i32, length: i32) -> i32 {
        match self {
            Self::Start => point - length,
            Self::Center => point - length / 2,
            Self::End => point,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Edge {
    horizontal: Side,
    vertical: Side,
}

impl Edge {
    pub(super) fn from_wire(value: u32) -> Option<Self> {
        let (horizontal, vertical) = match value {
            0 => (Side::Center, Side::Center),
            1 => (Side::Center, Side::Start),
            2 => (Side::Center, Side::End),
            3 => (Side::Start, Side::Center),
            4 => (Side::End, Side::Center),
            5 => (Side::Start, Side::Start),
            6 => (Side::Start, Side::End),
            7 => (Side::End, Side::Start),
            8 => (Side::End, Side::End),
            _ => return None,
        };
        Some(Self {
            horizontal,
            vertical,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Axes {
    x: bool,
    y: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Adjustment {
    slide: Axes,
    flip: Axes,
    resize: Axes,
}

impl Adjustment {
    pub(super) fn from_wire(bits: u32) -> Self {
        let set = |bit: u32| bits & bit != 0;
        Self {
            slide: Axes {
                x: set(1),
                y: set(2),
            },
            flip: Axes {
                x: set(4),
                y: set(8),
            },
            resize: Axes {
                x: set(16),
                y: set(32),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Positioner {
    pub(super) size: (i32, i32),
    pub(super) anchor_rect: Rect,
    pub(super) anchor: Edge,
    pub(super) gravity: Edge,
    pub(super) adjustment: Adjustment,
    pub(super) offset: (i32, i32),
}

impl Positioner {
    fn geometry(&self, anchor: Edge, gravity: Edge, offset: (i32, i32)) -> Rect {
        let area = self.anchor_rect;
        let (width, height) = self.size;
        let x = anchor.horizontal.point(area.x, area.width);
        let y = anchor.vertical.point(area.y, area.height);
        Rect {
            x: gravity.horizontal.origin(x, width) + offset.0,
            y: gravity.vertical.origin(y, height) + offset.1,
            width,
            height,
        }
    }

    pub(super) fn place(&self, bounds: Rect) -> Rect {
        let mut placed = self.geometry(self.anchor, self.gravity, self.offset);
        if !placed.fits_horizontally(bounds) && self.adjustment.flip.x {
            let flip = |edge: Edge| Edge {
                horizontal: edge.horizontal.flipped(),
                ..edge
            };
            let flipped = self.geometry(
                flip(self.anchor),
                flip(self.gravity),
                (-self.offset.0, self.offset.1),
            );
            if flipped.fits_horizontally(bounds) {
                placed.x = flipped.x;
            }
        }
        if !placed.fits_vertically(bounds) && self.adjustment.flip.y {
            let flip = |edge: Edge| Edge {
                vertical: edge.vertical.flipped(),
                ..edge
            };
            let flipped = self.geometry(
                flip(self.anchor),
                flip(self.gravity),
                (self.offset.0, -self.offset.1),
            );
            if flipped.fits_vertically(bounds) {
                placed.y = flipped.y;
            }
        }
        if !placed.fits_horizontally(bounds) && self.adjustment.slide.x {
            placed.x = placed
                .x
                .min(bounds.x + bounds.width - placed.width)
                .max(bounds.x);
        }
        if !placed.fits_vertically(bounds) && self.adjustment.slide.y {
            placed.y = placed
                .y
                .min(bounds.y + bounds.height - placed.height)
                .max(bounds.y);
        }
        if !placed.fits_horizontally(bounds) && self.adjustment.resize.x {
            let left = placed.x.max(bounds.x);
            let right = (placed.x + placed.width).min(bounds.x + bounds.width);
            placed.x = left;
            placed.width = (right - left).max(1);
        }
        if !placed.fits_vertically(bounds) && self.adjustment.resize.y {
            let top = placed.y.max(bounds.y);
            let bottom = (placed.y + placed.height).min(bounds.y + bounds.height);
            placed.y = top;
            placed.height = (bottom - top).max(1);
        }
        placed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTTOM_LEFT: u32 = 6;
    const BOTTOM_RIGHT: u32 = 8;
    const RIGHT: u32 = 4;
    const SLIDE_X: u32 = 1;
    const SLIDE_Y: u32 = 2;
    const FLIP_X: u32 = 4;
    const FLIP_Y: u32 = 8;
    const RESIZE_X: u32 = 16;
    const RESIZE_Y: u32 = 32;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn select_menu(anchor_rect: Rect, size: (i32, i32), adjustment: u32) -> Positioner {
        Positioner {
            size,
            anchor_rect,
            anchor: Edge::from_wire(BOTTOM_LEFT).expect("anchor"),
            gravity: Edge::from_wire(BOTTOM_RIGHT).expect("gravity"),
            adjustment: Adjustment::from_wire(adjustment),
            offset: (0, 0),
        }
    }

    #[test]
    fn popups_flip_slide_and_shrink_to_stay_inside_the_game_window() {
        let window = rect(0, 0, 800, 500);
        let field = rect(520, 410, 240, 30);
        for (positioner, expected, case) in [
            (
                select_menu(rect(40, 200, 240, 30), (240, 106), FLIP_Y | SLIDE_X),
                rect(40, 230, 240, 106),
                "a menu that fits opens below its field",
            ),
            (
                select_menu(field, (240, 106), FLIP_Y | SLIDE_X),
                rect(520, 304, 240, 106),
                "a menu below the window edge flips above its field",
            ),
            (
                select_menu(field, (240, 106), SLIDE_Y),
                rect(520, 394, 240, 106),
                "without flip it slides up to the edge",
            ),
            (
                select_menu(field, (240, 106), 0),
                rect(520, 440, 240, 106),
                "without adjustments it stays where the rules put it",
            ),
            (
                select_menu(rect(700, 100, 60, 30), (240, 106), SLIDE_X),
                rect(560, 130, 240, 106),
                "a menu past the right edge slides left",
            ),
            (
                select_menu(rect(10, 100, 60, 30), (900, 106), SLIDE_X),
                rect(0, 130, 900, 106),
                "a menu wider than the window aligns with its left edge",
            ),
            (
                select_menu(field, (240, 600), FLIP_Y | RESIZE_Y),
                rect(520, 440, 240, 60),
                "a menu that fits neither way is cut to the space below",
            ),
            (
                select_menu(rect(700, 100, 60, 30), (240, 106), RESIZE_X),
                rect(700, 130, 100, 106),
                "a menu past the right edge shrinks to the window",
            ),
        ] {
            assert_eq!(positioner.place(window), expected, "{case}");
        }
    }

    #[test]
    fn a_flip_mirrors_the_anchor_gravity_and_offset() {
        let submenu = Positioner {
            size: (200, 120),
            anchor_rect: rect(700, 50, 80, 24),
            anchor: Edge::from_wire(RIGHT).expect("anchor"),
            gravity: Edge::from_wire(BOTTOM_RIGHT).expect("gravity"),
            adjustment: Adjustment::from_wire(FLIP_X),
            offset: (4, -2),
        };
        assert_eq!(
            submenu.place(rect(0, 0, 800, 600)),
            rect(496, 60, 200, 120),
            "a submenu that cannot open to the right opens to the left, its offset mirrored"
        );
        assert_eq!(Edge::from_wire(9), None, "anchors stop at bottom_right");
    }
}
