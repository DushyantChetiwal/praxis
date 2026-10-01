use architect::Position;
use gpui::{Hsla, PathBuilder, Pixels, Point, Window, point, px};

/// Node size in canvas units. The layout's spacing is chosen around these, so
/// the two have to agree.
pub(super) const NODE_WIDTH: f32 = 280.0;
pub(super) const NODE_HEIGHT: f32 = 132.0;

/// A step showing its sub-plan inside itself. Wider and taller than a plain
/// step, because it has a row of children to fit.
pub(super) const EXPANDED_NODE_WIDTH: f32 = 392.0;
pub(super) const EXPANDED_NODE_HEIGHT: f32 = 220.0;

/// How close a click has to be to an edge to select it, in screen pixels.
pub(super) const EDGE_HIT_TOLERANCE: f32 = 9.0;

const GRID_SNAP: f32 = 8.0;

pub(super) fn snap(value: f32) -> f32 {
    (value / GRID_SNAP).round() * GRID_SNAP
}

/// A connection between two steps, in canvas space.
///
/// A connection that runs backwards is a loop, and drawing it the same way as a
/// forward one would hide it underneath the steps it passes. Those bow out
/// below the graph instead, where they read as a loop at a glance.
pub(super) struct EdgeCurve {
    start: Position,
    control_a: Position,
    control_b: Position,
    end: Position,
    /// Whether this connection runs back the way the plan came, which is what
    /// makes it a loop.
    pub(super) backwards: bool,
}

impl EdgeCurve {
    pub(super) fn between(from: Position, to: Position) -> Self {
        let backwards = to.x <= from.x + NODE_WIDTH / 2.0;

        if backwards {
            let bow = NODE_HEIGHT * 1.15;
            let start = Position {
                x: from.x,
                y: from.y + NODE_HEIGHT / 2.0,
            };
            let end = Position {
                x: to.x,
                y: to.y + NODE_HEIGHT / 2.0,
            };
            Self {
                start,
                control_a: Position {
                    x: start.x,
                    y: start.y + bow,
                },
                control_b: Position {
                    x: end.x,
                    y: end.y + bow,
                },
                end,
                backwards: true,
            }
        } else {
            let start = Position {
                x: from.x + NODE_WIDTH / 2.0,
                y: from.y,
            };
            let end = Position {
                x: to.x - NODE_WIDTH / 2.0,
                y: to.y,
            };
            let reach = ((end.x - start.x) * 0.5).max(48.0);
            Self {
                start,
                control_a: Position {
                    x: start.x + reach,
                    y: start.y,
                },
                control_b: Position {
                    x: end.x - reach,
                    y: end.y,
                },
                end,
                backwards: false,
            }
        }
    }

    pub(super) fn self_loop(position: Position, (width, height): (f32, f32)) -> Self {
        // Separate bottom ports keep the return arrow visible and prevent the
        // curve from retracing itself. Use the displayed card's extents.
        let start = Position {
            x: position.x + width / 4.0,
            y: position.y + height / 2.0,
        };
        let end = Position {
            x: position.x - width / 4.0,
            y: start.y,
        };
        let bow = NODE_HEIGHT * 1.15;
        Self {
            start,
            control_a: Position {
                x: start.x,
                y: start.y + bow,
            },
            control_b: Position {
                x: end.x,
                y: end.y + bow,
            },
            end,
            backwards: true,
        }
    }

    pub(super) fn at(&self, t: f32) -> Position {
        let inverse = 1.0 - t;
        let a = inverse * inverse * inverse;
        let b = 3.0 * inverse * inverse * t;
        let c = 3.0 * inverse * t * t;
        let d = t * t * t;
        Position {
            x: a * self.start.x + b * self.control_a.x + c * self.control_b.x + d * self.end.x,
            y: a * self.start.y + b * self.control_a.y + c * self.control_b.y + d * self.end.y,
        }
    }

    pub(super) fn midpoint(&self) -> Position {
        self.at(0.5)
    }

    /// Distance from a point to the curve, approximated by sampling. Exact
    /// bezier distance is not worth solving for a click test.
    pub(super) fn distance_to(&self, position: Position) -> f32 {
        const SAMPLES: usize = 32;
        (0..=SAMPLES)
            .map(|step| {
                let sample = self.at(step as f32 / SAMPLES as f32);
                let dx = sample.x - position.x;
                let dy = sample.y - position.y;
                (dx * dx + dy * dy).sqrt()
            })
            .fold(f32::MAX, f32::min)
    }
}

pub(super) fn paint_curve(
    curve: &EdgeCurve,
    to_screen: &impl Fn(Position) -> Point<Pixels>,
    width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let start = to_screen(curve.start);
    let end = to_screen(curve.end);

    let mut builder = PathBuilder::stroke(width);
    builder.move_to(start);
    builder.cubic_bezier_to(end, to_screen(curve.control_a), to_screen(curve.control_b));
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }

    // The arrowhead points along the curve as it arrives, which is what tells
    // you which way a loop runs.
    let approach = to_screen(curve.at(0.94));
    let dx = f32::from(end.x - approach.x);
    let dy = f32::from(end.y - approach.y);
    let length = (dx * dx + dy * dy).sqrt();
    if length < 0.001 {
        return;
    }
    let (ux, uy) = (dx / length, dy / length);
    let size = 8.0;

    let base = point(end.x - px(ux * size), end.y - px(uy * size));
    let mut head = PathBuilder::fill();
    head.move_to(end);
    head.line_to(point(
        base.x - px(uy * size * 0.5),
        base.y + px(ux * size * 0.5),
    ));
    head.line_to(point(
        base.x + px(uy * size * 0.5),
        base.y - px(ux * size * 0.5),
    ));
    head.close();
    if let Ok(path) = head.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_curve_connects_facing_node_edges() {
        let curve =
            EdgeCurve::between(Position { x: 0.0, y: 10.0 }, Position { x: 500.0, y: 30.0 });

        assert!(!curve.backwards);
        assert_eq!(curve.start.x, NODE_WIDTH / 2.0);
        assert_eq!(curve.start.y, 10.0);
        assert_eq!(curve.end.x, 500.0 - NODE_WIDTH / 2.0);
        assert_eq!(curve.end.y, 30.0);
        assert_eq!(curve.distance_to(curve.midpoint()), 0.0);
    }

    #[test]
    fn backward_curve_bows_below_nodes() {
        let curve =
            EdgeCurve::between(Position { x: 300.0, y: 20.0 }, Position { x: 0.0, y: 40.0 });

        assert!(curve.backwards);
        assert_eq!(curve.start.y, 20.0 + NODE_HEIGHT / 2.0);
        assert_eq!(curve.end.y, 40.0 + NODE_HEIGHT / 2.0);
        assert!(curve.control_a.y > curve.start.y);
        assert!(curve.control_b.y > curve.end.y);
    }

    #[test]
    fn self_loop_stays_outside_normal_and_expanded_cards_without_retracing() {
        let position = Position { x: 300.0, y: -40.0 };
        for (width, height) in [
            (NODE_WIDTH, NODE_HEIGHT),
            (EXPANDED_NODE_WIDTH, EXPANDED_NODE_HEIGHT),
        ] {
            let curve = EdgeCurve::self_loop(position, (width, height));
            let bottom = position.y + height / 2.0;
            assert!(curve.backwards);
            assert_eq!(curve.start.y, bottom);
            assert_eq!(curve.end.y, bottom);
            assert!(curve.start.x > position.x);
            assert!(curve.end.x < position.x);
            assert!(curve.start.x < position.x + width / 2.0);
            assert!(curve.end.x > position.x - width / 2.0);

            let mut previous = curve.start;
            for step in 1..=32 {
                let sample = curve.at(step as f32 / 32.0);
                assert!(sample.x.is_finite() && sample.y.is_finite());
                assert!(
                    sample.x < previous.x,
                    "the loop must not retrace itself"
                );
                if step < 32 {
                    assert!(sample.y > bottom, "the loop must clear the card");
                }
                assert!(curve.distance_to(sample) < 0.001);
                previous = sample;
            }
            assert!(curve.midpoint().y > bottom + EDGE_HIT_TOLERANCE);
            assert!(curve.distance_to(position) > EDGE_HIT_TOLERANCE);

            let approach = curve.at(0.94);
            assert!(
                approach.y > curve.end.y,
                "the arrow must point into the card"
            );
            assert!(curve.end.y - curve.control_b.y < 0.0);
            assert!(curve.start.y < curve.control_a.y);
        }
    }

    #[test]
    fn snap_rounds_to_canvas_grid() {
        assert_eq!(snap(11.9), 8.0);
        assert_eq!(snap(12.1), 16.0);
        assert_eq!(snap(-12.1), -16.0);
    }
}
