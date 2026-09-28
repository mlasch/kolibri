//! Where the hummingbird goes: straight-line glides, the wingbeat cycle, and
//! the random choices behind its excursions around the screen.

use embedded_graphics::prelude::Point;

/// One wingbeat: (index into `logo::*_WINGS`, body offset in large-bird pixels).
/// The body rises on the downstroke.
const FLAP: [(usize, i32); 4] = [(0, 0), (1, -1), (2, -2), (1, -1)];

/// Waypoints are picked in this range of top-left bird positions. It extends
/// past every panel edge, so an excursion sometimes leaves the screen.
const REACH_X: (i32, i32) = (-48, 136);
const REACH_Y: (i32, i32) = (-12, 38);

/// Which way the bird is looking. The artwork faces left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facing {
    /// Toward x = 0, the resting direction.
    Left,
    /// Toward the right edge.
    Right,
}

impl Facing {
    /// The direction of travel from `from` to `to`, or `None` for a vertical move.
    #[must_use]
    pub fn toward(from: Point, to: Point) -> Option<Self> {
        match to.x.cmp(&from.x) {
            core::cmp::Ordering::Less => Some(Self::Left),
            core::cmp::Ordering::Greater => Some(Self::Right),
            core::cmp::Ordering::Equal => None,
        }
    }
}

/// Points from `from` (exclusive) to `to` (inclusive) along a straight line,
/// no more than `step` pixels apart on either axis.
pub fn glide(from: Point, to: Point, step: i32) -> impl Iterator<Item = Point> {
    let delta = to - from;
    let longest = delta.x.abs().max(delta.y.abs());
    let frames = (longest + step - 1) / step;
    (1..=frames).map(move |i| from + delta * i / frames)
}

/// The wing pose and body position for animation frame `n`, with the bob
/// divided by `bob_scale` for smaller birds.
#[must_use]
pub fn wingbeat(n: usize, at: Point, bob_scale: i32) -> (usize, Point) {
    let (pose, bob) = FLAP[n % FLAP.len()];
    (pose, at + Point::new(0, bob / bob_scale))
}

/// A small xorshift32 generator: plenty for picking flight paths, and no
/// dependency on a HAL's RNG beyond the seed.
pub struct Rng(u32);

impl Rng {
    /// Seeds the generator; a zero seed (which xorshift cannot leave) is replaced.
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    /// The next raw value.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// A value in `0..n`, or 0 if `n` is 0.
    pub fn below(&mut self, n: u32) -> u32 {
        self.next_u32().checked_rem(n).unwrap_or(0)
    }

    /// A value in `lo..=hi`.
    pub fn between(&mut self, lo: i32, hi: i32) -> i32 {
        let offset = self.below(hi.abs_diff(lo).saturating_add(1));
        lo.saturating_add_unsigned(offset)
    }

    /// A random point to fly to, possibly off the panel.
    pub fn waypoint(&mut self) -> Point {
        Point::new(
            self.between(REACH_X.0, REACH_X.1),
            self.between(REACH_Y.0, REACH_Y.1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn a_horizontal_glide_lands_exactly_on_target() {
        let xs: Vec<i32> = glide(Point::new(128, 3), Point::new(86, 3), 6)
            .map(|p| p.x)
            .collect();
        assert_eq!(xs, [122, 116, 110, 104, 98, 92, 86]);
    }

    #[test]
    fn a_diagonal_glide_never_jumps_more_than_a_step() {
        let from = Point::new(4, 8);
        let to = Point::new(-64, -11);
        let path: Vec<Point> = glide(from, to, 4).collect();
        assert_eq!(path.last(), Some(&to));
        let mut prev = from;
        for p in path {
            let d = p - prev;
            assert!(d.x.abs() <= 4 && d.y.abs() <= 4, "{prev:?} -> {p:?}");
            prev = p;
        }
    }

    #[test]
    fn a_glide_to_where_it_already_is_is_empty() {
        assert_eq!(glide(Point::new(86, 3), Point::new(86, 3), 4).count(), 0);
    }

    #[test]
    fn a_wingbeat_starts_at_rest_and_cycles() {
        let at = Point::new(10, 20);
        assert_eq!(wingbeat(0, at, 1), (0, at));
        assert_eq!(wingbeat(2, at, 1), (2, Point::new(10, 18)));
        assert_eq!(wingbeat(2, at, 2), (2, Point::new(10, 19)));
        assert_eq!(wingbeat(FLAP.len(), at, 1), wingbeat(0, at, 1));
        assert!(
            FLAP.iter()
                .all(|&(pose, _)| pose < crate::logo::SMALL_WINGS.len())
        );
    }

    #[test]
    fn facing_follows_the_horizontal_direction() {
        let at = Point::new(10, 10);
        assert_eq!(Facing::toward(at, Point::new(0, 0)), Some(Facing::Left));
        assert_eq!(Facing::toward(at, Point::new(20, 0)), Some(Facing::Right));
        assert_eq!(Facing::toward(at, Point::new(10, 30)), None);
    }

    #[test]
    fn random_values_stay_in_range_and_vary() {
        let mut rng = Rng::new(0);
        let mut seen = [false; 3];
        for _ in 0..1000 {
            let v = rng.between(-1, 1);
            assert!((-1..=1).contains(&v));
            seen[usize::try_from(v + 1).unwrap()] = true;
            let p = rng.waypoint();
            assert!((REACH_X.0..=REACH_X.1).contains(&p.x));
            assert!((REACH_Y.0..=REACH_Y.1).contains(&p.y));
        }
        assert_eq!(seen, [true; 3]);
        assert_eq!(rng.below(0), 0);
    }
}
