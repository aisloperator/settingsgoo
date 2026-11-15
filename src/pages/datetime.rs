//! Date & Time, via systemd-timedated (`org.freedesktop.timedate1`).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::sys::{self, Choice, Toggle};

const SERVICE: &str = "org.freedesktop.timedate1";

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Date & Time",
        "System clock settings. Changing them may ask for an administrator password.",
    );
    let Some(proxy) = sys::system_proxy(SERVICE) else {
        page.append(&sys::dim_label(
            "The date and time service (systemd-timedated) is not available.",
        ));
        return page.upcast();
    };

    let list = sys::group(&page, "Clock");

    let zone = Rc::new(RefCell::new(
        sys::prop_str(&proxy, "Timezone").unwrap_or_else(|| "UTC".into()),
    ));

    let now = sys::value_label("");
    let update_now = {
        let (now, zone) = (now.downgrade(), zone.clone());
        move || {
            let Some(now) = now.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let tz = glib::TimeZone::new(Some(zone.borrow().as_str()));
            if let Some(text) = glib::DateTime::now(&tz)
                .ok()
                .and_then(|t| t.format("%c %Z").ok())
            {
                now.set_label(&text);
            }
            glib::ControlFlow::Continue
        }
    };
    update_now();
    glib::timeout_add_seconds_local(1, update_now);
    sys::row(&list, "Current time", None, &now);

    let tz_choice = Choice::new(&[], &zone.borrow());
    {
        let (proxy, zone) = (proxy.clone(), zone.clone());
        tz_choice.on_change(move |tz| {
            let (proxy, zone) = (proxy.clone(), zone.clone());
            async move {
                sys::call(&proxy, "SetTimezone", (tz.as_str(), true).to_variant()).await?;
                *zone.borrow_mut() = tz;
                Ok(())
            }
        });
    }
    {
        // The list of zones can be long; load it without blocking startup.
        let (proxy, tz_choice, zone) = (proxy.clone(), tz_choice.clone(), zone.clone());
        glib::spawn_future_local(async move {
            if let Ok(v) = sys::call(&proxy, "ListTimezones", ().to_variant()).await
                && let Some((zones,)) = v.get::<(Vec<String>,)>()
            {
                tz_choice.set_items(&sys::same(&zones), &zone.borrow());
            }
        });
    }
    sys::row(&list, "Time zone", None, &tz_choice.dropdown);

    let ntp = Toggle::new(sys::prop_bool(&proxy, "NTP").unwrap_or(false));
    ntp.switch
        .set_sensitive(sys::prop_bool(&proxy, "CanNTP").unwrap_or(false));
    {
        let proxy = proxy.clone();
        ntp.on_change(move |on| {
            let proxy = proxy.clone();
            async move {
                sys::call(&proxy, "SetNTP", (on, true).to_variant())
                    .await
                    .map(drop)
            }
        });
    }
    let synced = if sys::prop_bool(&proxy, "NTPSynchronized").unwrap_or(false) {
        "Set the time from the network. The clock is currently synchronized."
    } else {
        "Set the time from the network. The clock is not currently synchronized."
    };
    sys::row(&list, "Automatic time", Some(synced), &ntp.switch);

    let list = sys::group(&page, "Hardware clock");
    let rtc = Toggle::new(sys::prop_bool(&proxy, "LocalRTC").unwrap_or(false));
    rtc.on_change(move |local| {
        let proxy = proxy.clone();
        async move {
            sys::call(&proxy, "SetLocalRTC", (local, false, true).to_variant())
                .await
                .map(drop)
        }
    });
    sys::row(
        &list,
        "Hardware clock uses local time",
        Some(
            "Usually off. Turn on only when sharing the computer with an operating system that expects local time.",
        ),
        &rtc.switch,
    );

    page.upcast()
}
