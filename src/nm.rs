//! A small NetworkManager client using its D-Bus API directly
//! (`org.freedesktop.NetworkManager` on the system bus).
//!
//! Reads are synchronous: they go to a local service and are fast. Changes
//! are asynchronous and allow polkit to ask for authorization.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib::variant::ObjectPath;
use gtk::prelude::*;
use gtk::{gio, glib};

pub const SERVICE: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

pub const TYPE_ETHERNET: u32 = 1;
pub const TYPE_WIFI: u32 = 2;
const STATE_ACTIVATED: u32 = 100;

fn bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).ok()
}

fn call_sync(
    path: &str,
    iface: &str,
    method: &str,
    args: Option<glib::Variant>,
) -> Option<glib::Variant> {
    bus()?
        .call_sync(
            Some(SERVICE),
            path,
            iface,
            method,
            args.as_ref(),
            None,
            gio::DBusCallFlags::NONE,
            3000,
            None::<&gio::Cancellable>,
        )
        .ok()
}

async fn call(
    path: &str,
    iface: &str,
    method: &str,
    args: glib::Variant,
) -> Result<glib::Variant, String> {
    let bus = bus().ok_or("Cannot connect to the system bus")?;
    bus.call_future(
        Some(SERVICE),
        path,
        iface,
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
        -1,
    )
    .await
    .map_err(|mut e| {
        gio::DBusError::strip_remote_error(&mut e);
        e.message().to_string()
    })
}

fn object_path(p: &str) -> ObjectPath {
    ObjectPath::try_from(p.to_string())
        .unwrap_or_else(|_| ObjectPath::try_from("/".to_string()).unwrap())
}

/// A string-keyed dictionary of D-Bus values (`a{sv}`).
pub struct Dict(glib::VariantDict);

impl Dict {
    /// Wraps an `a{sv}` value. Anything else gives an empty dictionary, so
    /// unexpected data from the service can't crash the app.
    fn from(v: &glib::Variant) -> Self {
        let ok = v.type_() == glib::VariantTy::VARDICT;
        Dict(glib::VariantDict::new(ok.then_some(v)))
    }
    fn value(&self, key: &str) -> Option<glib::Variant> {
        self.0.lookup_value(key, None)
    }
    /// A string or object path.
    pub fn str(&self, key: &str) -> String {
        self.value(key)
            .and_then(|v| v.str().map(String::from))
            .unwrap_or_default()
    }
    pub fn u32(&self, key: &str) -> u32 {
        self.value(key).and_then(|v| v.get()).unwrap_or_default()
    }
    pub fn bool(&self, key: &str) -> bool {
        self.value(key).and_then(|v| v.get()).unwrap_or_default()
    }
    pub fn bytes(&self, key: &str) -> Vec<u8> {
        self.value(key).and_then(|v| v.get()).unwrap_or_default()
    }
    /// An array of object paths.
    pub fn paths(&self, key: &str) -> Vec<String> {
        self.value(key)
            .map(|v| v.iter().filter_map(|c| c.str().map(String::from)).collect())
            .unwrap_or_default()
    }
    /// An array of dictionaries (`aa{sv}`).
    fn dicts(&self, key: &str) -> Vec<Dict> {
        self.value(key)
            .map(|v| v.iter().map(|c| Dict::from(&c)).collect())
            .unwrap_or_default()
    }
}

/// One section of a connection's settings (`a{sa{sv}}`, as returned by
/// `GetSettings`), such as `802-11-wireless`.
fn settings_section(settings: &glib::Variant, name: &str) -> Option<Dict> {
    settings
        .iter()
        .find(|entry| entry.child_value(0).str() == Some(name))
        .map(|entry| Dict::from(&entry.child_value(1)))
}

/// All properties of one interface of an object.
fn props(path: &str, iface: &str) -> Option<Dict> {
    if path.is_empty() || path == "/" {
        return None;
    }
    let v = call_sync(path, PROPERTIES, "GetAll", Some((iface,).to_variant()))?;
    Some(Dict::from(&v.child_value(0)))
}

pub fn available() -> bool {
    props(ROOT, SERVICE).is_some()
}

// ---------------------------------------------------------------------------
// Devices

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub path: String,
    pub interface: String,
    pub kind: u32,
    pub state: u32,
    pub managed: bool,
    pub hw_address: String,
    /// Name of the active connection, if any.
    pub connection: Option<String>,
    /// Wired speed in Mbit/s, if known.
    pub speed: Option<u32>,
    pub ip: IpInfo,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IpInfo {
    pub ipv4: Vec<String>,
    pub gateway4: String,
    pub dns4: Vec<String>,
    pub ipv6: Vec<String>,
    pub gateway6: String,
}

impl Device {
    pub fn is_connected(&self) -> bool {
        self.state == STATE_ACTIVATED
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            TYPE_ETHERNET => "Ethernet",
            TYPE_WIFI => "Wi-Fi",
            5 => "Bluetooth",
            8 => "Mobile broadband",
            9 => "InfiniBand",
            10 => "Bond",
            11 => "VLAN",
            15 => "Team",
            29 => "WireGuard",
            _ => "Network",
        }
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            10 => "Not managed",
            20 => "Unavailable (cable unplugged?)",
            30 => "Disconnected",
            40..=90 => "Connecting…",
            100 => "Connected",
            110 => "Disconnecting…",
            120 => "Connection failed",
            _ => "Unknown",
        }
    }
}

fn addresses(cfg: &Dict) -> Vec<String> {
    cfg.dicts("AddressData")
        .iter()
        .map(|a| format!("{}/{}", a.str("address"), a.u32("prefix")))
        .collect()
}

fn ip_info(dev: &Dict) -> IpInfo {
    let mut ip = IpInfo::default();
    if let Some(c) = props(
        &dev.str("Ip4Config"),
        "org.freedesktop.NetworkManager.IP4Config",
    ) {
        ip.ipv4 = addresses(&c);
        ip.gateway4 = c.str("Gateway");
        ip.dns4 = c
            .dicts("NameserverData")
            .iter()
            .map(|d| d.str("address"))
            .collect();
    }
    if let Some(c) = props(
        &dev.str("Ip6Config"),
        "org.freedesktop.NetworkManager.IP6Config",
    ) {
        // Link-local addresses are not useful to show.
        ip.ipv6 = addresses(&c)
            .into_iter()
            .filter(|a| !a.starts_with("fe80:"))
            .collect();
        ip.gateway6 = c.str("Gateway");
    }
    ip
}

pub fn devices() -> Vec<Device> {
    let Some(nm) = props(ROOT, SERVICE) else {
        return vec![];
    };
    let mut devices: Vec<Device> = nm
        .paths("Devices")
        .into_iter()
        .filter_map(|path| {
            let d = props(&path, DEVICE)?;
            let connection = props(
                &d.str("ActiveConnection"),
                "org.freedesktop.NetworkManager.Connection.Active",
            )
            .map(|a| a.str("Id"));
            let speed = props(&path, "org.freedesktop.NetworkManager.Device.Wired")
                .map(|w| w.u32("Speed"))
                .filter(|&s| s > 0);
            Some(Device {
                interface: d.str("Interface"),
                kind: d.u32("DeviceType"),
                state: d.u32("State"),
                managed: d.bool("Managed"),
                hw_address: d.str("HwAddress"),
                connection,
                speed,
                ip: ip_info(&d),
                path,
            })
        })
        .collect();
    devices.sort_by(|a, b| a.interface.cmp(&b.interface));
    devices
}

/// Connects the device using the best saved connection for it.
pub async fn connect_device(dev: &str) -> Result<(), String> {
    let args = (object_path("/"), object_path(dev), object_path("/"));
    call(ROOT, SERVICE, "ActivateConnection", args.to_variant())
        .await
        .map(drop)
}

pub async fn disconnect_device(dev: &str) -> Result<(), String> {
    call(dev, DEVICE, "Disconnect", ().to_variant())
        .await
        .map(drop)
}

// ---------------------------------------------------------------------------
// Wi-Fi

pub struct Radio {
    pub enabled: bool,
    pub hardware_enabled: bool,
}

pub fn wifi_radio() -> Radio {
    let nm = props(ROOT, SERVICE);
    Radio {
        enabled: nm.as_ref().is_some_and(|d| d.bool("WirelessEnabled")),
        hardware_enabled: nm
            .as_ref()
            .is_some_and(|d| d.bool("WirelessHardwareEnabled")),
    }
}

pub async fn set_wifi_enabled(on: bool) -> Result<(), String> {
    let args = (SERVICE, "WirelessEnabled", on.to_variant());
    call(ROOT, PROPERTIES, "Set", args.to_variant())
        .await
        .map(drop)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Security {
    Open,
    /// Opportunistic Wireless Encryption: encrypted, but no password.
    Owe,
    Wep,
    Psk,
    Sae,
    Enterprise,
}

impl Security {
    pub fn name(self) -> &'static str {
        match self {
            Security::Open => "Open",
            Security::Owe => "Enhanced Open",
            Security::Wep => "WEP",
            Security::Psk => "WPA/WPA2",
            Security::Sae => "WPA3",
            Security::Enterprise => "Enterprise",
        }
    }

    pub fn needs_password(self) -> bool {
        matches!(self, Security::Psk | Security::Sae)
    }

    /// Whether this app can set up a new connection of this kind.
    pub fn supported(self) -> bool {
        !matches!(self, Security::Wep | Security::Enterprise)
    }

    fn from_flags(flags: u32, wpa: u32, rsn: u32) -> Self {
        const PSK: u32 = 0x100;
        const EAP: u32 = 0x200 | 0x2000;
        const SAE: u32 = 0x400;
        const OWE: u32 = 0x800 | 0x1000;
        let k = wpa | rsn;
        if k & EAP != 0 {
            Security::Enterprise
        } else if k & PSK != 0 {
            Security::Psk // also covers WPA2/WPA3 transition mode
        } else if k & SAE != 0 {
            Security::Sae
        } else if k & OWE != 0 {
            Security::Owe
        } else if flags & 1 != 0 {
            Security::Wep
        } else {
            Security::Open
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// Access point object path (the strongest one for this SSID).
    pub ap: String,
    /// Signal strength in percent.
    pub strength: u8,
    pub security: Security,
    /// Saved connection object path, if this network is known.
    pub saved: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WifiDevice {
    pub device: Device,
    pub networks: Vec<Network>,
    /// When the last scan finished (CLOCK_BOOTTIME milliseconds), or -1 if never.
    pub last_scan: i64,
}

impl Network {
    /// Signal strength as 1–4 bars, for the icon.
    pub fn bars(&self) -> u8 {
        match self.strength {
            0..=20 => 1,
            21..=40 => 2,
            41..=60 => 3,
            _ => 4,
        }
    }
}

/// Keeps one entry per network name (the connected access point, otherwise
/// the strongest) and sorts: connected first, then strongest signal, then name.
fn pick_networks(access_points: Vec<Network>) -> Vec<Network> {
    let mut best: HashMap<String, Network> = HashMap::new();
    for net in access_points {
        match best.get(&net.ssid) {
            Some(b) if b.active || (b.strength >= net.strength && !net.active) => {}
            _ => {
                best.insert(net.ssid.clone(), net);
            }
        }
    }
    let mut networks: Vec<Network> = best.into_values().collect();
    networks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.strength.cmp(&a.strength))
            .then(a.ssid.to_lowercase().cmp(&b.ssid.to_lowercase()))
    });
    networks
}

/// Saved Wi-Fi connections: SSID → connection object path.
fn saved_wifi() -> HashMap<String, String> {
    let mut saved = HashMap::new();
    let Some(list) = call_sync(
        SETTINGS,
        "org.freedesktop.NetworkManager.Settings",
        "ListConnections",
        None,
    ) else {
        return saved;
    };
    for path in list
        .child_value(0)
        .iter()
        .filter_map(|c| c.str().map(String::from))
    {
        let Some(s) = call_sync(
            &path,
            "org.freedesktop.NetworkManager.Settings.Connection",
            "GetSettings",
            None,
        ) else {
            continue;
        };
        if let Some(w) = settings_section(&s.child_value(0), "802-11-wireless") {
            let ssid = String::from_utf8_lossy(&w.bytes("ssid")).into_owned();
            if !ssid.is_empty() {
                saved.insert(ssid, path);
            }
        }
    }
    saved
}

pub fn wifi_devices() -> Vec<WifiDevice> {
    let wifi: Vec<Device> = devices()
        .into_iter()
        .filter(|d| d.kind == TYPE_WIFI)
        .collect();
    if wifi.is_empty() {
        return vec![];
    }
    let saved = saved_wifi();
    wifi.into_iter()
        .map(|device| {
            let w = props(&device.path, WIRELESS);
            let active_ap = w
                .as_ref()
                .map(|w| w.str("ActiveAccessPoint"))
                .unwrap_or_default();
            let last_scan = w
                .as_ref()
                .and_then(|w| w.value("LastScan"))
                .and_then(|v| v.get::<i64>())
                .unwrap_or(-1);
            let mut access_points = vec![];
            for ap in w.map(|w| w.paths("AccessPoints")).unwrap_or_default() {
                let Some(p) = props(&ap, "org.freedesktop.NetworkManager.AccessPoint") else {
                    continue;
                };
                let ssid = String::from_utf8_lossy(&p.bytes("Ssid")).into_owned();
                if ssid.is_empty() {
                    continue; // hidden network
                }
                access_points.push(Network {
                    strength: p.value("Strength").and_then(|v| v.get::<u8>()).unwrap_or(0),
                    security: Security::from_flags(
                        p.u32("Flags"),
                        p.u32("WpaFlags"),
                        p.u32("RsnFlags"),
                    ),
                    saved: saved.get(&ssid).cloned(),
                    active: ap == active_ap,
                    ssid,
                    ap,
                });
            }
            WifiDevice {
                device,
                networks: pick_networks(access_points),
                last_scan,
            }
        })
        .collect()
}

pub async fn request_scan(dev: &str) -> Result<(), String> {
    let opts: HashMap<String, glib::Variant> = HashMap::new();
    call(dev, WIRELESS, "RequestScan", (opts,).to_variant())
        .await
        .map(drop)
}

/// Connects to a network: with its saved connection if there is one,
/// otherwise by creating a new connection.
pub async fn connect_wifi(dev: &str, net: &Network, password: Option<&str>) -> Result<(), String> {
    if let Some(saved) = &net.saved {
        let args = (object_path(saved), object_path(dev), object_path(&net.ap));
        return call(ROOT, SERVICE, "ActivateConnection", args.to_variant())
            .await
            .map(drop);
    }
    let mut settings: HashMap<String, HashMap<String, glib::Variant>> = HashMap::new();
    let key_mgmt = match net.security {
        Security::Psk => Some("wpa-psk"),
        Security::Sae => Some("sae"),
        Security::Owe => Some("owe"),
        Security::Open => None,
        Security::Wep | Security::Enterprise => {
            return Err(format!(
                "{} networks are not supported yet.",
                net.security.name()
            ));
        }
    };
    if let Some(k) = key_mgmt {
        let mut sec = HashMap::new();
        sec.insert("key-mgmt".to_string(), k.to_variant());
        if let Some(pw) = password {
            sec.insert("psk".to_string(), pw.to_variant());
        }
        settings.insert("802-11-wireless-security".to_string(), sec);
    }
    let args = (settings, object_path(dev), object_path(&net.ap));
    call(ROOT, SERVICE, "AddAndActivateConnection", args.to_variant())
        .await
        .map(drop)
}

/// Deletes a saved connection.
pub async fn forget(connection: &str) -> Result<(), String> {
    call(
        connection,
        "org.freedesktop.NetworkManager.Settings.Connection",
        "Delete",
        ().to_variant(),
    )
    .await
    .map(drop)
}

/// A WPA passphrase is 8–63 characters, or exactly 64 hex digits.
pub fn valid_psk(pw: &str) -> bool {
    let n = pw.chars().count();
    (8..=63).contains(&n) || (pw.len() == 64 && pw.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// Change notification

/// Calls `f` (at most about once a second) whenever any NetworkManager object
/// changes. The subscription lasts for the life of the program.
pub fn watch(f: impl Fn() + 'static) {
    let Some(bus) = bus() else { return };
    let pending = Rc::new(Cell::new(false));
    let f = Rc::new(f);
    let sub = bus.subscribe_to_signal(
        Some(SERVICE),
        None,
        None,
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |_| {
            if pending.replace(true) {
                return;
            }
            let (pending, f) = (pending.clone(), f.clone());
            glib::timeout_add_local_once(Duration::from_millis(800), move || {
                pending.set(false);
                f();
            });
        },
    );
    // Pages live as long as the app, so keep the subscription forever.
    std::mem::forget(sub);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_from_flags() {
        assert_eq!(Security::from_flags(0, 0, 0), Security::Open);
        assert_eq!(Security::from_flags(1, 0, 0), Security::Wep);
        assert_eq!(Security::from_flags(1, 0x100, 0x100), Security::Psk);
        assert_eq!(Security::from_flags(1, 0, 0x400), Security::Sae);
        // WPA2/WPA3 transition mode connects with a PSK.
        assert_eq!(Security::from_flags(1, 0, 0x500), Security::Psk);
        assert_eq!(Security::from_flags(1, 0, 0x200), Security::Enterprise);
        assert_eq!(Security::from_flags(0, 0, 0x800), Security::Owe);
    }

    /// Reads every saved connection's settings from the running
    /// NetworkManager, if there is one. This used to panic.
    #[test]
    fn read_saved_connections() {
        let _ = saved_wifi();
    }

    #[test]
    fn settings_sections() {
        let mut wifi: HashMap<String, glib::Variant> = HashMap::new();
        wifi.insert("ssid".into(), b"Home".to_vec().to_variant());
        let mut all: HashMap<String, HashMap<String, glib::Variant>> = HashMap::new();
        all.insert("802-11-wireless".into(), wifi);
        let v = all.to_variant();
        let w = settings_section(&v, "802-11-wireless").unwrap();
        assert_eq!(w.bytes("ssid"), b"Home");
        assert!(settings_section(&v, "ipv4").is_none());
        // The wrong type gives an empty dictionary instead of a panic.
        assert_eq!(Dict::from(&v).str("anything"), "");
    }

    fn ap(ssid: &str, strength: u8, active: bool) -> Network {
        Network {
            ssid: ssid.into(),
            ap: format!("/ap/{ssid}/{strength}"),
            strength,
            security: Security::Psk,
            saved: None,
            active,
        }
    }

    #[test]
    fn networks_sorted_connected_then_strongest() {
        let nets = pick_networks(vec![
            ap("weak", 20, false),
            ap("strong", 90, false),
            ap("home", 40, true),
            ap("strong", 50, false), // second, weaker AP of the same network
            ap("mid", 60, false),
        ]);
        let order: Vec<(&str, u8)> = nets.iter().map(|n| (n.ssid.as_str(), n.strength)).collect();
        assert_eq!(
            order,
            [("home", 40), ("strong", 90), ("mid", 60), ("weak", 20)]
        );
    }

    #[test]
    fn connected_access_point_wins_over_stronger_one() {
        let nets = pick_networks(vec![ap("home", 90, false), ap("home", 30, true)]);
        assert_eq!(nets.len(), 1);
        assert!(nets[0].active);
        assert_eq!(nets[0].strength, 30);
    }

    #[test]
    fn psk_validation() {
        assert!(!valid_psk("short"));
        assert!(valid_psk("eightchr"));
        assert!(valid_psk(&"x".repeat(63)));
        assert!(!valid_psk(&"x".repeat(64)));
        assert!(valid_psk(&"a1".repeat(32)));
        assert!(!valid_psk(&"x".repeat(65)));
    }
}
