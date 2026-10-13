//! Helpers shared by every tool module.

use std::path::{Path, PathBuf};
use std::{env, fs};

use idevice::usbmuxd::{Connection, UsbmuxdConnection};
use plume_core::{AnisetteConfiguration, developer::DeveloperSession};
use plume_store::{AccountStore, GsaAccount};
use plume_utils::Device;
use serde_json::{Map, Value, json};

use crate::{Writer, write_message};

pub type ToolResult = Result<Value, String>;

/// Per-call context handed to every tool.
pub struct Ctx {
    pub progress: Progress,
}

/// Sends `notifications/progress` when the client supplied a progress token.
pub struct Progress {
    writer: Writer,
    token: Option<Value>,
}

impl Progress {
    pub fn new(writer: Writer, token: Option<Value>) -> Self {
        Self { writer, token }
    }

    /// `progress` is a percentage in 0..=100.
    pub fn send(&self, message: impl Into<String>, progress: i32) {
        let message = message.into();
        log::info!("[{progress}%] {message}");
        if let Some(token) = &self.token {
            write_message(
                &self.writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": progress.clamp(0, 100),
                        "total": 100,
                        "message": message
                    }
                }),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Paths & store (identical locations to the GUI and plumesign)
// ---------------------------------------------------------------------------

pub fn data_path() -> PathBuf {
    let base = if cfg!(windows) {
        env::var("APPDATA").unwrap_or_else(|_| ".".into())
    } else {
        env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| env::var("HOME").unwrap_or_else(|_| ".".into()) + "/.config")
    };

    let dir = Path::new(&base).join("PlumeImpactor");
    fs::create_dir_all(&dir).ok();
    dir
}

pub fn store_path() -> PathBuf {
    data_path().join("accounts.json")
}

pub async fn load_store() -> Result<AccountStore, String> {
    AccountStore::load(&Some(store_path()))
        .await
        .map_err(|e| format!("Failed to load account store: {e}"))
}

pub fn anisette_config() -> AnisetteConfiguration {
    AnisetteConfiguration::default().set_configuration_path(data_path())
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

pub fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn req_str(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key).ok_or_else(|| format!("Missing required argument `{key}`"))
}

pub fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

pub fn bool_or(args: &Value, key: &str, default: bool) -> bool {
    opt_bool(args, key).unwrap_or(default)
}

pub fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

pub fn str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => Vec::new(),
    }
}

pub fn opt_path(args: &Value, key: &str) -> Option<PathBuf> {
    opt_str(args, key).map(PathBuf::from)
}

pub fn req_existing_path(args: &Value, key: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(req_str(args, key)?);
    if !path.exists() {
        return Err(format!("`{key}`: path does not exist: {}", path.display()));
    }
    Ok(path)
}

/// Builds a JSON-Schema `object` for a tool's `inputSchema`.
pub fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Builds a tool definition for `tools/list`.
pub fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

// ---------------------------------------------------------------------------
// Accounts / developer sessions
// ---------------------------------------------------------------------------

/// Returns the account for `email`, or the selected account when `email` is None.
pub fn get_account(store: &AccountStore, email: Option<&str>) -> Result<GsaAccount, String> {
    match email {
        Some(email) => store
            .get_account(email)
            .cloned()
            .ok_or_else(|| format!("No saved account for {email}. Use `account_list`.")),
        None => store.selected_account().cloned().ok_or_else(|| {
            "No account selected. Log in with `account_login` or pick one with `account_switch`."
                .to_string()
        }),
    }
}

pub async fn session_for(account: &GsaAccount) -> Result<DeveloperSession, String> {
    DeveloperSession::new(
        account.adsid().clone(),
        account.xcode_gs_token().clone(),
        anisette_config(),
    )
    .await
    .map_err(|e| {
        format!(
            "Failed to restore session for {} (it may have expired; log in again): {e}",
            account.email()
        )
    })
}

/// Loads the store, picks the account (`email` arg or selected) and opens a session.
pub async fn open_session(args: &Value) -> Result<(GsaAccount, DeveloperSession), String> {
    let store = load_store().await?;
    let account = get_account(&store, opt_str(args, "email").as_deref())?;
    let session = session_for(&account).await?;
    Ok((account, session))
}

/// Team resolution, same rules as the GUI: explicit argument, then the team
/// stored on the account (validated), then the first team.
pub async fn resolve_team(
    session: &DeveloperSession,
    account: &GsaAccount,
    explicit: Option<String>,
) -> Result<String, String> {
    let teams = session
        .qh_list_teams()
        .await
        .map_err(|e| format!("Failed to list teams: {e}"))?
        .teams;

    if teams.is_empty() {
        return Err("No teams available for this account".to_string());
    }

    let wanted = explicit.unwrap_or_else(|| account.team_id().clone());
    if wanted.is_empty() {
        return Ok(teams[0].team_id.clone());
    }

    if teams.iter().any(|t| t.team_id == wanted) {
        Ok(wanted)
    } else {
        Err(format!(
            "Team '{wanted}' not found for this account. Use `account_teams` to list valid teams."
        ))
    }
}

/// Certificate-reset callback: only proceeds when the caller explicitly allowed it.
pub fn cert_reset_callback(allowed: bool, progress: &Progress) -> impl FnMut() -> bool + '_ {
    move || {
        if allowed {
            progress.send(
                "Too many certificates; revoking existing ones (allow_certificate_reset=true)",
                20,
            );
        } else {
            log::warn!("Certificate reset required but allow_certificate_reset=false");
        }
        allowed
    }
}

pub const CERT_RESET_HINT: &str = "A new development certificate is needed and the account already \
has the maximum number. Retry with `allow_certificate_reset: true` to revoke the existing ones \
(this breaks existing SideStore/AltStore installs).";

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

pub fn connection_kind(device: &Device) -> &'static str {
    match &device.usbmuxd_device {
        Some(d) => match &d.connection_type {
            Connection::Usb => "usb",
            Connection::Network(_) => "wifi",
            Connection::Unknown(_) => "unknown",
        },
        None => "local",
    }
}

pub fn device_json(device: &Device) -> Value {
    json!({
        "name": device.name,
        "udid": device.udid,
        "device_id": device.device_id,
        "connection": connection_kind(device),
        "is_mac": device.is_mac,
    })
}

/// "This Mac" pseudo-device (Apple Silicon only), same as the GUI.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn this_mac() -> Option<Device> {
    plume_gestalt::get_udid().map(|udid| Device {
        name: "This Mac".into(),
        udid,
        device_id: u32::MAX,
        usbmuxd_device: None,
        is_mac: true,
    })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn this_mac() -> Option<Device> {
    None
}

/// All connected devices. A device reachable over both USB and Wi-Fi appears
/// once, preferring USB.
pub async fn list_devices() -> Result<Vec<Device>, String> {
    let mut out: Vec<Device> = Vec::new();

    match UsbmuxdConnection::default().await {
        Ok(mut muxer) => {
            let raw = muxer
                .get_devices()
                .await
                .map_err(|e| format!("Failed to list devices from usbmuxd: {e}"))?;
            let devices =
                futures::future::join_all(raw.into_iter().map(Device::new)).await;

            for device in devices {
                if let Some(existing) = out.iter_mut().find(|d| d.udid == device.udid) {
                    if connection_kind(existing) != "usb" && connection_kind(&device) == "usb" {
                        *existing = device;
                    }
                } else {
                    out.push(device);
                }
            }
        }
        Err(e) => {
            if this_mac().is_none() {
                return Err(format!(
                    "Could not connect to usbmuxd ({e}). Make sure iTunes / Apple Devices \
                     (Windows) or usbmuxd (Linux) is installed and running."
                ));
            }
        }
    }

    if let Some(mac) = this_mac() {
        out.push(mac);
    }

    Ok(out)
}

/// Finds a device by UDID (or usbmuxd device id). With `None`, uses the only
/// connected device, or fails asking the caller to choose.
pub async fn find_device(udid: Option<&str>) -> Result<Device, String> {
    let devices = list_devices().await?;

    match udid {
        Some(wanted) => {
            let wanted_lower = wanted.to_lowercase();
            devices
                .into_iter()
                .find(|d| {
                    d.udid.to_lowercase() == wanted_lower
                        || d.device_id.to_string() == wanted
                        || (d.is_mac && wanted_lower == "mac")
                })
                .ok_or_else(|| {
                    format!("Device '{wanted}' is not connected. Use `device_list`.")
                })
        }
        None => match devices.len() {
            0 => Err("No devices connected. Connect and unlock a device, then trust this computer."
                .to_string()),
            1 => Ok(devices.into_iter().next().unwrap()),
            _ => Err(format!(
                "Multiple devices connected; pass `udid`. Connected: {}",
                devices
                    .iter()
                    .map(|d| format!("{} ({})", d.name, d.udid))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    }
}

pub fn require_usb_device(device: &Device) -> Result<(), String> {
    if device.is_mac || device.usbmuxd_device.is_none() {
        Err("This operation needs an iOS device connected through usbmuxd (not supported for Mac targets).".into())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// plist -> JSON
// ---------------------------------------------------------------------------

/// Converts a plist value to JSON. Binary data becomes `"<N bytes>"`, dates
/// become RFC 3339 strings.
pub fn plist_to_json(value: &plist::Value) -> Value {
    match value {
        plist::Value::Dictionary(d) => dict_to_json(d),
        plist::Value::Array(a) => Value::Array(a.iter().map(plist_to_json).collect()),
        plist::Value::Boolean(b) => Value::Bool(*b),
        plist::Value::Data(d) => Value::String(format!("<{} bytes>", d.len())),
        plist::Value::Date(d) => Value::String(d.to_xml_format()),
        plist::Value::Real(r) => json!(r),
        plist::Value::Integer(i) => i
            .as_signed()
            .map(|v| json!(v))
            .or_else(|| i.as_unsigned().map(|v| json!(v)))
            .unwrap_or(Value::Null),
        plist::Value::String(s) => Value::String(s.clone()),
        plist::Value::Uid(u) => json!(u.get()),
        _ => Value::Null,
    }
}

pub fn dict_to_json(dict: &plist::Dictionary) -> Value {
    let mut map = Map::new();
    for (k, v) in dict {
        map.insert(k.clone(), plist_to_json(v));
    }
    Value::Object(map)
}
