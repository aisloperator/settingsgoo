//! Users: the account of the user running the app — picture, real name,
//! account type, password, login shell, automatic login and group memberships.
//!
//! Account details go through AccountsService (`org.freedesktop.Accounts`),
//! which asks polkit for authorization. Group memberships are changed with
//! `gpasswd` run through `pkexec`. Automatic login is set through
//! AccountsService for GDM, and with a LightDM configuration file for LightDM.

use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_void};
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::sys::{self, Choice, Toggle};

const SERVICE: &str = "org.freedesktop.Accounts";
const USER_IFACE: &str = "org.freedesktop.Accounts.User";

/// Groups that give administrator rights (through sudo or polkit) on common
/// distributions.
const ADMIN_GROUPS: &[&str] = &["sudo", "wheel", "admin"];

const LIGHTDM_DROP_IN_DIR: &str = "/etc/lightdm/lightdm.conf.d";
const LIGHTDM_DROP_IN: &str = "/etc/lightdm/lightdm.conf.d/50-settingsgoo-autologin.conf";

// ---------------------------------------------------------------------------
// Password hashing, with libcrypt (libxcrypt), which every Linux system has
// for its own password checks.

#[link(name = "crypt")]
unsafe extern "C" {
    fn crypt_gensalt_ra(
        prefix: *const c_char,
        count: c_ulong,
        rbytes: *const c_char,
        nrbytes: c_int,
    ) -> *mut c_char;
    fn crypt(phrase: *const c_char, setting: *const c_char) -> *mut c_char;
}

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Hashes a password with the system's preferred method (for example
/// yescrypt or SHA-512), as stored in `/etc/shadow`.
fn hash_password(password: &str) -> Result<String, String> {
    hash_with_setting(password, None)
}

fn hash_with_setting(password: &str, setting: Option<&str>) -> Result<String, String> {
    let phrase =
        CString::new(password).map_err(|_| "The password can't contain a null character.")?;
    // SAFETY: crypt_gensalt_ra returns a malloc'd string (or null), which is
    // freed below. With a null prefix and no random bytes given, it uses the
    // default method and reads random bytes from the OS. crypt returns a
    // pointer to a static buffer (or null), which is copied before anything
    // else can call it; this only runs on the main thread.
    unsafe {
        let salt = match setting {
            Some(s) => CString::new(s).map_err(|_| "Bad setting")?.into_raw(),
            None => crypt_gensalt_ra(std::ptr::null(), 0, std::ptr::null(), 0),
        };
        if salt.is_null() {
            return Err("Could not prepare the password (crypt_gensalt failed).".into());
        }
        let hashed = crypt(phrase.as_ptr(), salt);
        match setting {
            Some(_) => drop(CString::from_raw(salt)),
            None => free(salt.cast()),
        }
        if hashed.is_null() {
            return Err("Could not prepare the password (crypt failed).".into());
        }
        let hashed = CStr::from_ptr(hashed).to_string_lossy().into_owned();
        if hashed.starts_with('*') || hashed.is_empty() {
            return Err("Could not prepare the password (crypt failed).".into());
        }
        Ok(hashed)
    }
}

// ---------------------------------------------------------------------------
// Groups

#[derive(Debug, Clone, PartialEq)]
struct GroupInfo {
    name: String,
    gid: u32,
    members: Vec<String>,
}

/// Parses `/etc/group` lines (`name:password:gid:member,member`).
fn parse_groups(text: &str) -> Vec<GroupInfo> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?.trim();
            let _password = f.next()?;
            let gid = f.next()?.trim().parse().ok()?;
            let members = f
                .next()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(String::from)
                .collect();
            (!name.is_empty() && !name.starts_with('#')).then(|| GroupInfo {
                name: name.to_string(),
                gid,
                members,
            })
        })
        .collect()
}

/// All groups known to the system (including from NSS sources such as LDAP,
/// through `getent`, if available).
fn all_groups() -> Vec<GroupInfo> {
    let text = sys::run("getent", &["group"])
        .or_else(|_| std::fs::read_to_string("/etc/group").map_err(|e| e.to_string()))
        .unwrap_or_default();
    let mut groups = parse_groups(&text);
    // getent may list a group from several sources; keep the first.
    let mut seen = std::collections::HashSet::new();
    groups.retain(|g| seen.insert(g.name.clone()));
    groups
}

/// The user's primary group ID, from the password database.
fn primary_gid(user: &str) -> Option<u32> {
    let line = sys::run("getent", &["passwd", user]).ok().or_else(|| {
        std::fs::read_to_string("/etc/passwd")
            .ok()?
            .lines()
            .find(|l| l.split(':').next() == Some(user))
            .map(String::from)
    })?;
    line.trim().split(':').nth(3)?.parse().ok()
}

/// The group IDs in effect for this process, which are those of the desktop
/// session it was started from.
fn session_gids() -> Vec<u32> {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |key: &str| -> Vec<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| {
                v.split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut gids = field("Groups:");
    // The real group ID (the first one on the Gid: line) is in effect too.
    gids.extend(field("Gid:").first());
    gids
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Membership {
    /// The user's primary group, which can't be removed here.
    Primary,
    /// A member now and in this session.
    Member,
    /// Added, but not in effect until the user logs in again.
    Added,
    /// Removed, but still in effect until the user logs out.
    Removed,
}

/// The user's groups, sorted by name, combining what the group database says
/// with what is in effect in this session.
fn memberships(
    user: &str,
    groups: &[GroupInfo],
    primary: Option<u32>,
    session: &[u32],
) -> Vec<(String, Membership)> {
    let mut out: Vec<(String, Membership)> = groups
        .iter()
        .filter_map(|g| {
            let configured = g.members.iter().any(|m| m == user);
            let effective = session.contains(&g.gid);
            let state = if Some(g.gid) == primary {
                Membership::Primary
            } else {
                match (configured, effective) {
                    (true, true) => Membership::Member,
                    (true, false) => Membership::Added,
                    (false, true) => Membership::Removed,
                    (false, false) => return None,
                }
            };
            Some((g.name.clone(), state))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Display manager and automatic login

#[derive(Debug, Clone, PartialEq)]
enum DisplayManager {
    Gdm,
    LightDm,
    Other(String),
    Unknown,
}

fn display_manager() -> DisplayManager {
    let name = std::fs::read_link("/etc/systemd/system/display-manager.service")
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .or_else(|| {
            // Debian and derivatives also record it here.
            let path = std::fs::read_to_string("/etc/X11/default-display-manager").ok()?;
            let base = path.trim().rsplit('/').next()?.to_string();
            (!base.is_empty()).then_some(base)
        });
    match name.as_deref() {
        Some("gdm" | "gdm3") => DisplayManager::Gdm,
        Some("lightdm") => DisplayManager::LightDm,
        Some(other) => DisplayManager::Other(other.to_string()),
        None => DisplayManager::Unknown,
    }
}

/// The `autologin-user` that LightDM would use, given the text of its
/// configuration files in the order LightDM reads them (later ones win).
fn lightdm_autologin_from(texts: &[String]) -> Option<String> {
    let mut user = None;
    for text in texts {
        let mut in_seat = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_seat = matches!(section.trim(), "Seat:*" | "SeatDefaults");
                continue;
            }
            if in_seat
                && let Some((k, v)) = line.split_once('=')
                && k.trim() == "autologin-user"
            {
                user = Some(v.trim().to_string());
            }
        }
    }
    user.filter(|u| !u.is_empty())
}

/// LightDM's configuration files in the order it reads them: the
/// `lightdm.conf.d` directories, then `/etc/lightdm/lightdm.conf` last.
fn lightdm_config_texts() -> Vec<String> {
    let mut texts = Vec::new();
    for dir in [
        "/usr/share/lightdm/lightdm.conf.d",
        "/usr/local/share/lightdm/lightdm.conf.d",
        "/etc/xdg/lightdm/lightdm.conf.d",
        LIGHTDM_DROP_IN_DIR,
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "conf"))
            .collect();
        files.sort();
        texts.extend(files.iter().filter_map(|f| std::fs::read_to_string(f).ok()));
    }
    texts.extend(std::fs::read_to_string("/etc/lightdm/lightdm.conf").ok());
    texts
}

fn lightdm_autologin() -> Option<String> {
    lightdm_autologin_from(&lightdm_config_texts())
}

/// Turns LightDM automatic login on or off for `user` with a drop-in file.
/// Distributions whose LightDM PAM setup only allows members of an
/// `autologin` group get the user added to it.
async fn set_lightdm_autologin(user: &str, on: bool) -> Result<(), String> {
    let script = r#"set -e
mkdir -p "$1"
printf '# Written by SettingsGoo (Users page).\n[Seat:*]\nautologin-user=%s\n' "$3" > "$2"
if [ -n "$3" ] && getent group autologin >/dev/null; then gpasswd -a "$3" autologin >/dev/null; fi"#;
    let value = if on { user } else { "" };
    sys::run_as_admin(&[
        "/bin/sh",
        "-c",
        script,
        "sh",
        LIGHTDM_DROP_IN_DIR,
        LIGHTDM_DROP_IN,
        value,
    ])
    .await?;
    let now = lightdm_autologin();
    if (now.as_deref() == Some(user)) != on {
        return Err(format!(
            "LightDM's automatic login is set to {} in /etc/lightdm/lightdm.conf, which \
             takes precedence. Change autologin-user there instead.",
            now.as_deref().unwrap_or("nobody")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AccountsService

fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

fn user_proxy(uid: u32) -> Option<gio::DBusProxy> {
    let accounts = sys::system_proxy(SERVICE)?;
    let found = accounts
        .call_sync(
            "FindUserById",
            Some(&(i64::from(uid),).to_variant()),
            gio::DBusCallFlags::NONE,
            -1,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    let path = found.child_value(0).str()?.to_string();
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        SERVICE,
        &path,
        USER_IFACE,
        None::<&gio::Cancellable>,
    )
    .ok()?;
    (!proxy.cached_property_names().is_empty()).then_some(proxy)
}

/// Calls a method on the user object (polkit may ask for a password).
async fn user_call(
    proxy: &gio::DBusProxy,
    method: &str,
    args: glib::Variant,
) -> Result<(), String> {
    sys::call(proxy, method, args).await.map(drop)
}

// ---------------------------------------------------------------------------
// Page

struct UsersPage {
    user: String,
    proxy: Option<gio::DBusProxy>,
    groups_area: gtk::Box,
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Users",
        "Your account on this computer. Most changes ask for an administrator password.",
    );
    let uid = current_uid();
    let proxy = uid.and_then(user_proxy);
    let user = proxy
        .as_ref()
        .and_then(|p| sys::prop_str(p, "UserName"))
        .unwrap_or_else(|| glib::user_name().to_string_lossy().into_owned());

    let this = Rc::new(UsersPage {
        user,
        proxy,
        groups_area: gtk::Box::new(gtk::Orientation::Vertical, 12),
    });

    if let Some(proxy) = &this.proxy {
        account_group(&this, &page, proxy);
    } else {
        page.append(&sys::dim_label(
            "The accounts service (AccountsService) is not available, so account details \
             can't be shown or changed. Group memberships are below.",
        ));
        let list = sys::group(&page, "Account");
        sys::row(&list, "Username", None, &sys::value_label(&this.user));
    }
    autologin_group(&this, &page);

    page.append(
        &gtk::Label::builder()
            .label("Groups")
            .xalign(0.0)
            .css_classes(["group-title"])
            .build(),
    );
    page.append(&sys::dim_label(
        "Groups give access to devices and features, such as printers, sound or \
         administration. Changes take effect after you log out and back in.",
    ));
    page.append(&this.groups_area);
    this.refresh_groups();

    page.upcast()
}

fn account_group(this: &Rc<UsersPage>, page: &gtk::Box, proxy: &gio::DBusProxy) {
    let list = sys::group(page, "Account");

    // Picture
    let picture = gtk::Image::builder().pixel_size(64).build();
    let icon_file = sys::prop_str(proxy, "IconFile").unwrap_or_default();
    show_picture(&picture, &icon_file);
    let choose = gtk::Button::with_label("Choose…");
    let remove = gtk::Button::with_label("Remove");
    let buttons = gtk::Box::builder().spacing(6).build();
    buttons.append(&choose);
    buttons.append(&remove);
    let hbox = gtk::Box::builder().spacing(12).build();
    hbox.append(&picture);
    hbox.append(&buttons);
    buttons.set_valign(gtk::Align::Center);
    {
        let (proxy, picture) = (proxy.clone(), picture.clone());
        choose.connect_clicked(move |b| choose_picture(b, &proxy, &picture));
    }
    {
        let (proxy, picture) = (proxy.clone(), picture.clone());
        remove.connect_clicked(move |b| {
            let (b, proxy, picture) = (b.clone(), proxy.clone(), picture.clone());
            glib::spawn_future_local(async move {
                match user_call(&proxy, "SetIconFile", ("",).to_variant()).await {
                    Ok(()) => show_picture(&picture, ""),
                    Err(e) => sys::show_error(&b, "Could not remove the picture", &e),
                }
            });
        });
    }
    sys::row(&list, "Picture", Some("Shown on the login screen"), &hbox);

    // Real name
    let entry = gtk::Entry::builder()
        .text(sys::prop_str(proxy, "RealName").unwrap_or_default())
        .width_chars(24)
        .build();
    let apply = gtk::Button::with_label("Apply");
    let hbox = gtk::Box::builder().spacing(6).build();
    hbox.append(&entry);
    hbox.append(&apply);
    let submit = {
        let (proxy, entry) = (proxy.clone(), entry.clone());
        move || {
            let (proxy, entry) = (proxy.clone(), entry.clone());
            glib::spawn_future_local(async move {
                let name = entry.text().trim().to_string();
                if let Err(e) =
                    user_call(&proxy, "SetRealName", (name.as_str(),).to_variant()).await
                {
                    sys::show_error(&entry, "Could not change the name", &e);
                }
            });
        }
    };
    let s = submit.clone();
    entry.connect_activate(move |_| s());
    apply.connect_clicked(move |_| submit());
    sys::row(
        &list,
        "Full name",
        Some("Your name as shown to others"),
        &hbox,
    );

    let uid = proxy
        .cached_property("Uid")
        .and_then(|v| v.get::<u64>())
        .map(|u| format!("  (user ID {u})"))
        .unwrap_or_default();
    sys::row(
        &list,
        "Username",
        Some("Used to log in; it can't be changed here"),
        &sys::value_label(&format!("{}{uid}", this.user)),
    );

    // Account type
    let account_type = proxy
        .cached_property("AccountType")
        .and_then(|v| v.get::<i32>())
        .unwrap_or(0);
    let types = [
        ("0".to_string(), "Standard".to_string()),
        ("1".to_string(), "Administrator".to_string()),
    ];
    let type_choice = Choice::new(&types, &account_type.to_string());
    {
        let (this, proxy, widget) = (this.clone(), proxy.clone(), type_choice.dropdown.clone());
        type_choice.on_change(move |t| {
            let (this, proxy, widget) = (this.clone(), proxy.clone(), widget.clone());
            async move {
                let t: i32 = t.parse().unwrap_or(0);
                if t == 0
                    && !sys::confirm(
                        &widget,
                        "Stop being an administrator?",
                        "You will no longer be able to change system settings or install \
                         software with your own password. If no other administrator account \
                         exists, you may not be able to undo this.",
                        "Make Standard",
                    )
                    .await
                {
                    return Err(String::new());
                }
                let result = user_call(&proxy, "SetAccountType", (t,).to_variant()).await;
                // The account type is a group membership, so show the change there.
                this.refresh_groups();
                result
            }
        });
    }
    sys::row(
        &list,
        "Account type",
        Some("Administrators can change system settings and install software"),
        &type_choice.dropdown,
    );

    // Password
    let change = gtk::Button::with_label("Change…");
    {
        let proxy = proxy.clone();
        change.connect_clicked(move |b| password_dialog(b, &proxy));
    }
    sys::row(
        &list,
        "Password",
        Some("The password for logging in and unlocking"),
        &change,
    );

    // Shell
    let shell = sys::prop_str(proxy, "Shell").unwrap_or_default();
    let shells = login_shells();
    let shell_choice = Choice::new(&sys::same(&shells), &shell);
    {
        let proxy = proxy.clone();
        shell_choice.on_change(move |s| {
            let proxy = proxy.clone();
            async move { user_call(&proxy, "SetShell", (s.as_str(),).to_variant()).await }
        });
    }
    sys::row(
        &list,
        "Login shell",
        Some("The command-line program used in terminals; from /etc/shells"),
        &shell_choice.dropdown,
    );

    if let Some(home) = sys::prop_str(proxy, "HomeDirectory") {
        sys::row(&list, "Home folder", None, &sys::value_label(&home));
    }
}

/// The login shells listed in `/etc/shells`.
fn login_shells() -> Vec<String> {
    let text = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let mut shells: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/'))
        .map(String::from)
        .collect();
    shells.dedup();
    shells
}

fn show_picture(image: &gtk::Image, path: &str) {
    match gdk::Texture::from_filename(path) {
        Ok(t) if !path.is_empty() => image.set_paintable(Some(&t)),
        _ => image.set_icon_name(Some("avatar-default-symbolic")),
    }
}

fn choose_picture(button: &gtk::Button, proxy: &gio::DBusProxy, picture: &gtk::Image) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Images"));
    filter.add_pixbuf_formats();
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title("Choose a Picture")
        .modal(true)
        .filters(&filters)
        .default_filter(&filter)
        .build();
    if let Some(pictures) = glib::user_special_dir(glib::UserDirectory::Pictures) {
        dialog.set_initial_folder(Some(&gio::File::for_path(pictures)));
    }
    let parent = button.root().and_downcast::<gtk::Window>();
    let (button, proxy, picture) = (button.clone(), proxy.clone(), picture.clone());
    glib::spawn_future_local(async move {
        let Ok(file) = dialog.open_future(parent.as_ref()).await else {
            return; // Cancelled
        };
        let Some(path) = file.path() else {
            sys::show_error(&button, "Could not use the picture", "Choose a local file.");
            return;
        };
        if let Err(e) = set_picture(&proxy, &path).await {
            sys::show_error(&button, "Could not set the picture", &e);
            return;
        }
        show_picture(&picture, &path.to_string_lossy());
    });
}

/// Scales the image down to a small PNG (AccountsService refuses large files)
/// and gives it to AccountsService, which keeps its own copy.
async fn set_picture(proxy: &gio::DBusProxy, path: &std::path::Path) -> Result<(), String> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, 256, 256, true)
        .map_err(|e| e.to_string())?;
    let tmp =
        glib::user_runtime_dir().join(format!("settingsgoo-picture-{}.png", std::process::id()));
    pixbuf.savev(&tmp, "png", &[]).map_err(|e| e.to_string())?;
    let result = user_call(
        proxy,
        "SetIconFile",
        (tmp.to_string_lossy().as_ref(),).to_variant(),
    )
    .await;
    let _ = std::fs::remove_file(&tmp);
    result
}

fn password_dialog(button: &gtk::Button, proxy: &gio::DBusProxy) {
    let parent = button.root().and_downcast::<gtk::Window>();
    let window = gtk::Window::builder()
        .title("Change Password")
        .modal(true)
        .resizable(false)
        .default_width(380)
        .build();
    window.set_transient_for(parent.as_ref());

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&sys::dim_label(
        "Changing the password asks for an administrator password. If you are an \
         administrator, that is your current password.",
    ));
    let new = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("New password")
        .build();
    let again = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("New password again")
        .activates_default(true)
        .build();
    let message = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["error"])
        .build();
    content.append(&new);
    content.append(&again);
    content.append(&message);

    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::builder()
        .label("Change Password")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let buttons = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&ok);
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&ok));

    let check = {
        let (new, again, ok, message) = (new.clone(), again.clone(), ok.clone(), message.clone());
        move || {
            let (a, b) = (new.text(), again.text());
            let matched = a == b;
            ok.set_sensitive(!a.is_empty() && matched);
            message.set_label(if !b.is_empty() && !matched {
                "The passwords don't match."
            } else {
                ""
            });
        }
    };
    let c = check.clone();
    new.connect_changed(move |_| c());
    again.connect_changed(move |_| check());
    new.connect_activate({
        let again = again.clone();
        move |_| {
            again.grab_focus();
        }
    });
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let (window, proxy) = (window.clone(), proxy.clone());
        ok.connect_clicked(move |ok| {
            let password = new.text().to_string();
            if password.is_empty() || password != again.text().as_str() {
                return;
            }
            let hashed = match hash_password(&password) {
                Ok(h) => h,
                Err(e) => {
                    message.set_label(&e);
                    return;
                }
            };
            ok.set_sensitive(false);
            let (window, proxy, ok, message) =
                (window.clone(), proxy.clone(), ok.clone(), message.clone());
            glib::spawn_future_local(async move {
                match user_call(&proxy, "SetPassword", (hashed.as_str(), "").to_variant()).await {
                    Ok(()) => window.close(),
                    Err(e) => {
                        message.set_label(&format!("Could not change the password: {e}"));
                        ok.set_sensitive(true);
                    }
                }
            });
        });
    }
    window.present();
}

fn autologin_group(this: &Rc<UsersPage>, page: &gtk::Box) {
    let list = sys::group(page, "Login");
    let dm = display_manager();
    let title = "Automatic login";
    match (&dm, &this.proxy) {
        (DisplayManager::Gdm, Some(proxy)) => {
            let on = sys::prop_bool(proxy, "AutomaticLogin").unwrap_or(false);
            let toggle = Toggle::new(on);
            let proxy = proxy.clone();
            toggle.on_change(move |on| {
                let proxy = proxy.clone();
                async move { user_call(&proxy, "SetAutomaticLogin", (on,).to_variant()).await }
            });
            sys::row(
                &list,
                title,
                Some("Log in to this account without a password when the computer starts (GDM)"),
                &toggle.switch,
            );
        }
        (DisplayManager::LightDm, _) => {
            let current = lightdm_autologin();
            let toggle = Toggle::new(current.as_deref() == Some(this.user.as_str()));
            let subtitle = match current.as_deref() {
                Some(u) if u != this.user => format!(
                    "Log in without a password when the computer starts (LightDM). \
                     Currently set for {u}."
                ),
                _ => "Log in to this account without a password when the computer starts \
                      (LightDM)"
                    .to_string(),
            };
            let this = this.clone();
            toggle.on_change(move |on| {
                let this = this.clone();
                async move {
                    let result = set_lightdm_autologin(&this.user, on).await;
                    // It may have added the user to the autologin group.
                    this.refresh_groups();
                    result
                }
            });
            sys::row(&list, title, Some(&subtitle), &toggle.switch);
        }
        _ => {
            let why = match &dm {
                DisplayManager::Other(name) => {
                    format!("Not supported for the {name} display manager yet")
                }
                DisplayManager::Gdm => "Needs AccountsService".to_string(),
                _ => "No supported display manager (GDM or LightDM) was found".to_string(),
            };
            let switch = gtk::Switch::builder().sensitive(false).build();
            sys::row(&list, title, Some(&why), &switch);
        }
    }
}

impl UsersPage {
    fn refresh_groups(self: &Rc<Self>) {
        sys::clear(&self.groups_area);
        let groups = all_groups();
        let primary = primary_gid(&self.user);
        let mine = memberships(&self.user, &groups, primary, &session_gids());
        let list = sys::group(&self.groups_area, "");

        for (name, state) in &mine {
            let subtitle = match state {
                Membership::Primary => Some("Your primary group"),
                Membership::Member => None,
                Membership::Added => Some("Added; takes effect after you log out and back in"),
                Membership::Removed => Some("Removed; still in effect until you log out"),
            };
            let control: gtk::Widget = match state {
                Membership::Member | Membership::Added => {
                    let b = gtk::Button::with_label("Remove");
                    let (this, name) = (self.clone(), name.clone());
                    b.connect_clicked(move |b| this.remove_from_group(b, &name));
                    b.upcast()
                }
                _ => gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast(),
            };
            sys::row_with_icon(
                &list,
                Some("system-users-symbolic"),
                name,
                subtitle,
                &control,
            );
        }

        // Adding to another group
        let mut others: Vec<String> = groups
            .iter()
            .map(|g| g.name.clone())
            .filter(|n| {
                !mine
                    .iter()
                    .any(|(m, s)| m == n && *s != Membership::Removed)
            })
            .collect();
        others.sort_by_key(|n| n.to_lowercase());
        if others.is_empty() {
            return;
        }
        let mut items = vec![(String::new(), "Choose…".to_string())];
        items.extend(sys::same(&others));
        let choice = Choice::new(&items, "");
        let add = gtk::Button::with_label("Add");
        let hbox = gtk::Box::builder().spacing(6).build();
        hbox.append(&choice.dropdown);
        hbox.append(&add);
        {
            let this = self.clone();
            add.connect_clicked(move |b| {
                let (this, b, group) = (this.clone(), b.clone(), choice.value());
                if group.is_empty() {
                    return;
                }
                glib::spawn_future_local(async move {
                    match gpasswd("-a", &this.user, &group).await {
                        Ok(()) => this.refresh_groups(),
                        Err(e) => sys::show_error(&b, "Could not add you to the group", &e),
                    }
                });
            });
        }
        sys::row(&list, "Add to a group", None, &hbox);
    }

    fn remove_from_group(self: &Rc<Self>, button: &gtk::Button, group: &str) {
        let (this, button, group) = (self.clone(), button.clone(), group.to_string());
        glib::spawn_future_local(async move {
            if ADMIN_GROUPS.contains(&group.as_str())
                && !sys::confirm(
                    &button,
                    &format!("Leave the {group} group?"),
                    "This group gives administrator rights. Without it you may no longer be \
                     able to change system settings, and if no other administrator account \
                     exists, you may not be able to undo this.",
                    "Leave",
                )
                .await
            {
                return;
            }
            match gpasswd("-d", &this.user, &group).await {
                Ok(()) => this.refresh_groups(),
                Err(e) => sys::show_error(&button, "Could not remove you from the group", &e),
            }
        });
    }
}

/// Adds (`-a`) or removes (`-d`) a user to or from a group, as root.
async fn gpasswd(flag: &str, user: &str, group: &str) -> Result<(), String> {
    let program = glib::find_program_in_path("gpasswd")
        .or_else(|| {
            ["/usr/bin/gpasswd", "/usr/sbin/gpasswd"]
                .iter()
                .map(std::path::PathBuf::from)
                .find(|p| p.exists())
        })
        .ok_or("gpasswd (part of the shadow utilities) is not installed.")?;
    let program = program.to_string_lossy();
    sys::run_as_admin(&[program.as_ref(), flag, user, group])
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUPS: &str = "root:x:0:\nsudo:x:27:alice,bob\naudio:x:29:alice\n\
                          video:x:44:bob\nalice:x:1000:\nbroken line\n";

    #[test]
    fn groups_parse() {
        let g = parse_groups(GROUPS);
        assert_eq!(g.len(), 5);
        assert_eq!(g[1].name, "sudo");
        assert_eq!(g[1].gid, 27);
        assert_eq!(g[1].members, ["alice", "bob"]);
        assert!(g[0].members.is_empty());
    }

    #[test]
    fn membership_states() {
        let g = parse_groups(GROUPS);
        // In effect: primary, audio, video (just removed). Not yet: sudo.
        let m = memberships("alice", &g, Some(1000), &[1000, 29, 44]);
        assert_eq!(
            m,
            [
                ("alice".to_string(), Membership::Primary),
                ("audio".to_string(), Membership::Member),
                ("sudo".to_string(), Membership::Added),
                ("video".to_string(), Membership::Removed),
            ]
        );
    }

    #[test]
    fn lightdm_parse() {
        let main = "[LightDM]\nautologin-user=wrong\n[Seat:*]\n#autologin-user=x\n".to_string();
        assert_eq!(lightdm_autologin_from(std::slice::from_ref(&main)), None);
        let drop_in = "[Seat:*]\nautologin-user = alice\n".to_string();
        assert_eq!(
            lightdm_autologin_from(&[drop_in.clone(), main.clone()]).as_deref(),
            Some("alice")
        );
        let off = "[SeatDefaults]\nautologin-user=\n".to_string();
        assert_eq!(lightdm_autologin_from(&[drop_in, off]), None);
    }

    #[test]
    fn password_hash() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with('$'), "{h}");
        // Hashing again with the stored value as the setting gives the same hash.
        assert_eq!(hash_with_setting("correct horse", Some(&h)).unwrap(), h);
        assert_ne!(hash_with_setting("wrong", Some(&h)).unwrap(), h);
        assert!(hash_password("a\0b").is_err());
    }
}
