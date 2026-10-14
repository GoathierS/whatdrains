mod chart;
mod window;

use adw::prelude::*;
use gtk::glib;

pub const APP_ID: &str = "io.github.GoathierS.WhatDrains";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(window::build);
    app.run()
}
