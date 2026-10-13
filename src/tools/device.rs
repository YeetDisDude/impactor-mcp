//! Device tools: listing, info, pairing ("Trust"), installed apps, installing
//! pre-signed apps, pairing files and provisioning profiles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use idevice::IdeviceService;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::usbmuxd::{UsbmuxdAddr, UsbmuxdConnection};
use plume_core::MobileProvision;
use plume_utils::{Device, Package, SignerApp, SignerAppReal};
use serde_json::{Value, json};

use crate::common::{self, Ctx, ToolResult, bool_or, opt_str, req_str, schema, tool};

const LABEL: &str = "impactor_mcp";

fn udid_prop() -> Value {
    json!({
        "type": "string",
        "description": "UDID (or usbmuxd device id) of the target device, as returned by `device_list`. \
                        Optional when exactly one device is connected. Use \"mac\" for This Mac (Apple Silicon only)."
    })
}

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "device_list",
            "List connected devices (iPhone/iPad over USB or Wi-Fi via usbmuxd, plus \"This Mac\" on \
             Apple Silicon Macs). Returns name, udid, device_id, connection (usb/wifi/local) and is_mac. \
             Use the udid with the other device_* tools.",
            schema(json!({}), &[]),
        ),
        tool(
            "device_info",
            "Read lockdown device values (DeviceName, ProductType, ProductVersion, BuildVersion, \
             SerialNumber, etc.) from a connected iOS device. Uses the host pairing record when \
             available for a full value set; otherwise returns the limited unpaired set.",
            schema(json!({ "udid": udid_prop() }), &[]),
        ),
        tool(
            "device_pair",
            "Pair (\"Trust\") this computer with a USB-connected iOS device and save the pairing \
             record to usbmuxd. The device must be unlocked; the user must tap \"Trust\" and enter \
             the passcode on the device when prompted, so tell them to watch the screen.",
            schema(json!({ "udid": udid_prop() }), &[]),
        ),
        tool(
            "device_list_apps",
            "List user-installed apps on a USB-connected iOS device. Each entry includes bundle_id, \
             name, version, the detected app type (e.g. SideStore, Feather, StikDebug, LiveContainer) \
             and, when that app type accepts one, its pairing_file_path (use with \
             `device_install_pairing_file`). With pairing_capable_only=true only apps that can \
             receive a pairing file are returned.",
            schema(
                json!({
                    "udid": udid_prop(),
                    "pairing_capable_only": {
                        "type": "boolean",
                        "description": "Only return apps that accept a pairing file (default false).",
                        "default": false
                    }
                }),
                &[],
            ),
        ),
        tool(
            "device_install_app",
            "Install an ALREADY-SIGNED .ipa file or .app directory onto a device as-is, without \
             re-signing (use `sign_and_install` to sign first). Works for iOS devices over usbmuxd \
             and for \"This Mac\" on Apple Silicon (installs to /Applications/iOS). Reports progress.",
            schema(
                json!({
                    "path": { "type": "string", "description": "Absolute path to a signed .ipa file or .app bundle directory." },
                    "udid": udid_prop()
                }),
                &["path"],
            ),
        ),
        tool(
            "device_install_pairing_file",
            "Write this computer's pairing file for the device into an installed app's Documents \
             container (house_arrest), as the Impactor GUI \"Utilities\" screen does for SideStore, \
             StikDebug, Feather, LiveContainer, etc. Also enables Wi-Fi debugging on the device. \
             The destination path defaults to the known location for the detected app type; pass \
             pairing_path for unknown apps. With remote_pairing=true an RPPairing (RemoteXPC) file \
             is generated/reused instead of the classic lockdown pairing record (needed by some \
             apps on iOS 17.4+, e.g. Dev Auto Capture). The device must already be paired/trusted.",
            schema(
                json!({
                    "bundle_id": { "type": "string", "description": "Bundle identifier of the installed target app (see `device_list_apps`)." },
                    "udid": udid_prop(),
                    "pairing_path": {
                        "type": "string",
                        "description": "Destination path inside the app container, e.g. /Documents/pairingFile.plist. Defaults to the path for the detected app type."
                    },
                    "remote_pairing": {
                        "type": "boolean",
                        "description": "Install a remote (RPPairing) pairing file instead of the lockdown pairing record (default false).",
                        "default": false
                    }
                }),
                &["bundle_id"],
            ),
        ),
        tool(
            "device_install_profile",
            "Install a provisioning profile (.mobileprovision) onto a USB-connected iOS device via \
             misagent.",
            schema(
                json!({
                    "path": { "type": "string", "description": "Absolute path to the .mobileprovision file." },
                    "udid": udid_prop()
                }),
                &["path"],
            ),
        ),
        tool(
            "device_is_app_installed",
            "Check whether a user app with the given bundle identifier is installed on a \
             USB-connected iOS device.",
            schema(
                json!({
                    "bundle_id": { "type": "string", "description": "Bundle identifier to look for." },
                    "udid": udid_prop()
                }),
                &["bundle_id"],
            ),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult> {
    let result = match name {
        "device_list" => device_list().await,
        "device_info" => device_info(args).await,
        "device_pair" => device_pair(args).await,
        "device_list_apps" => device_list_apps(args).await,
        "device_install_app" => device_install_app(args, ctx).await,
        "device_install_pairing_file" => device_install_pairing_file(args, ctx).await,
        "device_install_profile" => device_install_profile(args).await,
        "device_is_app_installed" => device_is_app_installed(args).await,
        _ => return None,
    };
    Some(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn target_device(args: &Value) -> Result<Device, String> {
    common::find_device(opt_str(args, "udid").as_deref()).await
}

async fn usb_target_device(args: &Value) -> Result<Device, String> {
    let device = target_device(args).await?;
    common::require_usb_device(&device)?;
    Ok(device)
}

/// All user apps (bundle id -> info dictionary) via installation_proxy.
async fn user_apps(device: &Device) -> Result<HashMap<String, plist::Value>, String> {
    let usbmuxd_device = device
        .usbmuxd_device
        .as_ref()
        .ok_or_else(|| "Device is not connected through usbmuxd".to_string())?;
    let provider = usbmuxd_device.to_provider(UsbmuxdAddr::from_env_var().unwrap_or_default(), LABEL);

    let mut ic = InstallationProxyClient::connect(&provider)
        .await
        .map_err(|e| {
            format!(
                "Failed to connect to installation_proxy on {} ({e}). Make sure the device is \
                 unlocked and trusts this computer (`device_pair`).",
                device.name
            )
        })?;

    ic.get_apps(Some("User"), None)
        .await
        .map_err(|e| format!("Failed to list installed apps: {e}"))
}

fn info_string(info: &plist::Value, key: &str) -> Option<String> {
    info.as_dictionary()
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_string())
        .map(str::to_string)
}

fn app_display_name(info: &plist::Value) -> Option<String> {
    info_string(info, "CFBundleDisplayName")
        .or_else(|| info_string(info, "CFBundleName"))
        .or_else(|| info_string(info, "CFBundleExecutable"))
}

fn signer_app_json(app: &SignerAppReal) -> Value {
    json!({
        "bundle_id": app.bundle_id,
        "app_type": app.app.to_string(),
        "pairing_file_path": app.app.pairing_file_path(),
    })
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

async fn device_list() -> ToolResult {
    let devices = common::list_devices().await?;
    Ok(json!({
        "count": devices.len(),
        "devices": devices.iter().map(common::device_json).collect::<Vec<_>>(),
    }))
}

async fn device_info(args: &Value) -> ToolResult {
    let device = target_device(args).await?;

    if device.is_mac {
        return Ok(json!({
            "device": common::device_json(&device),
            "note": "This Mac is a local install target; lockdown values are only available for iOS devices."
        }));
    }

    let usbmuxd_device = device
        .usbmuxd_device
        .as_ref()
        .ok_or_else(|| "Device is not connected through usbmuxd".to_string())?;
    let provider = usbmuxd_device.to_provider(UsbmuxdAddr::from_env_var().unwrap_or_default(), LABEL);

    let mut lockdown = LockdownClient::connect(&provider)
        .await
        .map_err(|e| format!("Failed to connect to lockdown on {}: {e}", device.name))?;

    // Try to start a session with the host pairing record for the full value set.
    let mut paired_session = false;
    match UsbmuxdConnection::default().await {
        Ok(mut usbmuxd) => match usbmuxd.get_pair_record(&device.udid).await {
            Ok(pairing_file) => match lockdown.start_session(&pairing_file).await {
                Ok(()) => paired_session = true,
                Err(e) => log::warn!("Lockdown start_session failed: {e}"),
            },
            Err(e) => log::info!("No pairing record for {}: {e}", device.udid),
        },
        Err(e) => log::warn!("Could not connect to usbmuxd for pair record: {e}"),
    }

    let values = lockdown
        .get_value(None, None)
        .await
        .map_err(|e| format!("Failed to read device values from lockdown: {e}"))?;

    Ok(json!({
        "device": common::device_json(&device),
        "paired_session": paired_session,
        "note": if paired_session {
            "Values read inside a paired lockdown session."
        } else {
            "Not paired with this computer: only a limited value set is available. Use `device_pair` for full info."
        },
        "values": common::plist_to_json(&values),
    }))
}

async fn device_pair(args: &Value) -> ToolResult {
    let device = usb_target_device(args).await?;

    device.pair().await.map_err(|e| {
        format!(
            "Failed to pair with {}: {e}. Make sure the device is unlocked, then tap \"Trust\" \
             and enter the passcode when prompted on the device, and retry.",
            device.name
        )
    })?;

    Ok(json!({
        "paired": true,
        "device": common::device_json(&device),
        "message": format!("Paired with {}; the pairing record was saved to usbmuxd.", device.name),
    }))
}

async fn device_list_apps(args: &Value) -> ToolResult {
    let device = usb_target_device(args).await?;
    let pairing_capable_only = bool_or(args, "pairing_capable_only", false);

    if pairing_capable_only {
        let apps = device
            .installed_apps()
            .await
            .map_err(|e| format!("Failed to list pairing-capable apps: {e}"))?;
        return Ok(json!({
            "device": common::device_json(&device),
            "count": apps.len(),
            "apps": apps.iter().map(signer_app_json).collect::<Vec<_>>(),
        }));
    }

    let apps = user_apps(&device).await?;
    let mut out: Vec<Value> = apps
        .iter()
        .map(|(bundle_id, info)| {
            let name = app_display_name(info);
            let app = SignerApp::from_bundle_identifier_or_name(
                Some(bundle_id.as_str()),
                name.as_deref(),
            );
            let supports_pairing = app.supports_pairing_file_alt();
            json!({
                "bundle_id": bundle_id,
                "name": name,
                "version": info_string(info, "CFBundleShortVersionString"),
                "build": info_string(info, "CFBundleVersion"),
                "app_type": app.to_string(),
                "supports_pairing_file": supports_pairing,
                "pairing_file_path": if supports_pairing { app.pairing_file_path() } else { None },
            })
        })
        .collect();

    out.sort_by(|a, b| {
        let key = |v: &Value| {
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase()
        };
        key(a).cmp(&key(b))
    });

    Ok(json!({
        "device": common::device_json(&device),
        "count": out.len(),
        "apps": out,
    }))
}

async fn device_install_app(args: &Value, ctx: &Ctx) -> ToolResult {
    let path = common::req_existing_path(args, "path")?;
    let device = target_device(args).await?;

    if !device.is_mac {
        common::require_usb_device(&device)?;
    }

    ctx.progress.send(format!("Preparing {}", path.display()), 0);

    // An .ipa is extracted to a staging directory; a .app directory is used directly.
    let mut package: Option<Package> = None;
    let app_path: PathBuf = if path.is_dir() {
        path.clone()
    } else {
        let pkg = Package::new(path.clone())
            .map_err(|e| format!("Failed to open package {}: {e}", path.display()))?;
        let bundle = match pkg.get_package_bundle() {
            Ok(b) => b,
            Err(e) => {
                pkg.remove_package_stage();
                return Err(format!(
                    "Failed to extract an .app bundle from {}: {e}",
                    path.display()
                ));
            }
        };
        let dir = bundle.bundle_dir().clone();
        package = Some(pkg);
        dir
    };

    let result = if device.is_mac {
        ctx.progress.send("Installing to This Mac", 10);
        plume_utils::install_app_mac(&app_path)
            .await
            .map_err(|e| format!("Failed to install to This Mac: {e}"))
    } else {
        install_to_device(&device, &app_path, ctx).await
    };

    if let Some(pkg) = package {
        pkg.remove_package_stage();
    }

    result?;
    ctx.progress.send("Installation complete", 100);

    Ok(json!({
        "installed": true,
        "device": common::device_json(&device),
        "app_path": if device.is_mac { Value::Null } else { json!(app_path.display().to_string()) },
        "source": path.display().to_string(),
    }))
}

/// Runs `Device::install_app`, forwarding its progress (the callback must be
/// `Send + 'static`) to the MCP progress channel.
async fn install_to_device(device: &Device, app_path: &PathBuf, ctx: &Ctx) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<i32>();

    let install = device.install_app(app_path, move |progress| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(progress);
        }
    });

    let forward = async {
        loop {
            while let Ok(p) = rx.try_recv() {
                ctx.progress.send(format!("Installing ({p}%)"), p);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };

    let install = std::pin::pin!(install);
    let forward = std::pin::pin!(forward);
    let result = match futures::future::select(install, forward).await {
        futures::future::Either::Left((res, _)) => res,
        futures::future::Either::Right(_) => unreachable!("progress forwarder never completes"),
    };

    while let Ok(p) = rx.try_recv() {
        ctx.progress.send(format!("Installing ({p}%)"), p);
    }

    result.map_err(|e| {
        format!(
            "Failed to install on {}: {e}. Make sure the app is signed for this device \
             (registered UDID, valid profile) and the device is unlocked.",
            device.name
        )
    })
}

async fn device_install_pairing_file(args: &Value, ctx: &Ctx) -> ToolResult {
    let bundle_id = req_str(args, "bundle_id")?;
    let remote_pairing = bool_or(args, "remote_pairing", false);
    let device = usb_target_device(args).await?;

    // Verify the app is installed and detect its type.
    let apps = user_apps(&device).await?;
    let info = apps.get(&bundle_id).ok_or_else(|| {
        format!(
            "App '{bundle_id}' is not installed on {}. Use `device_list_apps` to find the bundle id.",
            device.name
        )
    })?;
    let name = app_display_name(info);
    let app = SignerApp::from_bundle_identifier_or_name(Some(bundle_id.as_str()), name.as_deref());

    let pairing_path = match opt_str(args, "pairing_path") {
        Some(p) => p,
        None => app.pairing_file_path().map(str::to_string).ok_or_else(|| {
            format!(
                "Unknown pairing file location for '{bundle_id}' (detected type: {app}). Pass \
                 `pairing_path`, e.g. /Documents/pairingFile.plist."
            )
        })?,
    };

    ctx.progress.send(
        format!(
            "Installing {} pairing file to {bundle_id}:{pairing_path}",
            if remote_pairing { "remote (RP)" } else { "lockdown" }
        ),
        10,
    );

    if remote_pairing {
        device
            .install_remote_pairing_record(&bundle_id, &pairing_path, common::data_path())
            .await
            .map_err(|e| {
                format!(
                    "Failed to install remote pairing file: {e}. The device must be paired \
                     (`device_pair`), unlocked, and on iOS 17+."
                )
            })?;
    } else {
        device
            .install_pairing_record(&bundle_id, &pairing_path)
            .await
            .map_err(|e| {
                format!(
                    "Failed to install pairing file: {e}. Make sure the device is paired with \
                     this computer (`device_pair`) and unlocked, and the app supports file sharing \
                     of its Documents directory."
                )
            })?;
    }

    ctx.progress.send("Pairing file installed", 100);

    Ok(json!({
        "installed": true,
        "device": common::device_json(&device),
        "bundle_id": bundle_id,
        "app_name": name,
        "app_type": app.to_string(),
        "pairing_path": pairing_path,
        "remote_pairing": remote_pairing,
    }))
}

async fn device_install_profile(args: &Value) -> ToolResult {
    let path = common::req_existing_path(args, "path")?;
    let profile = MobileProvision::load_with_path(&path)
        .map_err(|e| format!("Failed to load provisioning profile {}: {e}", path.display()))?;
    let device = usb_target_device(args).await?;

    device
        .install_profile(&profile)
        .await
        .map_err(|e| format!("Failed to install profile on {}: {e}", device.name))?;

    Ok(json!({
        "installed": true,
        "device": common::device_json(&device),
        "profile": path.display().to_string(),
        "bundle_id": profile.bundle_id(),
    }))
}

async fn device_is_app_installed(args: &Value) -> ToolResult {
    let bundle_id = req_str(args, "bundle_id")?;
    let device = usb_target_device(args).await?;

    let installed = device
        .is_app_installed(&bundle_id)
        .await
        .map_err(|e| format!("Failed to query installed apps: {e}"))?;

    Ok(json!({
        "bundle_id": bundle_id,
        "installed": installed,
        "device": common::device_json(&device),
    }))
}
