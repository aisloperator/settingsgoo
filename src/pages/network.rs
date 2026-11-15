//! Network: wired and other non-Wi-Fi connections, via NetworkManager.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::nm::{self, Device};
use crate::sys;

/// Device types shown here. Wi-Fi has its own page; bridges, veth, tun and
/// loopback devices are usually created by other software and only clutter.
const SHOWN: [u32; 8] = [nm::TYPE_ETHERNET, 5, 8, 9, 10, 11, 15, 29];

/// Address, gateway, DNS, speed and hardware-address rows for a device.
pub fn detail_rows(list: &gtk::ListBox, dev: &Device) {
    let info = |title: &str, value: String| {
        if !value.is_empty() {
            sys::row(list, title, None, &sys::value_label(&value));
        }
    };
    info("IPv4 address", dev.ip.ipv4.join("\n"));
    info("Gateway", dev.ip.gateway4.clone());
    info("DNS", dev.ip.dns4.join("\n"));
    info("IPv6 address", dev.ip.ipv6.join("\n"));
    info("IPv6 gateway", dev.ip.gateway6.clone());
    info(
        "Speed",
        dev.speed.map(|s| format!("{s} Mb/s")).unwrap_or_default(),
    );
    info("Hardware address", dev.hw_address.clone());
}

/// A Connect or Disconnect button for a device.
fn connect_button(dev: &Device) -> gtk::Button {
    let connected = dev.is_connected();
    let button = gtk::Button::with_label(if connected { "Disconnect" } else { "Connect" });
    // 20 = unavailable (e.g. no cable); 10 = not managed by NetworkManager.
    button.set_sensitive(dev.managed && dev.state > 20);
    let path = dev.path.clone();
    button.connect_clicked(move |b| {
        let (b, path) = (b.clone(), path.clone());
        glib::spawn_future_local(async move {
            let result = if connected {
                nm::disconnect_device(&path).await
            } else {
                nm::connect_device(&path).await
            };
            if let Err(e) = result {
                sys::show_error(&b, "Could not change the connection", &e);
            }
        });
    });
    button
}

fn render(container: &gtk::Box, devices: &[Device]) {
    sys::clear(container);
    if devices.is_empty() {
        container.append(&sys::dim_label("No wired network devices were found."));
        return;
    }
    for dev in devices {
        let list = sys::group(
            container,
            &format!("{} ({})", dev.kind_name(), dev.interface),
        );
        let status = match (&dev.connection, dev.is_connected()) {
            (Some(name), true) => format!("Connected — {name}"),
            _ => dev.state_name().to_string(),
        };
        sys::row(&list, "Status", Some(&status), &connect_button(dev));
        detail_rows(&list, dev);
    }
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Network",
        "Wired and other network connections, managed by NetworkManager.",
    );
    if !nm::available() {
        page.append(&sys::dim_label("NetworkManager is not running."));
        return page.upcast();
    }
    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.append(&container);

    let shown: Rc<RefCell<Option<Vec<Device>>>> = Rc::default();
    let refresh = move || {
        let devices: Vec<Device> = nm::devices()
            .into_iter()
            .filter(|d| SHOWN.contains(&d.kind))
            .collect();
        // Rebuild only when something visible changed.
        if shown.borrow().as_ref() != Some(&devices) {
            render(&container, &devices);
            *shown.borrow_mut() = Some(devices);
        }
    };
    refresh();
    nm::watch(refresh);
    page.upcast()
}
