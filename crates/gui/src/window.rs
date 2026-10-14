use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::glib;

use whatdrains_core::apps::AppKind;
use whatdrains_core::battery::{self, PowerState};
use whatdrains_core::store::{self, AppTotal, Store};

use crate::chart::{self, ChartData};

const REFRESH_EVERY_S: u32 = 60;
/// The daemon writes a record every minute; older than this means it is not running.
const STALE_AFTER_S: i64 = 3 * 60;
const MAX_ROWS: usize = 30;
const SERVICE: &str = "whatdrainsd.service";

struct Ui {
    hours: Cell<i64>,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    stack: gtk::Stack,
    level: gtk::Label,
    details: gtk::Label,
    chart: gtk::DrawingArea,
    chart_data: Rc<RefCell<ChartData>>,
    usage_time: gtk::Label,
    apps: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,
}

pub fn build(app: &adw::Application) {
    let range_day = gtk::ToggleButton::builder().label("24 Hours").active(true).build();
    let range_week = gtk::ToggleButton::builder().label("7 Days").group(&range_day).build();
    let range = gtk::Box::builder().css_classes(["linked"]).build();
    range.append(&range_day);
    range.append(&range_week);

    let header = adw::HeaderBar::builder().title_widget(&range).build();

    let banner = adw::Banner::builder().title("Background recording is off").button_label("Turn On").build();

    let level = gtk::Label::builder().css_classes(["title-1"]).xalign(0.0).build();
    let details = gtk::Label::builder().css_classes(["dim-label"]).xalign(0.0).wrap(true).build();

    let chart_data = Rc::new(RefCell::new(ChartData { points: Vec::new(), since: 0, until: 1 }));
    let chart = gtk::DrawingArea::builder().content_height(170).build();
    {
        let data = Rc::clone(&chart_data);
        chart.set_draw_func(move |area, cr, w, h| chart::draw(area, cr, w, h, &data.borrow()));
    }
    let chart_card = gtk::Frame::builder().css_classes(["card"]).child(&chart).build();
    chart.set_margin_top(12);
    chart.set_margin_bottom(6);
    chart.set_margin_start(6);
    chart.set_margin_end(12);

    let apps = adw::PreferencesGroup::builder().title("Apps").build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let summary = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    summary.append(&level);
    summary.append(&details);
    content.append(&summary);
    let usage_time = gtk::Label::builder().css_classes(["dim-label", "caption"]).xalign(0.0).build();
    let chart_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    chart_box.append(&chart_card);
    chart_box.append(&usage_time);
    content.append(&chart_box);
    content.append(&apps);

    let clamp = adw::Clamp::builder().maximum_size(640).child(&content).build();
    let scrolled =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();

    let empty = adw::StatusPage::builder()
        .icon_name("battery-level-50-symbolic")
        .title("No Data Yet")
        .description("What Drains records battery use in the background.\nCome back in a few minutes.")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&scrolled, Some("content"));
    stack.add_named(&empty, Some("empty"));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("What Drains")
        .default_width(520)
        .default_height(760)
        .content(&toolbar)
        .build();

    let ui = Rc::new(Ui {
        hours: Cell::new(24),
        toasts,
        banner: banner.clone(),
        stack,
        level,
        details,
        chart,
        chart_data,
        usage_time,
        apps,
        rows: RefCell::new(Vec::new()),
    });

    {
        let ui = Rc::clone(&ui);
        range_week.connect_toggled(move |b| {
            ui.hours.set(if b.is_active() { 24 * 7 } else { 24 });
            refresh(&ui);
        });
    }
    {
        let ui = Rc::clone(&ui);
        banner.connect_button_clicked(move |_| start_daemon(&ui));
    }
    {
        let ui = Rc::clone(&ui);
        glib::timeout_add_seconds_local(REFRESH_EVERY_S, move || {
            refresh(&ui);
            glib::ControlFlow::Continue
        });
    }

    refresh(&ui);
    window.present();

    // Development helper used to produce the README screenshot.
    if let Some(path) = std::env::var_os("WHATDRAINS_SCREENSHOT") {
        glib::timeout_add_seconds_local_once(2, move || {
            if let Err(e) = save_screenshot(&window, std::path::Path::new(&path)) {
                eprintln!("whatdrains: screenshot failed: {e}");
            }
            window.close();
        });
    }
}

fn save_screenshot(window: &adw::ApplicationWindow, path: &std::path::Path) -> Result<(), String> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let (w, h) = (window.width() as f64, window.height() as f64);
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w, h);
    let node = snapshot.to_node().ok_or("empty window")?;
    let renderer = window.renderer().ok_or("no renderer")?;
    renderer.render_texture(node, None).save_to_png(path).map_err(|e| e.to_string())
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn start_daemon(ui: &Rc<Ui>) {
    let ok = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", SERVICE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        ui.toasts.add_toast(adw::Toast::new("Could not start whatdrainsd.service — is it installed?"));
        return;
    }
    ui.toasts.add_toast(adw::Toast::new("Recording started"));
    ui.banner.set_revealed(false);
    let ui = Rc::clone(ui);
    glib::timeout_add_seconds_local_once(70, move || refresh(&ui));
}

fn refresh(ui: &Ui) {
    refresh_battery_now(ui);

    let now = now_unix();
    let since = now - ui.hours.get() * 3600;
    let path = store::default_path();
    let db = path.exists().then(|| Store::open_read_only(&path).ok()).flatten();

    let last = db.as_ref().and_then(|db| db.last_record().ok().flatten());
    ui.banner.set_revealed(last.is_none_or(|t| now - t > STALE_AFTER_S));

    let points = db.as_ref().and_then(|db| db.battery_since(since).ok()).unwrap_or_default();
    let apps = db.as_ref().and_then(|db| db.apps_since(since).ok()).unwrap_or_default();

    ui.stack.set_visible_child_name(if points.is_empty() && apps.is_empty() { "empty" } else { "content" });

    let on_battery_s = 60.0 * points.iter().filter(|p| p.state == PowerState::Discharging).count() as f64;
    let screen_on_s: f64 = points.iter().map(|p| p.screen_on_s).sum();
    ui.usage_time.set_label(&format!("On battery {} · Screen on {}", duration(on_battery_s), duration(screen_on_s)));

    *ui.chart_data.borrow_mut() = ChartData { points, since, until: now };
    ui.chart.queue_draw();

    fill_apps(ui, &apps);
}

fn refresh_battery_now(ui: &Ui) {
    let Some(r) = battery::find_battery().and_then(|p| battery::read_battery(&p)) else {
        ui.level.set_label("No Battery");
        ui.details.set_label("Apps are ranked by CPU and GPU time");
        return;
    };
    ui.level.set_label(&format!("{:.0} %", r.percent));
    let mut parts = vec![match r.state {
        PowerState::Discharging => "On battery".to_string(),
        PowerState::Charging => "Charging".to_string(),
        PowerState::OnAc => "Plugged in".to_string(),
        PowerState::Unknown => "Unknown state".to_string(),
    }];
    if let Some(p) = r.power_w.filter(|_| r.state != PowerState::OnAc) {
        parts.push(format!("{p:.1} W"));
    }
    if let Some(t) = r.time_left_s {
        let what = if r.state == PowerState::Charging { "until full" } else { "left" };
        parts.push(format!("{} {what}", duration(t)));
    }
    ui.details.set_label(&parts.join(" · "));
}

fn fill_apps(ui: &Ui, apps: &[AppTotal]) {
    for row in ui.rows.borrow_mut().drain(..) {
        ui.apps.remove(&row);
    }

    let total_j: f64 = apps.iter().map(|a| a.usage.energy_j).sum();
    let total_activity: f64 = apps.iter().map(|a| a.usage.cpu_s + a.usage.gpu_s).sum();
    let by_energy = total_j > 0.0;
    ui.apps.set_description(Some(&if by_energy {
        format!("Estimated share of the {:.1} Wh used on battery", total_j / 3600.0)
    } else {
        "No time on battery in this period, apps are ranked by CPU and GPU time".to_string()
    }));

    let has_screen = apps.iter().any(|a| a.kind == AppKind::Screen);
    let theme = gtk::IconTheme::for_display(&ui.apps.display());
    let mut rows = ui.rows.borrow_mut();
    for a in apps {
        let share = if by_energy {
            a.usage.energy_j / total_j
        } else {
            (a.usage.cpu_s + a.usage.gpu_s) / total_activity.max(1e-9)
        };
        if share < 0.001 || rows.len() >= MAX_ROWS {
            continue;
        }

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&a.label))
            .subtitle(subtitle(a, has_screen))
            .build();
        row.add_prefix(&icon(a, &theme));

        let pct = gtk::Label::builder()
            .label(format!("{:.0} %", (share * 100.0).max(1.0)))
            .css_classes(["numeric"])
            .width_chars(5)
            .xalign(1.0)
            .build();
        let bar = gtk::LevelBar::builder().value(share).width_request(72).valign(gtk::Align::Center).build();
        bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
        bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
        bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_FULL));
        row.add_suffix(&bar);
        row.add_suffix(&pct);

        ui.apps.add(&row);
        rows.push(row);
    }
}

fn subtitle(a: &AppTotal, has_screen: bool) -> String {
    let mut parts = Vec::new();
    if a.usage.energy_j > 0.0 {
        parts.push(format!("{:.2} Wh", a.usage.energy_j / 3600.0));
    }
    if a.kind == AppKind::Idle {
        parts.push(
            if has_screen { "Wi-Fi, memory and idle hardware" } else { "screen, Wi-Fi and idle hardware" }.into(),
        );
    } else if a.kind == AppKind::Screen {
        parts.push("backlight".into());
    } else {
        if a.usage.cpu_s >= 1.0 {
            parts.push(format!("CPU {}", duration(a.usage.cpu_s)));
        }
        if a.usage.gpu_s >= 1.0 {
            parts.push(format!("GPU {}", duration(a.usage.gpu_s)));
        }
    }
    if let Some(kind) = match a.kind {
        AppKind::Command => Some("terminal command"),
        AppKind::Service => Some("background service"),
        AppKind::Desktop => Some("desktop"),
        AppKind::System => Some("system service"),
        _ => None,
    } {
        parts.push(kind.into());
    }
    parts.join(" · ")
}

fn icon(a: &AppTotal, theme: &gtk::IconTheme) -> gtk::Image {
    let fallback = match a.kind {
        AppKind::App => "application-x-executable-symbolic",
        AppKind::Command => "utilities-terminal-symbolic",
        AppKind::Service | AppKind::System => "emblem-system-symbolic",
        AppKind::Desktop => "user-desktop-symbolic",
        AppKind::Kernel => "computer-symbolic",
        AppKind::Idle => "power-profile-power-saver-symbolic",
        AppKind::Screen => "display-brightness-symbolic",
    };
    let image = match a.icon.as_deref() {
        Some(path) if path.starts_with('/') => gtk::Image::from_file(path),
        Some(name) if theme.has_icon(name) => gtk::Image::from_icon_name(name),
        _ => gtk::Image::from_icon_name(fallback),
    };
    image.set_pixel_size(32);
    image
}

fn duration(s: f64) -> String {
    let m = (s / 60.0).round() as i64;
    match m {
        0 => format!("{s:.0} s"),
        m if m < 60 => format!("{m} min"),
        m => format!("{} h {:02} min", m / 60, m % 60),
    }
}
