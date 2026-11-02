//! Minimal time charts drawn with the egui painter (x = seconds since SpotiPod start).

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

pub struct Series<'a> {
    pub name: &'a str,
    pub color: Color32,
    pub points: Vec<[f64; 2]>,
}

pub enum Kind {
    Line,
    /// One bar per point (per-second buckets).
    Bars,
}

pub struct Chart<'a> {
    pub title: &'a str,
    pub unit: &'a str,
    pub kind: Kind,
    pub height: f32,
    /// Visible x range, seconds.
    pub x: (f64, f64),
    /// Lower bound of the y maximum, so a flat series does not fill the chart.
    pub min_max: f64,
}

impl Chart<'_> {
    pub fn show(&self, ui: &mut Ui, series: &[Series]) {
        let visuals = ui.visuals().clone();
        ui.label(egui::RichText::new(self.title).strong());
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);

        let (x0, x1) = (self.x.0, self.x.1.max(self.x.0 + 1.0));
        let y_max = series
            .iter()
            .flat_map(|s| &s.points)
            .filter(|p| p[0] >= x0 && p[0] <= x1)
            .map(|p| p[1])
            .fold(self.min_max, f64::max)
            * 1.1;
        let plot = rect.shrink2(vec2(6.0, 8.0));
        let to_screen = |x: f64, y: f64| -> Pos2 {
            pos2(
                plot.left() + ((x - x0) / (x1 - x0)) as f32 * plot.width(),
                plot.bottom() - (y / y_max) as f32 * plot.height(),
            )
        };

        let grid = Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color);
        let text = visuals.weak_text_color();
        for i in 1..4 {
            let y = plot.bottom() - plot.height() * i as f32 / 4.0;
            painter.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], grid);
            painter.text(
                pos2(plot.left() + 2.0, y - 1.0),
                Align2::LEFT_BOTTOM,
                format!("{} {}", fmt_value(y_max * i as f64 / 4.0), self.unit),
                FontId::proportional(10.0),
                text,
            );
        }

        for s in series {
            let pts: Vec<Pos2> = s
                .points
                .iter()
                .filter(|p| p[0] >= x0 && p[0] <= x1)
                .map(|p| to_screen(p[0], p[1]))
                .collect();
            match self.kind {
                Kind::Line => {
                    if pts.len() > 1 {
                        painter.line(pts, Stroke::new(1.5, s.color));
                    }
                }
                Kind::Bars => {
                    let w = (plot.width() / (x1 - x0) as f32).max(1.0);
                    for p in pts {
                        let r = Rect::from_min_max(
                            pos2(p.x - w * 0.4, p.y),
                            pos2(p.x + w * 0.4, plot.bottom()),
                        );
                        painter.rect_filled(r, 0.0, s.color);
                    }
                }
            }
        }

        // Hover: values nearest to the cursor.
        if let Some(pos) = response.hover_pos() {
            let x = x0 + ((pos.x - plot.left()) / plot.width()) as f64 * (x1 - x0);
            painter.line_segment(
                [pos2(pos.x, plot.top()), pos2(pos.x, plot.bottom())],
                Stroke::new(1.0, text),
            );
            let mut lines = vec![format!("t = {}", fmt_time(x))];
            for s in series {
                let nearest = s
                    .points
                    .iter()
                    .min_by(|a, b| (a[0] - x).abs().total_cmp(&(b[0] - x).abs()));
                if let Some(p) = nearest {
                    lines.push(format!("{}: {} {}", s.name, fmt_value(p[1]), self.unit));
                }
            }
            response.on_hover_text(lines.join("\n"));
        }

        // Legend.
        if series.len() > 1 {
            ui.horizontal(|ui| {
                for s in series {
                    ui.colored_label(s.color, format!("■ {}", s.name));
                }
            });
        }
    }
}

pub fn fmt_value(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
