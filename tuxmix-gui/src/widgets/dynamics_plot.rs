//! Amplitude graph for the Dynamics panel.
//!
//! The horizontal axis is input level and the vertical axis is output level,
//! both in dBFS. The curve shows the compressor above its threshold and the
//! expander below its threshold, with the shared CompEx gain applied. It is a
//! parameter preview, like the graph in the RME Dynamics panel.

use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

use tuxmix_core::Dynamics;

use crate::theme;

const IN_MIN: f32 = -60.0;
const IN_MAX: f32 = 0.0;
const OUT_MIN: f32 = -60.0;
const OUT_MAX: f32 = 24.0;

struct DynamicsPlot {
    dynamics: Dynamics,
    scale: f32,
}

fn transfer(db: f32, d: &Dynamics) -> f32 {
    let mut y = db;
    if d.comp_ratio > 1.0 && db > d.comp_threshold_db {
        y = d.comp_threshold_db + (db - d.comp_threshold_db) / d.comp_ratio;
    }
    if d.exp_ratio > 1.0 && db < d.exp_threshold_db {
        y = d.exp_threshold_db + (db - d.exp_threshold_db) * d.exp_ratio;
    }
    (y + d.gain_db).clamp(OUT_MIN, OUT_MAX)
}

fn x_of(db: f32, plot: Rectangle) -> f32 {
    plot.x + (db - IN_MIN) / (IN_MAX - IN_MIN) * plot.width
}

fn y_of(db: f32, plot: Rectangle) -> f32 {
    plot.y + (1.0 - (db - OUT_MIN) / (OUT_MAX - OUT_MIN)) * plot.height
}

impl<Message> canvas::Program<Message> for DynamicsPlot {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let margin = 8.0 * self.scale;
        let plot = Rectangle {
            x: margin,
            y: margin,
            width: (bounds.width - margin * 2.0).max(1.0),
            height: (bounds.height - margin * 2.0).max(1.0),
        };

        let grid = Color { a: 0.14, ..theme::TEXT_SEC };
        for db in [-60.0, -40.0, -20.0, 0.0, 20.0] {
            if !(OUT_MIN..=OUT_MAX).contains(&db) {
                continue;
            }
            let y = y_of(db, plot);
            frame.stroke(
                &Path::line(Point::new(plot.x, y), Point::new(plot.x + plot.width, y)),
                Stroke::default().with_color(grid).with_width(1.0),
            );
        }
        for db in [-60.0, -40.0, -20.0, 0.0] {
            let x = x_of(db, plot);
            frame.stroke(
                &Path::line(Point::new(x, plot.y), Point::new(x, plot.y + plot.height)),
                Stroke::default().with_color(grid).with_width(1.0),
            );
        }

        let unity = Path::line(
            Point::new(x_of(IN_MIN, plot), y_of(IN_MIN, plot)),
            Point::new(x_of(IN_MAX, plot), y_of(IN_MAX, plot)),
        );
        frame.stroke(
            &unity,
            Stroke::default()
                .with_color(Color { a: 0.3, ..theme::TEXT_SEC })
                .with_width(1.0),
        );

        // RME colours the graph by section: green below the expander
        // threshold, blue between the two thresholds, red above the
        // compressor threshold.
        let a = if self.dynamics.on { 1.0 } else { 0.4 };
        let exp = self.dynamics.exp_threshold_db.clamp(IN_MIN, IN_MAX);
        let comp = self.dynamics.comp_threshold_db.clamp(IN_MIN, IN_MAX);
        let (low, high) = if exp <= comp { (exp, comp) } else { (comp, exp) };
        let green = Color { a, ..theme::MGREEN };
        let blue = Color { a, ..theme::MUTE_COLOR };
        let red = Color { a, ..theme::MRED };
        let width = 1.5 * self.scale;

        let mut segment = |from: f32, to: f32, colour: Color| {
            if to - from < 0.01 {
                return;
            }
            let steps = 60;
            let mut points = Vec::with_capacity(steps + 1);
            for i in 0..=steps {
                let db = from + (to - from) * (i as f32 / steps as f32);
                points.push(Point::new(
                    x_of(db, plot),
                    y_of(transfer(db, &self.dynamics), plot),
                ));
            }
            let path = Path::new(|p| {
                p.move_to(points[0]);
                for point in &points[1..] {
                    p.line_to(*point);
                }
            });
            frame.stroke(&path, Stroke::default().with_color(colour).with_width(width));
        };

        segment(IN_MIN, low, green);
        segment(low, high, blue);
        segment(high, IN_MAX, red);

        vec![frame.into_geometry()]
    }
}

pub fn dynamics_plot<'a, Message: 'a>(
    dynamics: Dynamics,
    height: f32,
    scale: f32,
) -> Element<'a, Message> {
    Canvas::new(DynamicsPlot { dynamics, scale })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_bends_above_threshold() {
        let d = Dynamics {
            comp_threshold_db: -20.0,
            comp_ratio: 2.0,
            exp_ratio: 1.0,
            ..Dynamics::default()
        };
        assert!((transfer(0.0, &d) + 10.0).abs() < 1e-4);
        assert!((transfer(-30.0, &d) + 30.0).abs() < 1e-4);
    }

    #[test]
    fn expander_bends_below_threshold() {
        let d = Dynamics {
            exp_threshold_db: -40.0,
            exp_ratio: 2.0,
            comp_ratio: 1.0,
            ..Dynamics::default()
        };
        assert!((transfer(-50.0, &d) + 60.0).abs() < 1e-4);
        assert!((transfer(-20.0, &d) + 20.0).abs() < 1e-4);
    }

    #[test]
    fn shared_gain_offsets_the_curve() {
        let d = Dynamics {
            gain_db: 3.0,
            comp_ratio: 1.0,
            exp_ratio: 1.0,
            ..Dynamics::default()
        };
        assert!((transfer(-12.0, &d) + 9.0).abs() < 1e-4);
    }
}
