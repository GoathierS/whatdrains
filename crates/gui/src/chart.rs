//! Battery level chart, drawn with cairo.

use gtk::cairo::Context;
use gtk::glib;
use gtk::prelude::*;

use whatdrains_core::battery::PowerState;
use whatdrains_core::store::BatteryPoint;

/// A gap longer than this means the daemon was not running (or the laptop slept).
const MAX_GAP_S: i64 = 5 * 60;

pub struct ChartData {
    pub points: Vec<BatteryPoint>,
    pub since: i64,
    pub until: i64,
}

pub fn draw(area: &gtk::DrawingArea, cr: &Context, width: i32, height: i32, data: &ChartData) {
    let fg = area.color();
    let (w, h) = (width as f64, height as f64);
    let (left, right, top, bottom) = (36.0, 8.0, 8.0, 22.0);
    let plot_w = (w - left - right).max(1.0);
    let plot_h = (h - top - bottom).max(1.0);
    let span = (data.until - data.since).max(1) as f64;
    let x_of = |ts: i64| left + (ts - data.since) as f64 / span * plot_w;
    let y_of = |pct: f64| top + (1.0 - pct / 100.0) * plot_h;

    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    cr.set_font_size(10.0);

    // Plugged-in periods.
    cr.set_source_rgba(0.21, 0.52, 0.89, 0.16);
    for p in data.points.iter().filter(|p| matches!(p.state, PowerState::Charging | PowerState::OnAc)) {
        cr.rectangle(x_of(p.ts), top, (60.0 / span * plot_w).max(1.0), plot_h);
    }
    let _ = cr.fill();

    // Horizontal grid and percentage labels.
    for pct in [0.0, 50.0, 100.0] {
        let y = y_of(pct);
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.12);
        cr.move_to(left, y.round() + 0.5);
        cr.line_to(w - right, y.round() + 0.5);
        let _ = cr.stroke();
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.55);
        let label = format!("{pct:.0} %");
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(left - 6.0 - ext.width(), y + ext.height() / 2.0);
            let _ = cr.show_text(&label);
        }
    }

    // Time labels.
    let (step, fmt) = if span <= 2.0 * 86_400.0 { (6 * 3600, "%H:%M") } else { (86_400, "%a") };
    let offset = glib::DateTime::now_local().map(|d| d.utc_offset().as_seconds()).unwrap_or(0);
    let mut t = (data.since + offset).div_euclid(step) * step - offset + step;
    while t < data.until {
        if let Some(label) = glib::DateTime::from_unix_local(t).ok().and_then(|d| d.format(fmt).ok()) {
            if let Ok(ext) = cr.text_extents(&label) {
                cr.move_to(x_of(t) - ext.width() / 2.0, h - 6.0);
                let _ = cr.show_text(&label);
            }
        }
        t += step;
    }

    // Battery level: one segment per continuous run of samples.
    let mut segments: Vec<&[BatteryPoint]> = Vec::new();
    let mut start = 0;
    for i in 1..=data.points.len() {
        if i == data.points.len() || data.points[i].ts - data.points[i - 1].ts > MAX_GAP_S {
            segments.push(&data.points[start..i]);
            start = i;
        }
    }
    for seg in segments.iter().filter(|s| !s.is_empty()) {
        let first = &seg[0];
        let last = &seg[seg.len() - 1];

        cr.move_to(x_of(first.ts), y_of(0.0));
        for p in seg.iter() {
            cr.line_to(x_of(p.ts), y_of(p.percent));
        }
        cr.line_to(x_of(last.ts), y_of(0.0));
        cr.close_path();
        cr.set_source_rgba(0.2, 0.82, 0.48, 0.18);
        let _ = cr.fill();

        cr.move_to(x_of(first.ts), y_of(first.percent));
        for p in seg.iter() {
            cr.line_to(x_of(p.ts), y_of(p.percent));
        }
        cr.set_source_rgb(0.18, 0.76, 0.49);
        cr.set_line_width(2.0);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        let _ = cr.stroke();
    }
}
