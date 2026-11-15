//! Sound: default output/input device, volume and mute.
//!
//! Uses `pactl`, which talks to PulseAudio and to PipeWire (via
//! pipewire-pulse), so it works the same under any desktop.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::sys::{self, Choice, Toggle};

/// Either the output ("sink") or input ("source") side.
#[derive(Clone, Copy)]
struct Kind {
    name: &'static str, // "sink" or "source"
    default: &'static str,
}

const OUTPUT: Kind = Kind {
    name: "sink",
    default: "@DEFAULT_SINK@",
};
const INPUT: Kind = Kind {
    name: "source",
    default: "@DEFAULT_SOURCE@",
};

fn pactl(args: &[&str]) -> Result<String, String> {
    sys::run("pactl", args)
}

/// (name, description) of each device, from `pactl list sinks|sources`.
fn devices(kind: Kind) -> Vec<(String, String)> {
    let Ok(out) = pactl(&["list", &format!("{}s", kind.name)]) else {
        return vec![];
    };
    let mut devices = vec![];
    let mut name = None;
    for line in out.lines().map(str::trim) {
        if let Some(n) = line.strip_prefix("Name: ") {
            name = Some(n.to_string());
        } else if let Some(d) = line.strip_prefix("Description: ")
            && let Some(n) = name.take()
        {
            devices.push((n, d.to_string()));
        }
    }
    // Monitor sources just echo an output; they are not microphones.
    devices.retain(|(n, _)| !n.ends_with(".monitor"));
    devices
}

fn default_device(kind: Kind) -> String {
    pactl(&[&format!("get-default-{}", kind.name)])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Volume of the first channel, in percent.
fn volume(kind: Kind) -> Option<f64> {
    let out = pactl(&[&format!("get-{}-volume", kind.name), kind.default]).ok()?;
    out.split_whitespace()
        .find_map(|w| w.strip_suffix('%'))
        .and_then(|n| n.parse().ok())
}

fn muted(kind: Kind) -> bool {
    pactl(&[&format!("get-{}-mute", kind.name), kind.default])
        .map(|s| s.contains("yes"))
        .unwrap_or(false)
}

fn add_group(page: &gtk::Box, title: &str, kind: Kind) {
    let list = sys::group(page, title);
    let devs = devices(kind);
    if devs.is_empty() {
        sys::row(
            &list,
            "No devices found",
            None,
            &gtk::Box::new(gtk::Orientation::Horizontal, 0),
        );
        return;
    }

    let device = Choice::new(&devs, &default_device(kind));
    sys::row(&list, "Device", None, &device.dropdown);

    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    scale.set_width_request(260);
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    let scale_quiet = Rc::new(Cell::new(false));
    sys::row(&list, "Volume", None, &scale);

    let mute = Toggle::new(false);
    sys::row(&list, "Mute", None, &mute.switch);

    let refresh = {
        let (scale, scale_quiet, mute) = (scale.clone(), scale_quiet.clone(), mute.clone());
        move || {
            scale_quiet.set(true);
            scale.set_value(volume(kind).unwrap_or(0.0));
            scale_quiet.set(false);
            mute.set(muted(kind));
        }
    };
    refresh();

    {
        let q = scale_quiet.clone();
        scale.connect_value_changed(move |s| {
            if q.get() {
                return;
            }
            let pct = format!("{}%", s.value().round() as i64);
            if let Err(e) = pactl(&[&format!("set-{}-volume", kind.name), kind.default, &pct]) {
                eprintln!("settingsgoo: {e}");
            }
        });
    }
    mute.on_change(move |on| async move {
        pactl(&[
            &format!("set-{}-mute", kind.name),
            kind.default,
            if on { "1" } else { "0" },
        ])
        .map(drop)
    });
    device.on_change(move |name| {
        let refresh = refresh.clone();
        async move {
            pactl(&[&format!("set-default-{}", kind.name), &name])?;
            refresh();
            Ok(())
        }
    });
}

pub fn build() -> gtk::Widget {
    let page = sys::page("Sound", "Default devices and volume.");
    if !sys::have_program("pactl") || pactl(&["info"]).is_err() {
        page.append(&sys::dim_label(
            "No PulseAudio-compatible sound server was found (PulseAudio, or PipeWire with pipewire-pulse), or the pactl program is not installed.",
        ));
        return page.upcast();
    }
    add_group(&page, "Output", OUTPUT);
    add_group(&page, "Input", INPUT);
    page.upcast()
}
