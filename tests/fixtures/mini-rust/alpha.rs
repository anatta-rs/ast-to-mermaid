//! Golden-corpus module: struct + impl methods, intra-module calls,
//! cross-module calls into `beta`, a literal receiver, and label
//! characters that need escaping (`>=`, `&`).

use crate::beta::entry;

pub struct Point {
    x: f64,
    y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    pub fn dot(&self, other: &Point) -> f64 {
        self.x * other.x + self.y * other.y
    }

    pub fn norm(&self) -> f64 {
        self.dot(self).sqrt()
    }
}

pub fn describe(p: &Point) -> String {
    if p.norm() >= 1.0 {
        "alpha: norm must be < 1 & finite".to_string()
    } else {
        format!("{}", p.dot(p))
    }
}

pub fn alpha_entry(p: &Point) -> f64 {
    let d = describe(p);
    entry(d.len() as f64)
}
