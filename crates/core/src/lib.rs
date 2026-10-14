//! Core of whatdrains: reads battery and per-process activity, attributes battery
//! drain to applications, and stores the history.

pub mod apps;
pub mod battery;
pub mod model;
pub mod procs;
pub mod store;

/// Resolves an [`apps::AppId`] to the label and icon shown to users.
pub fn display_info(db: &mut apps::DesktopDb, id: &apps::AppId) -> (String, Option<String>) {
    match id.desktop_id.as_deref().and_then(|d| db.lookup(d)) {
        Some(info) => (info.name, info.icon),
        None => (id.label.clone(), None),
    }
}
