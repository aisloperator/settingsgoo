//! The settings pages shown in the sidebar.

mod about;
mod appearance;
mod bluetooth;
mod datetime;
mod displays;
pub mod network;
mod region;
mod sound;
mod users;
mod wifi;

pub struct Page {
    pub id: &'static str,
    pub title: &'static str,
    pub widget: gtk::Widget,
}

pub fn all() -> Vec<Page> {
    vec![
        Page {
            id: "wifi",
            title: "Wi-Fi",
            widget: wifi::build(),
        },
        Page {
            id: "network",
            title: "Network",
            widget: network::build(),
        },
        Page {
            id: "bluetooth",
            title: "Bluetooth",
            widget: bluetooth::build(),
        },
        Page {
            id: "appearance",
            title: "Appearance",
            widget: appearance::build(),
        },
        Page {
            id: "displays",
            title: "Displays",
            widget: displays::build(),
        },
        Page {
            id: "sound",
            title: "Sound",
            widget: sound::build(),
        },
        Page {
            id: "region",
            title: "Region & Keyboard",
            widget: region::build(),
        },
        Page {
            id: "datetime",
            title: "Date & Time",
            widget: datetime::build(),
        },
        Page {
            id: "users",
            title: "Users",
            widget: users::build(),
        },
        Page {
            id: "about",
            title: "About",
            widget: about::build(),
        },
    ]
}
