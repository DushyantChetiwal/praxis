use architect::Position;
use gpui::{Hsla, PathBuilder, Pixels, Point, Window, point, px};

/// Node size in canvas units. The layout's spacing is chosen around these, so
/// the two have to agree.
pub(super) const NODE_WIDTH: f32 = 248.0;
pub(super) const NODE_HEIGHT: f32 = 104.0;

/// A step showing its sub-plan inside itself. Wider and taller than a plain
/// step, because it has a row of children to fit.
pub(super) const EXPANDED_NODE_WIDTH: f32 = 344.0;
pub(super) const EXPANDED_NODE_HEIGHT: f32 = 188.0;

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

    fn at(&self, t: f32) -> Position {
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
    fn snap_rounds_to_canvas_grid() {
        assert_eq!(snap(11.9), 8.0);
        assert_eq!(snap(12.1), 16.0);
        assert_eq!(snap(-12.1), -16.0);
    }
}
