use num::Float;
use std::cmp::{max, min};

pub type Coord = f64;

#[derive(PartialEq, Debug)]
pub struct Point {
    pub x: Coord,
    pub y: Coord,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Orientation {
    ClockWise,
    CounterClockWise,
    Colinear,
}

impl Point {
    pub fn orientation(p: &Point, q: &Point, r: &Point) -> Orientation {
        let val = (q.y - p.y) * (r.x - q.x) - (q.x - p.x) * (r.y - q.y);

        if val.abs() < 1e-7 {
            Orientation::Colinear
        } else if val > 0. {
            Orientation::ClockWise
        } else {
            Orientation::CounterClockWise
        }
    }
}

#[derive(PartialEq, Debug)]
pub struct Line {
    pub from: Point,
    pub to: Point,
}

impl Line {
    fn is_point_on_line_if_colinear(&self, point: &Point) -> bool {
        let from = &self.from;
        let to = &self.to;

        point.x >= Float::min(from.x, to.x)
            && point.x <= Float::max(from.x, to.x)
            && point.y >= Float::min(from.y, to.y)
            && point.y <= Float::max(from.y, to.y)
    }

    pub fn intersect(&self, other: &Self) -> bool {
        let o1 = Point::orientation(&self.from, &self.to, &other.from);
        let o2 = Point::orientation(&self.from, &self.to, &other.to);
        let o3 = Point::orientation(&other.from, &other.to, &self.from);
        let o4 = Point::orientation(&other.from, &other.to, &self.to);
        if o1 != o2 && o3 != o4 {
            return true;
        }

        if o1 == Orientation::Colinear && self.is_point_on_line_if_colinear(&other.from) {
            return true;
        }
        if o2 == Orientation::Colinear && self.is_point_on_line_if_colinear(&other.to) {
            return true;
        }
        if o3 == Orientation::Colinear && other.is_point_on_line_if_colinear(&self.from) {
            return true;
        }
        if o4 == Orientation::Colinear && other.is_point_on_line_if_colinear(&self.to) {
            return true;
        }

        false
    }

    pub fn connected_to(&self, other: &Self) -> bool {
        self.from == other.from
            || self.from == other.to
            || self.to == other.from
            || self.to == other.to
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn orient() {
        let a = Point { x: 0., y: 0. };
        let b = Point { x: 1., y: 0. };
        let c = Point { x: 0., y: 1. };
        assert_eq!(
            Point::orientation(&a, &b, &c),
            Orientation::CounterClockWise
        );
        let a = Point { x: 0., y: 0. };
        let b = Point { x: 0., y: 1. };
        let c = Point { x: 1., y: 0. };
        assert_eq!(Point::orientation(&a, &b, &c), Orientation::ClockWise);
        let a = Point { x: 0., y: 0. };
        let b = Point { x: 1., y: 1. };
        let c = Point { x: 4., y: 4. };
        assert_eq!(Point::orientation(&a, &b, &c), Orientation::Colinear);
    }

    #[test]
    fn intersect() {
        let a = Line {
            from: Point { x: 0., y: 0. },
            to: Point { x: 1., y: 0. },
        };
        let b = Line {
            from: Point { x: 1., y: 1. },
            to: Point { x: 1., y: -1. },
        };
        assert!(a.intersect(&b));

        let a = Line {
            from: Point { x: 0., y: 0. },
            to: Point { x: 1., y: 0. },
        };
        let b = Line {
            from: Point { x: 2., y: 1. },
            to: Point { x: 1., y: -1. },
        };
        assert!(!a.intersect(&b));

        let a = Line {
            from: Point { x: 0., y: 0. },
            to: Point { x: 1., y: 1. },
        };
        let b = Line {
            from: Point { x: 0., y: 1. },
            to: Point { x: 1., y: 0. },
        };
        assert!(a.intersect(&b));

        let a = Line {
            from: Point { x: 0., y: 0. },
            to: Point { x: 1., y: 1. },
        };
        let b = Line {
            from: Point { x: 1., y: 1. },
            to: Point { x: 2., y: 2. },
        };
        assert!(a.intersect(&b));

        let a = Line {
            from: Point { x: 0., y: 0. },
            to: Point { x: 1., y: 1. },
        };
        let b = Line {
            from: Point { x: 2., y: 2. },
            to: Point { x: 3., y: 3. },
        };
        assert!(!a.intersect(&b));
    }
}
