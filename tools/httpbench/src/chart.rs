//! SVG chart rendering.
//!
//! One shared theme and one generic renderer, so every chart in the report
//! reads the same way: log-scaled sweep axis, a legend keyed by engine, and
//! markers on every measured point so it is obvious where the data actually is
//! rather than where the interpolation went.

use std::path::Path;

use plotters::{
    coord::{
        Shift,
        ranged1d::{AsRangedCoord, Ranged, ValueFormatter},
    },
    prelude::*,
    style::text_anchor::{HPos, Pos, VPos},
};

use crate::Result;

const WIDTH: u32 = 1100;
const HEIGHT: u32 = 620;
const FONT: &str = "Segoe UI";

/// Colour per engine, so a series means the same thing in every chart.
pub const HTTPSYS_COLOR: RGBColor = RGBColor(0, 114, 198);
pub const AXUM_COLOR: RGBColor = RGBColor(214, 73, 55);

/// One line on a chart.
pub struct Series {
    pub name: String,
    pub color: RGBColor,
    /// Dashed lines are used for the tail-latency companion of a solid median.
    pub dashed: bool,
    pub points: Vec<(f64, f64)>,
}

/// Everything needed to render one chart.
///
/// The x axis is always a base-2 log scale: every sweep in this harness
/// doubles its independent variable.
pub struct Chart<'a> {
    pub caption: &'a str,
    pub subtitle: &'a str,
    pub x_desc: &'a str,
    pub y_desc: &'a str,
    pub y_log: bool,
    pub x_format: fn(f64) -> String,
    pub y_format: fn(f64) -> String,
    pub series: Vec<Series>,
}

impl Chart<'_> {
    pub fn render(&self, path: &Path) -> Result<()> {
        let (x_min, x_max, y_min, y_max) = self.bounds()?;
        let root = SVGBackend::new(path, (WIDTH, HEIGHT)).into_drawing_area();
        root.fill(&WHITE)?;

        // Caption and subtitle are drawn by hand: plotters' built-in caption
        // has no room for a second line.
        root.draw_text(
            self.caption,
            &TextStyle::from((FONT, 24).into_font().style(FontStyle::Bold))
                .color(&BLACK)
                .pos(Pos::new(HPos::Left, VPos::Top)),
            (20, 16),
        )?;
        root.draw_text(
            self.subtitle,
            &TextStyle::from((FONT, 13).into_font())
                .color(&RGBColor(110, 110, 110))
                .pos(Pos::new(HPos::Left, VPos::Top)),
            (20, 48),
        )?;

        let plot = root.margin(76, 24, 20, 20);
        // Both sweeps double their x value at every step, so a base-2 log axis
        // puts a tick on each measured point instead of near it.
        let x = (x_min..x_max).log_scale().base(2.0);
        match self.y_log {
            true => self.draw(&plot, x, (y_min..y_max).log_scale()),
            false => self.draw(&plot, x, y_min..y_max),
        }?;

        root.present()?;
        Ok(())
    }

    /// Axis bounds with a little headroom, and a floor that keeps log scales
    /// away from zero.
    fn bounds(&self) -> Result<(f64, f64, f64, f64)> {
        let points = || self.series.iter().flat_map(|s| s.points.iter());
        if points().next().is_none() {
            return Err(format!("chart '{}' has no data points", self.caption).into());
        }

        let x_min = points().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let x_max = points().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let y_max = points().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let y_min = points().map(|p| p.1).fold(f64::INFINITY, f64::min);

        // Only a little x padding: the ticks are already on the data points.
        let (x_min, x_max) = (x_min / 1.35, x_max * 1.35);
        let (y_min, y_max) = if self.y_log {
            pad(y_min, y_max, true, 1.0)
        } else {
            // Linear axes start at zero: for throughput and CPU the distance
            // from nothing is the whole point.
            (0.0, y_max * 1.08)
        };
        Ok((x_min, x_max, y_min, y_max))
    }

    fn draw<XR, YR>(&self, area: &DrawingArea<SVGBackend<'_>, Shift>, x: XR, y: YR) -> Result<()>
    where
        XR: AsRangedCoord<Value = f64>,
        YR: AsRangedCoord<Value = f64>,
        XR::CoordDescType: ValueFormatter<f64> + Ranged<ValueType = f64>,
        YR::CoordDescType: ValueFormatter<f64> + Ranged<ValueType = f64>,
    {
        let mut chart = ChartBuilder::on(area)
            .set_label_area_size(LabelAreaPosition::Left, 78)
            .set_label_area_size(LabelAreaPosition::Bottom, 52)
            .build_cartesian_2d(x, y)?;

        let x_format = self.x_format;
        let y_format = self.y_format;
        chart
            .configure_mesh()
            .light_line_style(RGBColor(238, 238, 238))
            .bold_line_style(RGBColor(214, 214, 214))
            .axis_style(RGBColor(120, 120, 120))
            .label_style((FONT, 13).into_font().color(&RGBColor(70, 70, 70)))
            .x_desc(self.x_desc)
            .y_desc(self.y_desc)
            .x_label_formatter(&move |v| x_format(*v))
            .y_label_formatter(&move |v| y_format(*v))
            .x_labels(9)
            .y_labels(8)
            .draw()?;

        for series in &self.series {
            let color = series.color;
            let points = series.points.clone();
            if series.dashed {
                chart.draw_series(DashedLineSeries::new(
                    points.iter().copied(),
                    7,
                    5,
                    color.stroke_width(2),
                ))?;
            } else {
                chart.draw_series(LineSeries::new(
                    points.iter().copied(),
                    color.stroke_width(3),
                ))?;
            }
            chart
                .draw_series(points.iter().map(|&p| Circle::new(p, 4, color.filled())))?
                .label(&series.name)
                .legend(move |(x, y)| {
                    PathElement::new(vec![(x, y), (x + 22, y)], color.stroke_width(3))
                });
        }

        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperLeft)
            .label_font((FONT, 14).into_font())
            .background_style(WHITE.mix(0.85))
            .border_style(RGBColor(190, 190, 190))
            .draw()?;
        Ok(())
    }
}

/// Widen a range so the extreme points are not glued to the frame.
fn pad(min: f64, max: f64, logarithmic: bool, fallback: f64) -> (f64, f64) {
    if !min.is_finite() || !max.is_finite() {
        return (0.0, fallback);
    }
    if logarithmic {
        let min = min.max(f64::MIN_POSITIVE);
        let max = max.max(min * 2.0);
        (min / 1.35, max * 1.35)
    } else if (max - min).abs() < f64::EPSILON {
        (min - fallback, max + fallback)
    } else {
        let margin = (max - min) * 0.08;
        (min - margin, max + margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(series: Vec<Series>) -> Chart<'static> {
        Chart {
            caption: "test",
            subtitle: "sub",
            x_desc: "x",
            y_desc: "y",
            y_log: false,
            x_format: |v| format!("{v}"),
            y_format: |v| format!("{v}"),
            series,
        }
    }

    #[test]
    fn log_padding_never_reaches_zero() {
        let (min, max) = pad(1.0, 100.0, true, 1.0);
        assert!(min > 0.0);
        assert!(max > 100.0);
    }

    #[test]
    fn flat_linear_data_still_gets_a_range() {
        let (min, max) = pad(5.0, 5.0, false, 1.0);
        assert!(min < max);
    }

    #[test]
    fn linear_y_axis_starts_at_zero() {
        let c = chart(vec![Series {
            name: "a".into(),
            color: HTTPSYS_COLOR,
            dashed: false,
            points: vec![(1.0, 10.0), (2.0, 20.0)],
        }]);
        let (_, _, y_min, y_max) = c.bounds().unwrap();
        assert_eq!(y_min, 0.0);
        assert!(y_max > 20.0);
    }

    #[test]
    fn empty_charts_are_an_error_not_a_panic() {
        assert!(chart(Vec::new()).bounds().is_err());
    }

    #[test]
    fn renders_an_svg_file() {
        let dir = std::env::temp_dir().join("httpbench-chart-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chart.svg");
        chart(vec![Series {
            name: "httpsys".into(),
            color: HTTPSYS_COLOR,
            dashed: false,
            points: vec![(1.0, 10.0), (8.0, 40.0), (64.0, 90.0)],
        }])
        .render(&path)
        .expect("render");
        let svg = std::fs::read_to_string(&path).unwrap();
        assert!(
            svg.starts_with("<svg"),
            "not an svg: {}",
            &svg[..40.min(svg.len())]
        );
        assert!(svg.contains("httpsys"), "legend missing");
    }
}
