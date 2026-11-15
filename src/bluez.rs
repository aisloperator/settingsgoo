//! A small BlueZ client using its D-Bus API directly (`org.bluez` on the
//! system bus), plus a pairing agent.
//!
//! Reads are synchronous; changes are asynchronous.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib::variant::ObjectPath;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::sys::{self, Dict};

pub const SERVICE: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";
const AGENT: &str = "org.bluez.Agent1";
const AGENT_MANAGER: &str = "org.bluez.AgentManager1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const AGENT_PATH: &str = "/com/aisloperator/settingsgoo/agent";

/// Pairing and connecting can take a while (the user may have to type a code
/// on the other device), so don't give up after D-Bus's default 25 seconds.
const LONG_TIMEOUT_MS: i32 = 120_000;

fn bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).ok()
}

async fn call(
    path: &str,
    iface: &str,
    method: &str,
    args: glib::Variant,
    timeout_ms: i32,
) -> Result<glib::Variant, String> {
    let bus = bus().ok_or("Cannot connect to the system bus")?;
    bus.call_future(
        Some(SERVICE),
        path,
        iface,
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        timeout_ms,
    )
    .await
    .map_err(|mut e| {
        gio::DBusError::strip_remote_error(&mut e);
        friendly_error(e.message())
    })
}

/// BlueZ's error messages are terse ("Page Timeout", "br-connection-…");
/// replace the common ones with something a person can act on.
fn friendly_error(msg: &str) -> String {
    let m = msg.to_lowercase();
    let hint = if m.contains("page timeout") || m.contains("timeout") || m.contains("timed out") {
        "The device did not respond. Make sure it is turned on, nearby, and (for pairing) in pairing mode."
    } else if m.contains("authentication") || m.contains("rejected") || m.contains("canceled") {
        "Pairing was refused or canceled."
    } else if m.contains("profile-unavailable") || m.contains("profile unavailable") {
        "This computer does not support any of the device's connection types (for audio devices, check that PipeWire or PulseAudio Bluetooth support is installed)."
    } else if m.contains("blocked") || m.contains("rfkill") {
        "Bluetooth is blocked by a hardware or software switch (rfkill)."
    } else if m.contains("not ready") {
        "Bluetooth is turned off."
    } else {
        return msg.to_string();
    };
    format!("{hint}\n\n({msg})")
}

fn object_path(p: &str) -> ObjectPath {
    ObjectPath::try_from(p.to_string())
        .unwrap_or_else(|_| ObjectPath::try_from("/".to_string()).unwrap())
}

/// `GetManagedObjects`: every BlueZ object with its interfaces' properties.
fn managed_objects() -> Option<glib::Variant> {
    let v = bus()?
        .call_sync(
            Some(SERVICE),
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            None,
            None,
            gio::DBusCallFlags::NONE,
            3000,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    Some(v.child_value(0))
}

// ---------------------------------------------------------------------------
// Adapters and devices

#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    pub path: String,
    pub address: String,
    /// The name other devices see.
    pub alias: String,
    pub powered: bool,
    /// BlueZ 5.66+: "on", "off", "off-enabling", "on-disabling" or "off-blocked".
    pub power_state: String,
    pub discoverable: bool,
    /// Seconds that "discoverable" stays on (0 = until turned off).
    pub discoverable_timeout: u32,
    pub discovering: bool,
}

impl Adapter {
    /// Blocked by rfkill (a hardware switch or `rfkill block`).
    pub fn blocked(&self) -> bool {
        self.power_state == "off-blocked"
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub path: String,
    pub adapter: String,
    pub address: String,
    /// The name the device reports, if it has sent one.
    pub name: Option<String>,
    /// The name to show: set by the user, otherwise the device's own name,
    /// otherwise its address.
    pub alias: String,
    /// A freedesktop icon name describing the kind of device, e.g. `audio-headset`.
    pub icon: String,
    pub paired: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub connected: bool,
    /// Signal strength as 1–4 bars while the device is being discovered.
    pub signal: Option<u8>,
    /// Battery level in percent, if the device reports it.
    pub battery: Option<u8>,
}

impl Device {
    /// A short description of the kind of device.
    pub fn kind_name(&self) -> Option<&'static str> {
        Some(match self.icon.as_str() {
            "audio-headset" => "Headset",
            "audio-headphones" => "Headphones",
            "audio-card" | "audio-speakers" => "Speaker",
            "input-keyboard" => "Keyboard",
            "input-mouse" => "Mouse",
            "input-tablet" => "Tablet",
            "input-gaming" => "Game controller",
            "phone" => "Phone",
            "computer" => "Computer",
            "printer" => "Printer",
            "camera-photo" => "Camera",
            "camera-video" => "Video camera",
            "modem" => "Modem",
            "network-wireless" => "Network",
            "multimedia-player" => "Media player",
            "video-display" => "Display",
            _ => return None,
        })
    }
}

/// Received signal strength (dBm) as 1–4 bars.
pub fn bars(rssi: i16) -> u8 {
    match rssi {
        -60.. => 4,
        -70..=-61 => 3,
        -80..=-71 => 2,
        _ => 1,
    }
}

pub struct Objects {
    pub adapters: Vec<Adapter>,
    pub devices: Vec<Device>,
}

/// Reads all adapters and devices from the result of `GetManagedObjects`.
fn parse_objects(objects: &glib::Variant) -> Objects {
    let mut adapters = vec![];
    let mut devices = vec![];
    if !objects.is_container() {
        return Objects { adapters, devices };
    }
    for entry in objects.iter() {
        if entry.n_children() != 2 {
            continue;
        }
        let Some(path) = entry.child_value(0).str().map(String::from) else {
            continue;
        };
        let ifaces = entry.child_value(1);
        if let Some(a) = sys::section(&ifaces, ADAPTER) {
            adapters.push(Adapter {
                address: a.str("Address"),
                alias: a.str("Alias"),
                powered: a.bool("Powered"),
                power_state: a.str("PowerState"),
                discoverable: a.bool("Discoverable"),
                discoverable_timeout: a.u32("DiscoverableTimeout"),
                discovering: a.bool("Discovering"),
                path: path.clone(),
            });
        }
        if let Some(d) = sys::section(&ifaces, DEVICE) {
            let battery = sys::section(&ifaces, BATTERY).and_then(|b| b.get::<u8>("Percentage"));
            devices.push(device(path, &d, battery));
        }
    }
    adapters.sort_by(|a, b| a.path.cmp(&b.path));
    devices.sort_by_key(|d| (d.alias.to_lowercase(), d.address.clone()));
    Objects { adapters, devices }
}

fn device(path: String, d: &Dict, battery: Option<u8>) -> Device {
    let address = d.str("Address");
    let name = Some(d.str("Name")).filter(|n| !n.trim().is_empty());
    let alias = Some(d.str("Alias"))
        .filter(|a| !a.trim().is_empty())
        .or_else(|| name.clone())
        .unwrap_or_else(|| address.clone());
    Device {
        adapter: d.str("Adapter"),
        name,
        alias,
        icon: d.str("Icon"),
        paired: d.bool("Paired"),
        trusted: d.bool("Trusted"),
        blocked: d.bool("Blocked"),
        connected: d.bool("Connected"),
        signal: d.get::<i16>("RSSI").map(bars),
        battery,
        address,
        path,
    }
}

/// All adapters and devices, or `None` if bluetoothd isn't running.
pub fn objects() -> Option<Objects> {
    managed_objects().map(|v| parse_objects(&v))
}

async fn set_property(
    path: &str,
    iface: &str,
    name: &str,
    value: glib::Variant,
) -> Result<(), String> {
    let args = (iface, name, value);
    call(path, PROPERTIES, "Set", args.to_variant(), -1)
        .await
        .map(drop)
}

pub async fn set_powered(adapter: &str, on: bool) -> Result<(), String> {
    set_property(adapter, ADAPTER, "Powered", on.to_variant()).await
}

pub async fn set_discoverable(adapter: &str, on: bool) -> Result<(), String> {
    set_property(adapter, ADAPTER, "Discoverable", on.to_variant()).await
}

pub async fn set_alias(adapter: &str, alias: &str) -> Result<(), String> {
    set_property(adapter, ADAPTER, "Alias", alias.to_variant()).await
}

pub async fn set_trusted(device: &str, on: bool) -> Result<(), String> {
    set_property(device, DEVICE, "Trusted", on.to_variant()).await
}

pub async fn start_discovery(adapter: &str) -> Result<(), String> {
    call(adapter, ADAPTER, "StartDiscovery", ().to_variant(), -1)
        .await
        .map(drop)
}

pub async fn stop_discovery(adapter: &str) -> Result<(), String> {
    call(adapter, ADAPTER, "StopDiscovery", ().to_variant(), -1)
        .await
        .map(drop)
}

pub async fn pair(device: &str) -> Result<(), String> {
    call(device, DEVICE, "Pair", ().to_variant(), LONG_TIMEOUT_MS)
        .await
        .map(drop)
}

pub async fn cancel_pairing(device: &str) -> Result<(), String> {
    call(device, DEVICE, "CancelPairing", ().to_variant(), -1)
        .await
        .map(drop)
}

pub async fn connect(device: &str) -> Result<(), String> {
    call(device, DEVICE, "Connect", ().to_variant(), LONG_TIMEOUT_MS)
        .await
        .map(drop)
}

pub async fn disconnect(device: &str) -> Result<(), String> {
    call(device, DEVICE, "Disconnect", ().to_variant(), -1)
        .await
        .map(drop)
}

/// Unpairs a device and removes it from the adapter's list.
pub async fn remove(adapter: &str, device: &str) -> Result<(), String> {
    let args = (object_path(device),);
    call(adapter, ADAPTER, "RemoveDevice", args.to_variant(), -1)
        .await
        .map(drop)
}

// ---------------------------------------------------------------------------
// Change notification

/// Calls `f` (at most about once a second) whenever a BlueZ object changes,
/// and right away when bluetoothd starts or stops.
pub fn watch(f: impl Fn() + 'static) {
    let f = Rc::new(f);
    {
        let f = f.clone();
        sys::watch_system_service(SERVICE, move || f());
    }
    let Some(bus) = bus() else { return };
    let (f1, f2) = (f.clone(), f);
    // The watch lasts for the life of the program (dropping the id doesn't end it).
    gio::bus_watch_name_on_connection(
        &bus,
        SERVICE,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| f1(),
        move |_, _| f2(),
    );
}

// ---------------------------------------------------------------------------
// Pairing agent
//
// BlueZ asks an "agent" to show or ask for pairing codes. Desktops normally
// provide one; this app registers its own and asks to be the default, so
// pairing works on any desktop. BlueZ goes back to the previous agent when
// the app exits.

/// Something BlueZ wants the user to see or decide.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Type a PIN (legacy devices). Reply with [`Reply::pin`].
    PinCode { device: String },
    /// Type a 6-digit passkey. Reply with [`Reply::passkey`].
    Passkey { device: String },
    /// Show a PIN to type on the device. No reply needed.
    DisplayPinCode { device: String, pin: String },
    /// Show a passkey to type on the device; `entered` digits have been typed
    /// so far. Sent again as each digit is typed. No reply needed.
    DisplayPasskey {
        device: String,
        passkey: u32,
        entered: u16,
    },
    /// Confirm that both devices show the same passkey.
    Confirm { device: String, passkey: u32 },
    /// Allow a device to pair that needs no code.
    Authorize { device: String },
    /// Allow a device to use a service (UUID).
    AuthorizeService { device: String, uuid: String },
    /// BlueZ gave up on the current request; close any prompt.
    Cancel,
}

/// The answer to an agent request. Dropping it without answering refuses.
pub struct Reply(Option<gio::DBusMethodInvocation>);

impl Reply {
    pub fn accept(mut self) {
        if let Some(inv) = self.0.take() {
            inv.return_value(None);
        }
    }
    pub fn pin(mut self, pin: &str) {
        if let Some(inv) = self.0.take() {
            inv.return_value(Some(&(pin,).to_variant()));
        }
    }
    pub fn passkey(mut self, passkey: u32) {
        if let Some(inv) = self.0.take() {
            inv.return_value(Some(&(passkey,).to_variant()));
        }
    }
    fn refuse(&mut self) {
        if let Some(inv) = self.0.take() {
            inv.return_dbus_error("org.bluez.Error.Rejected", "Refused by the user");
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.refuse();
    }
}

const AGENT_XML: &str = r#"
<node>
  <interface name="org.bluez.Agent1">
    <method name="Release"/>
    <method name="RequestPinCode">
      <arg type="o" direction="in"/><arg type="s" direction="out"/>
    </method>
    <method name="DisplayPinCode">
      <arg type="o" direction="in"/><arg type="s" direction="in"/>
    </method>
    <method name="RequestPasskey">
      <arg type="o" direction="in"/><arg type="u" direction="out"/>
    </method>
    <method name="DisplayPasskey">
      <arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="q" direction="in"/>
    </method>
    <method name="RequestConfirmation">
      <arg type="o" direction="in"/><arg type="u" direction="in"/>
    </method>
    <method name="RequestAuthorization">
      <arg type="o" direction="in"/>
    </method>
    <method name="AuthorizeService">
      <arg type="o" direction="in"/><arg type="s" direction="in"/>
    </method>
    <method name="Cancel"/>
  </interface>
</node>
"#;

/// Turns an agent method call into a [`Request`]. `None` for calls that need
/// no user involvement (`Release`) or that are malformed.
fn parse_request(method: &str, params: &glib::Variant) -> Option<Request> {
    let s = |i: usize| -> Option<String> {
        (params.n_children() > i)
            .then(|| params.child_value(i).str().map(String::from))
            .flatten()
    };
    let device = || s(0);
    Some(match method {
        "RequestPinCode" => Request::PinCode { device: device()? },
        "RequestPasskey" => Request::Passkey { device: device()? },
        "DisplayPinCode" => Request::DisplayPinCode {
            device: device()?,
            pin: s(1)?,
        },
        "DisplayPasskey" => {
            let (device, passkey, entered) = params.get::<(ObjectPath, u32, u16)>()?;
            Request::DisplayPasskey {
                device: device.as_str().to_string(),
                passkey,
                entered,
            }
        }
        "RequestConfirmation" => {
            let (device, passkey) = params.get::<(ObjectPath, u32)>()?;
            Request::Confirm {
                device: device.as_str().to_string(),
                passkey,
            }
        }
        "RequestAuthorization" => Request::Authorize { device: device()? },
        "AuthorizeService" => Request::AuthorizeService {
            device: device()?,
            uuid: s(1)?,
        },
        "Cancel" => Request::Cancel,
        _ => return None,
    })
}

/// The unique bus name of bluetoothd, to check that agent calls come from it.
fn service_owner(bus: &gio::DBusConnection) -> Option<String> {
    let v = bus
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&(SERVICE,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    v.child_value(0).str().map(String::from)
}

type Handler = Rc<dyn Fn(Request, Reply)>;

/// Exports the agent and registers it with bluetoothd, now and whenever
/// bluetoothd restarts. `handler` is called for each request.
pub fn start_agent(handler: impl Fn(Request, Reply) + 'static) {
    let Some(bus) = bus() else { return };
    let Ok(info) = gio::DBusNodeInfo::for_xml(AGENT_XML) else {
        return;
    };
    let Some(iface) = info.lookup_interface(AGENT) else {
        return;
    };
    let handler: Handler = Rc::new(handler);
    let owner: Rc<RefCell<Option<String>>> = Rc::default();
    let registration = {
        let owner = owner.clone();
        bus.register_object(AGENT_PATH, &iface)
            .method_call(move |bus, sender, _path, _iface, method, params, inv| {
                // Only bluetoothd may talk to the agent.
                let expected = owner.borrow().clone().or_else(|| service_owner(&bus));
                if sender.is_none() || sender.map(String::from) != expected {
                    inv.return_dbus_error("org.bluez.Error.Rejected", "Not from bluetoothd");
                    return;
                }
                match parse_request(method, &params) {
                    Some(req) => {
                        let no_reply = matches!(
                            req,
                            Request::DisplayPinCode { .. }
                                | Request::DisplayPasskey { .. }
                                | Request::Cancel
                        );
                        if no_reply {
                            inv.return_value(None);
                            handler(req, Reply(None));
                        } else {
                            handler(req, Reply(Some(inv)));
                        }
                    }
                    None => inv.return_value(None),
                }
            })
            .build()
    };
    // The object stays exported for the life of the program.
    if registration.is_err() {
        eprintln!("settingsgoo: could not export the Bluetooth pairing agent");
        return;
    }

    gio::bus_watch_name_on_connection(
        &bus,
        SERVICE,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, unique| {
            *owner.borrow_mut() = Some(unique.to_string());
            glib::spawn_future_local(register_agent());
        },
        |_, _| {},
    );
}

async fn register_agent() {
    let path = object_path(AGENT_PATH);
    // "KeyboardDisplay": this computer can both show and type codes.
    let args = (path.clone(), "KeyboardDisplay");
    if let Err(e) = call(
        "/org/bluez",
        AGENT_MANAGER,
        "RegisterAgent",
        args.to_variant(),
        -1,
    )
    .await
    {
        eprintln!("settingsgoo: Bluetooth agent: {e}");
        return;
    }
    if let Err(e) = call(
        "/org/bluez",
        AGENT_MANAGER,
        "RequestDefaultAgent",
        (path,).to_variant(),
        -1,
    )
    .await
    {
        eprintln!("settingsgoo: Bluetooth agent: {e}");
    }
}

/// A readable name for a few common Bluetooth service UUIDs.
pub fn service_name(uuid: &str) -> Option<&'static str> {
    let short = uuid.get(4..8)?.to_lowercase();
    if !uuid
        .to_lowercase()
        .ends_with("-0000-1000-8000-00805f9b34fb")
    {
        return None;
    }
    let names: HashMap<&str, &str> = [
        ("1101", "serial port"),
        ("1105", "file transfer (OBEX Push)"),
        ("1106", "file transfer (OBEX FTP)"),
        ("1108", "headset audio"),
        ("110a", "audio playback (source)"),
        ("110b", "audio playback (sink)"),
        ("110c", "remote control"),
        ("110e", "remote control"),
        ("1112", "headset audio"),
        ("1115", "network access (PANU)"),
        ("1116", "network access (NAP)"),
        ("111e", "hands-free calls"),
        ("111f", "hands-free calls"),
        ("1124", "keyboard, mouse or other input"),
        ("112f", "phone book access"),
        ("1132", "message access"),
        ("1812", "keyboard, mouse or other input"),
    ]
    .into_iter()
    .collect();
    names.get(short.as_str()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, glib::Variant)]) -> HashMap<String, glib::Variant> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// Builds a `GetManagedObjects` result like bluetoothd's.
    fn sample() -> glib::Variant {
        let mut objs: HashMap<ObjectPath, HashMap<String, HashMap<String, glib::Variant>>> =
            HashMap::new();
        objs.insert(
            object_path("/org/bluez"),
            [(AGENT_MANAGER.to_string(), HashMap::new())]
                .into_iter()
                .collect(),
        );
        objs.insert(
            object_path("/org/bluez/hci0"),
            [(
                ADAPTER.to_string(),
                dict(&[
                    ("Address", "00:11:22:33:44:55".to_variant()),
                    ("Alias", "laptop".to_variant()),
                    ("Powered", true.to_variant()),
                    ("PowerState", "on".to_variant()),
                    ("Discovering", false.to_variant()),
                    ("DiscoverableTimeout", 180u32.to_variant()),
                ]),
            )]
            .into_iter()
            .collect(),
        );
        objs.insert(
            object_path("/org/bluez/hci0/dev_AA"),
            [
                (
                    DEVICE.to_string(),
                    dict(&[
                        ("Address", "AA:AA:AA:AA:AA:AA".to_variant()),
                        ("Name", "Headphones".to_variant()),
                        ("Alias", "My Headphones".to_variant()),
                        ("Icon", "audio-headphones".to_variant()),
                        ("Paired", true.to_variant()),
                        ("Connected", true.to_variant()),
                        ("Adapter", object_path("/org/bluez/hci0").to_variant()),
                    ]),
                ),
                (
                    BATTERY.to_string(),
                    dict(&[("Percentage", 80u8.to_variant())]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        objs.insert(
            object_path("/org/bluez/hci0/dev_BB"),
            [(
                DEVICE.to_string(),
                dict(&[
                    ("Address", "BB:BB:BB:BB:BB:BB".to_variant()),
                    ("Alias", "BB-BB-BB-BB-BB-BB".to_variant()),
                    ("RSSI", (-75i16).to_variant()),
                    // A property of an unexpected type is ignored.
                    ("Paired", "yes".to_variant()),
                ]),
            )]
            .into_iter()
            .collect(),
        );
        objs.to_variant()
    }

    #[test]
    fn parses_managed_objects() {
        let o = parse_objects(&sample());
        assert_eq!(o.adapters.len(), 1);
        let a = &o.adapters[0];
        assert_eq!(a.alias, "laptop");
        assert!(a.powered && !a.blocked() && !a.discovering);
        assert_eq!(a.discoverable_timeout, 180);

        assert_eq!(o.devices.len(), 2);
        let bb = &o.devices[0]; // sorted by name: "BB-…" before "My …"
        assert_eq!(bb.name, None);
        assert!(!bb.paired);
        assert_eq!(bb.signal, Some(2));
        assert_eq!(bb.battery, None);
        let hp = &o.devices[1];
        assert_eq!(hp.alias, "My Headphones");
        assert_eq!(hp.name.as_deref(), Some("Headphones"));
        assert!(hp.paired && hp.connected);
        assert_eq!(hp.battery, Some(80));
        assert_eq!(hp.adapter, "/org/bluez/hci0");
        assert_eq!(hp.kind_name(), Some("Headphones"));
        assert_eq!(hp.signal, None);
    }

    #[test]
    fn unexpected_data_gives_nothing() {
        let o = parse_objects(&"nonsense".to_variant());
        assert!(o.adapters.is_empty() && o.devices.is_empty());
    }

    #[test]
    fn signal_bars() {
        assert_eq!(bars(-40), 4);
        assert_eq!(bars(-60), 4);
        assert_eq!(bars(-61), 3);
        assert_eq!(bars(-75), 2);
        assert_eq!(bars(-95), 1);
    }

    #[test]
    fn agent_requests() {
        let dev = || object_path("/org/bluez/hci0/dev_AA");
        assert_eq!(
            parse_request("RequestConfirmation", &(dev(), 123456u32).to_variant()),
            Some(Request::Confirm {
                device: "/org/bluez/hci0/dev_AA".into(),
                passkey: 123456
            })
        );
        assert_eq!(
            parse_request("DisplayPasskey", &(dev(), 42u32, 3u16).to_variant()),
            Some(Request::DisplayPasskey {
                device: "/org/bluez/hci0/dev_AA".into(),
                passkey: 42,
                entered: 3
            })
        );
        assert_eq!(
            parse_request("RequestPinCode", &(dev(),).to_variant()),
            Some(Request::PinCode {
                device: "/org/bluez/hci0/dev_AA".into()
            })
        );
        assert_eq!(
            parse_request("Cancel", &().to_variant()),
            Some(Request::Cancel)
        );
        assert_eq!(parse_request("Release", &().to_variant()), None);
        assert_eq!(parse_request("RequestPinCode", &().to_variant()), None);
    }

    #[test]
    fn service_names() {
        assert_eq!(
            service_name("0000110b-0000-1000-8000-00805f9b34fb"),
            Some("audio playback (sink)")
        );
        assert_eq!(service_name("0000ffff-0000-1000-8000-00805f9b34fb"), None);
        assert_eq!(service_name("short"), None);
    }
}
