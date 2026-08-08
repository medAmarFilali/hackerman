//! Points and rectangles on the shared layout canvas.
//!
//! Canvas coordinates are in layout units (logical pixels by default) and use
//! floating point so that converting between monitors with different scale
//! factors never accumulates rounding error. Like every desktop OS, `y` grows
//! downward.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle whose top-left corner is at (`x`, `y`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// Whether `point` lies inside the rectangle. The left and top edges are
    /// inside, the right and bottom edges are not, so two monitors that touch
    /// never both claim the same point.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges() {
        let r = Rect::new(-1920.0, 100.0, 1920.0, 1080.0);
        assert_eq!(r.right(), 0.0);
        assert_eq!(r.bottom(), 1180.0);
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(99.5, 49.5)));
        assert!(!r.contains(Point::new(100.0, 10.0)));
        assert!(!r.contains(Point::new(10.0, 50.0)));
        assert!(!r.contains(Point::new(-0.1, 10.0)));
    }

    #[test]
    fn touching_rects_never_share_a_point() {
        let left = Rect::new(0.0, 0.0, 100.0, 100.0);
        let right = Rect::new(100.0, 0.0, 100.0, 100.0);
        let seam = Point::new(100.0, 50.0);
        assert!(!left.contains(seam));
        assert!(right.contains(seam));
    }
}
