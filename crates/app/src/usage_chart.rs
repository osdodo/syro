//! The usage panel's day chart: one column per day, one segment per model, each model
//! in the colour it keeps everywhere else in the panel.
//!
//! A stacked column is what `BarChart` cannot draw — it plots one number per band —
//! so this is a [`Plot`] of its own, over the same primitives the built-in charts are
//! built from.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::plot::{
    AxisText, Grid, IntoPlot, Plot, PlotAxis, axis_gutter,
    label::TEXT_SIZE,
    scale::{Scale, ScaleBand, ScaleLinear},
    shape::Bar,
    tooltip::{CrossLine, Tooltip, TooltipState},
};
use gpui_kit::*;

/// The height kept under the plot for the day labels.
fn axis_gap() -> f32 {
    axis_gutter(px(TEXT_SIZE))
}

/// One model's part of one day.
#[derive(Clone)]
pub struct ModelSpend {
    pub model: SharedString,
    pub tokens: u64,
    pub cost: f64,
    pub estimated: bool,
    pub unpriced: bool,
    pub color: Hsla,
}

/// One day's column, bottom segment first.
#[derive(Clone)]
pub struct DaySpend {
    pub label: SharedString,
    pub models: Vec<ModelSpend>,
}

/// The panel's day chart.
#[derive(IntoPlot)]
pub struct ModelBars {
    days: Vec<DaySpend>,
}

/// What one segment is drawn as: where its band starts, where its base and its top
/// sit on the value axis, and what fills it.
struct Segment {
    cross: f32,
    base: f32,
    top: f32,
    color: Hsla,
}

impl ModelBars {
    pub fn new(days: Vec<DaySpend>) -> Self {
        Self { days }
    }

    /// The bands, one per day, in the window's own width.
    fn bands(&self, width: f32) -> ScaleBand<SharedString> {
        ScaleBand::new(self.days.iter().map(|day| day.label.clone()), [0., width])
            .max_band_width(30.)
            .padding_inner(0.4)
            .padding_outer(0.2)
    }

    /// The tallest column: the scale's top, and what every segment is measured from.
    fn tallest(&self) -> f64 {
        self.days
            .iter()
            .map(|day| day.models.iter().map(|model| model.tokens).sum::<u64>())
            .max()
            .unwrap_or(0)
            .max(1) as f64
    }
}

impl Plot for ModelBars {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let height = bounds.size.height.as_f32() - axis_gap();
        let bands = self.bands(bounds.size.width.as_f32());
        let band_width = bands.band_width();
        // A few points of headroom above the tallest column, so its top segment is not
        // flush with the chart's own edge.
        let value = ScaleLinear::new(vec![0., self.tallest()], [height, 6.]);

        let labels = self.days.iter().filter_map(|day| {
            bands.tick(&day.label).map(|tick| {
                AxisText::new(
                    day.label.clone(),
                    tick + band_width / 2.,
                    cx.theme().muted_foreground,
                )
                .align(TextAlign::Center)
            })
        });
        PlotAxis::new()
            .x(height)
            .x_label(labels)
            .stroke(cx.theme().border)
            .paint(&bounds, window, cx);
        Grid::new()
            .y((0..=3).map(|step| height * step as f32 / 4.))
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        let mut segments = Vec::new();
        for day in &self.days {
            let Some(cross) = bands.tick(&day.label) else {
                continue;
            };
            let mut base = 0;
            for model in &day.models {
                let top = base + model.tokens;
                segments.push(Segment {
                    cross,
                    // The scale runs bottom-up: its first pixel is the baseline.
                    base: value.tick(&(base as f64)).unwrap_or(height),
                    top: value.tick(&(top as f64)).unwrap_or(height),
                    color: model.color,
                });
                base = top;
            }
        }
        Bar::new()
            .data(segments)
            .band_width(band_width)
            .cross(|segment| Some(segment.cross))
            .base(|segment| segment.base)
            .value(|segment| Some(segment.top))
            .fill(|segment, _, _| segment.color)
            .paint(&bounds, window, cx);
    }

    fn id(&self) -> Option<ElementId> {
        Some("usage-days-chart".into())
    }

    fn tooltip_state(
        &self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        _cx: &App,
    ) -> Option<TooltipState> {
        // The gutter under the plot is the axis labels', not a column's.
        if position.y.as_f32() > bounds.size.height.as_f32() - axis_gap() {
            return None;
        }
        let bands = self.bands(bounds.size.width.as_f32());
        let index = bands.nearest_index(position.x.as_f32());
        let day = self.days.get(index)?;
        let center = bands.tick(&day.label)? + bands.band_width() / 2.;
        Some(TooltipState::new(
            index,
            point(px(center), position.y),
            vec![],
        ))
    }

    fn tooltip(
        &self,
        state: &TooltipState,
        cursor: Point<Pixels>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<AnyElement> {
        let day = self.days.get(state.index)?;
        let bands = self.bands(bounds.size.width.as_f32());
        let mut tooltip = Tooltip::new(cursor, bounds.size)
            .gap(px(8.))
            .cross_line(
                CrossLine::new(state.cross_line)
                    .height(bounds.size.height.as_f32() - axis_gap())
                    .band(px(bands.band_width())),
            )
            .title(day.label.clone());

        // One row per model, in the same colours and the same order as the column.
        let mut tokens = 0;
        let mut cost = 0.;
        let mut estimated = false;
        let mut unpriced = false;
        for model in &day.models {
            tokens += model.tokens;
            cost += model.cost;
            estimated |= model.estimated;
            unpriced |= model.unpriced;
            tooltip = tooltip.row(
                model.color,
                model.model.clone(),
                format!(
                    "{} tok · {}",
                    crate::resource_bar::format_tokens(model.tokens),
                    crate::resource_bar::format_cost(model.cost, model.estimated, model.unpriced)
                ),
            );
        }
        Some(
            tooltip
                .plain_row(
                    "",
                    format!(
                        "{} tok · {}",
                        crate::resource_bar::format_tokens(tokens),
                        crate::resource_bar::format_cost(cost, estimated, unpriced)
                    ),
                )
                .into_any_element(),
        )
    }
}
