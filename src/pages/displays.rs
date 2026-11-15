//! Displays: information about connected monitors, as reported by GDK.
//!
//! Changing resolution, arrangement or scale has no standard interface shared
//! by X11 and the various Wayland compositors, so this page is read-only.

use gtk::gdk;
use gtk::prelude::*;

use crate::sys;

fn fill(container: &gtk::Box, monitors: &gtk::gio::ListModel) {
    sys::clear(container);
    for i in 0..monitors.n_items() {
        let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
            continue;
        };
        let title = m
            .description()
            .or_else(|| m.connector())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("Monitor {}", i + 1));
        let list = sys::group(container, &title);
        let info =
            |label: &str, value: String| sys::row(&list, label, None, &sys::value_label(&value));

        if let Some(c) = m.connector() {
            info("Connector", c.to_string());
        }
        let maker = [m.manufacturer(), m.model()]
            .into_iter()
            .flatten()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if !maker.is_empty() {
            info("Model", maker);
        }
        let g = m.geometry();
        let scale = m.scale();
        info(
            "Resolution",
            format!(
                "{} × {} pixels",
                (g.width() as f64 * scale).round(),
                (g.height() as f64 * scale).round()
            ),
        );
        info("Scale", format!("{}%", (scale * 100.0).round()));
        info("Position", format!("{}, {}", g.x(), g.y()));
        if m.refresh_rate() > 0 {
            info(
                "Refresh rate",
                format!("{:.2} Hz", m.refresh_rate() as f64 / 1000.0),
            );
        }
        let (w, h) = (m.width_mm(), m.height_mm());
        if w > 0 && h > 0 {
            let inches = ((w * w + h * h) as f64).sqrt() / 25.4;
            info("Physical size", format!("{w} × {h} mm ({inches:.1}″)"));
        }
    }
    if monitors.n_items() == 0 {
        container.append(&sys::dim_label("No monitors were reported."));
    }
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Displays",
        "Connected monitors. Arranging monitors and changing resolution depend on the window manager or compositor, and are not available here yet.",
    );
    page.append(&sys::dim_label(&format!(
        "Windowing system: {}",
        super::about::session_type()
    )));

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.append(&container);
    if let Some(display) = gdk::Display::default() {
        let monitors = display.monitors();
        fill(&container, &monitors);
        let c = container.clone();
        monitors.connect_items_changed(move |model, _, _, _| fill(&c, model));
    }
    page.upcast()
}
