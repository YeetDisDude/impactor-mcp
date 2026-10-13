//! Auto-refresh of saved apps (port of apps/plumeimpactor/src/refresh.rs).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::Utc;
use plume_core::{CertificateIdentity, MobileProvision};
use plume_store::{AccountStore, RefreshApp, RefreshDevice};
use plume_utils::{Bundle, Device, Signer, SignerMode, SignerOptions};
use serde_json::{Value, json};

use crate::common::{
    self, Ctx, Progress, ToolResult, bool_or, data_path, opt_str, req_str,
};
use crate::tools::sign::{install_with_progress, map_cert_err, scheduled_refresh_from_provision};

const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 3);

/// UDIDs with a refresh in progress (shared by tool calls and the background loop).
static ACTIVE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

struct RefreshGuard(String);

impl RefreshGuard {
    fn acquire(udid: &str) -> Result<Self, String> {
        let mut active = ACTIVE
            .lock()
            .map_err(|_| "Failed to lock refresh registry".to_string())?;
        if !active.insert(udid.to_string()) {
            return Err(format!("A refresh is already in progress for device {udid}"));
        }
        Ok(Self(udid.to_string()))
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.remove(&self.0);
        }
    }
}

pub fn definitions() -> Vec<Value> {
    let app_selector = json!({
        "udid": { "type": "string", "description": "UDID of the device the app was saved for (see `refresh_list`)." },
        "app_path": { "type": "string", "description": "Saved app path exactly as shown by `refresh_list`." },
        "bundle_id": { "type": "string", "description": "Bundle identifier of the saved app (alternative to app_path)." }
    });

    let mut now_props = app_selector.clone();
    now_props["allow_certificate_reset"] = json!({
        "type": "boolean",
        "description": "Allow revoking existing development certificates if a new one is needed (breaks existing SideStore/AltStore installs). Default false."
    });

    let mut forget_props = app_selector;
    forget_props["delete_files"] = json!({
        "type": "boolean",
        "description": "Also delete the saved signed copy from the refresh store (default true)."
    });

    vec![
        common::tool(
            "refresh_list",
            "List apps saved for auto-refresh (installed with `sign_and_install` refresh=true or \
             from the Impactor GUI), grouped by device: name, bundle id, saved path, next \
             scheduled refresh (RFC 3339), whether it is due now, and whether the saved copy \
             still exists. Free-account profiles expire after 7 days; refresh is scheduled 3 \
             days before expiry.",
            common::schema(json!({}), &[]),
        ),
        common::tool(
            "refresh_app_now",
            "Refresh one saved app immediately: renews its provisioning profile and, if the \
             certificate changed, the app is not installed anymore, or the target is a Mac, \
             re-signs and reinstalls the saved copy. The device must be connected. Identify the \
             app by `app_path` or `bundle_id` (optional if the device has only one saved app). \
             Long running; reports progress.",
            common::schema(now_props, &["udid"]),
        ),
        common::tool(
            "refresh_run_due",
            "Refresh every saved app whose scheduled refresh time has passed and whose device is \
             currently connected. Returns a per-app result (refreshed / not connected / error).",
            common::schema(
                json!({
                    "allow_certificate_reset": { "type": "boolean", "description": "Allow revoking existing development certificates if needed. Default false." }
                }),
                &[],
            ),
        ),
        common::tool(
            "refresh_forget_app",
            "Stop auto-refreshing a saved app: removes it from the refresh list (and the device \
             entry when no apps remain) and by default deletes its saved signed copy. Does not \
             uninstall the app from the device.",
            common::schema(forget_props, &["udid"]),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult> {
    match name {
        "refresh_list" => Some(refresh_list().await),
        "refresh_app_now" => Some(refresh_app_now(args, ctx).await),
        "refresh_run_due" => Some(
            run_due(&ctx.progress, bool_or(args, "allow_certificate_reset", false))
                .await
                .map(|results| json!({ "results": results })),
        ),
        "refresh_forget_app" => Some(refresh_forget_app(args).await),
        _ => None,
    }
}

/// Background loop (IMPACTOR_MCP_AUTO_REFRESH=1): every 3 minutes refreshes due
/// apps on connected devices. Certificate resets are never allowed here.
pub fn spawn_auto_refresh() {
    let spawned = std::thread::Builder::new()
        .name("impactor-mcp-auto-refresh".into())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("Auto-refresh: failed to build runtime: {err}");
                    return;
                }
            };
            // No progress token: nothing is ever written to stdout.
            let progress = Progress::new(Arc::new(Mutex::new(std::io::stdout())), None);
            log::info!("Auto-refresh enabled (checking every {}s)", CHECK_INTERVAL.as_secs());

            loop {
                match rt.block_on(run_due(&progress, false)) {
                    Ok(results) => {
                        for r in results {
                            if r.get("status").and_then(Value::as_str) == Some("error") {
                                log::error!("Auto-refresh failed: {r}");
                            } else {
                                log::info!("Auto-refresh: {r}");
                            }
                        }
                    }
                    Err(err) => log::error!("Auto-refresh error: {err}"),
                }
                std::thread::sleep(CHECK_INTERVAL);
            }
        });

    if let Err(err) = spawned {
        log::error!("Failed to start auto-refresh thread: {err}");
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn app_json(app: &RefreshApp) -> Value {
    json!({
        "name": app.name,
        "bundle_id": app.bundle_id,
        "path": app.path.display().to_string(),
        "scheduled_refresh": app.scheduled_refresh.to_rfc3339(),
        "due": app.scheduled_refresh <= Utc::now(),
        "path_exists": app.path.exists(),
    })
}

async fn refresh_list() -> ToolResult {
    let store = common::load_store().await?;
    let mut devices: Vec<&RefreshDevice> = store.refreshes().values().collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));

    let list: Vec<Value> = devices
        .into_iter()
        .map(|d| {
            json!({
                "udid": d.udid,
                "name": d.name,
                "account": d.account,
                "is_mac": d.is_mac,
                "apps": d.apps.iter().map(app_json).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({
        "devices": list,
        "auto_refresh_enabled": std::env::var("IMPACTOR_MCP_AUTO_REFRESH").is_ok_and(|v| v == "1"),
    }))
}

/// Finds the refresh device entry (exact or case-insensitive UDID) and one of its apps.
fn select_app(
    store: &AccountStore,
    args: &Value,
) -> Result<(RefreshDevice, RefreshApp), String> {
    let udid = req_str(args, "udid")?;
    let refresh_device = store
        .get_refresh_device(&udid)
        .or_else(|| {
            store
                .refreshes()
                .values()
                .find(|d| d.udid.eq_ignore_ascii_case(&udid))
        })
        .cloned()
        .ok_or_else(|| format!("No saved refresh apps for device {udid}. Use `refresh_list`."))?;

    let app_path = opt_str(args, "app_path");
    let bundle_id = opt_str(args, "bundle_id");

    let app = if let Some(path) = app_path {
        refresh_device
            .apps
            .iter()
            .find(|a| a.path.to_string_lossy() == path || a.path == PathBuf::from(&path))
            .cloned()
            .ok_or_else(|| format!("No saved app with path {path} for device {udid}"))?
    } else if let Some(bid) = bundle_id {
        refresh_device
            .apps
            .iter()
            .find(|a| a.bundle_id.as_deref() == Some(bid.as_str()))
            .cloned()
            .ok_or_else(|| format!("No saved app with bundle id {bid} for device {udid}"))?
    } else if refresh_device.apps.len() == 1 {
        refresh_device.apps[0].clone()
    } else {
        return Err(format!(
            "Device {udid} has {} saved apps; pass `app_path` or `bundle_id`.",
            refresh_device.apps.len()
        ));
    };

    Ok((refresh_device, app))
}

async fn refresh_app_now(args: &Value, ctx: &Ctx) -> ToolResult {
    let allow_reset = bool_or(args, "allow_certificate_reset", false);
    let store = common::load_store().await?;
    let (refresh_device, app) = select_app(&store, args)?;

    let device = common::find_device(Some(&refresh_device.udid)).await?;

    refresh_app(&store, &refresh_device, &app, &device, allow_reset, &ctx.progress).await
}

async fn refresh_forget_app(args: &Value) -> ToolResult {
    let delete_files = bool_or(args, "delete_files", true);
    let mut store = common::load_store().await?;
    let (mut refresh_device, app) = select_app(&store, args)?;

    let mut deleted = Value::Bool(false);
    if delete_files && app.path.exists() {
        // Only ever delete copies inside our own refresh store.
        let refresh_store = data_path().join("refresh_store");
        if app.path.starts_with(&refresh_store) {
            deleted = match std::fs::remove_dir_all(&app.path) {
                Ok(()) => Value::Bool(true),
                Err(err) => json!(format!("failed to delete: {err}")),
            };
        } else {
            deleted = json!("skipped: path is outside the refresh store");
        }
    }

    refresh_device.apps.retain(|a| a.path != app.path);
    let device_removed = refresh_device.apps.is_empty();
    let udid = refresh_device.udid.clone();
    if device_removed {
        store
            .remove_refresh_device_sync(&udid)
            .map_err(|e| format!("Failed to update store: {e}"))?;
    } else {
        store
            .add_or_update_refresh_device_sync(refresh_device)
            .map_err(|e| format!("Failed to update store: {e}"))?;
    }

    Ok(json!({
        "forgotten": app_json(&app),
        "udid": udid,
        "files_deleted": deleted,
        "device_entry_removed": device_removed,
    }))
}

// ---------------------------------------------------------------------------
// Refresh logic
// ---------------------------------------------------------------------------

/// Refreshes every due app whose device is connected. Never fails as a whole
/// because of a single app; per-app outcomes are returned.
async fn run_due(progress: &Progress, allow_reset: bool) -> Result<Vec<Value>, String> {
    let store = common::load_store().await?;
    let now = Utc::now();

    let due: Vec<(RefreshDevice, RefreshApp)> = store
        .refreshes()
        .values()
        .flat_map(|d| {
            d.apps
                .iter()
                .filter(|a| a.scheduled_refresh <= now)
                .map(move |a| (d.clone(), a.clone()))
        })
        .collect();

    if due.is_empty() {
        return Ok(Vec::new());
    }

    let connected = match common::list_devices().await {
        Ok(devices) => devices,
        Err(err) => {
            log::debug!("Refresh: cannot list devices: {err}");
            Vec::new()
        }
    };

    let mut results = Vec::new();
    for (refresh_device, app) in due {
        let base = json!({
            "udid": refresh_device.udid,
            "device": refresh_device.name,
            "app": app.name,
            "bundle_id": app.bundle_id,
            "path": app.path.display().to_string(),
        });

        let Some(device) = connected
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(&refresh_device.udid))
        else {
            let mut r = base;
            r["status"] = json!("not_connected");
            results.push(r);
            continue;
        };

        let mut r = base;
        match refresh_app(&store, &refresh_device, &app, device, allow_reset, progress).await {
            Ok(v) => {
                r["status"] = json!("refreshed");
                r["result"] = v;
            }
            Err(err) => {
                r["status"] = json!("error");
                r["error"] = json!(err);
            }
        }
        results.push(r);
    }

    Ok(results)
}

/// Same decision logic as the GUI daemon: reinstall when the target is a Mac,
/// the certificate is new, or the app is not installed; otherwise only push
/// fresh provisioning profiles. Then update the schedule.
async fn refresh_app(
    store: &AccountStore,
    refresh_device: &RefreshDevice,
    app: &RefreshApp,
    device: &Device,
    allow_reset: bool,
    progress: &Progress,
) -> Result<Value, String> {
    let _guard = RefreshGuard::acquire(&device.udid)?;

    let label = app.name.clone().unwrap_or_else(|| "app".to_string());
    if !app.path.exists() {
        return Err(format!(
            "Saved copy of {label} is missing ({}); reinstall it with sign_and_install refresh=true",
            app.path.display()
        ));
    }

    progress.send(format!("Refreshing {label} for {}...", refresh_device.name), 5);

    let account = store.get_account(&refresh_device.account).ok_or_else(|| {
        format!("Account {} not found (log in again)", refresh_device.account)
    })?;
    let session = common::session_for(account).await?;
    let team_id = common::resolve_team(&session, account, None).await?;

    progress.send("Checking certificate...", 15);
    let identity = {
        let mut on_reset = common::cert_reset_callback(allow_reset, progress);
        CertificateIdentity::new_with_session(
            &session,
            data_path(),
            None,
            &team_id,
            false,
            Some(&mut on_reset),
        )
        .await
        .map_err(map_cert_err)?
    };
    let identity_is_new = identity.new;

    let is_installed = if device.is_mac {
        false
    } else if let Some(bundle_id) = app.bundle_id.as_deref() {
        device
            .is_app_installed(bundle_id)
            .await
            .map_err(|e| format!("Failed to check if app is installed: {e}"))?
    } else {
        false
    };

    let needs_reinstall = device.is_mac || identity_is_new || !is_installed;

    let options = SignerOptions {
        mode: SignerMode::Pem,
        ..Default::default()
    };
    let bundle =
        Bundle::new(app.path.clone()).map_err(|e| format!("Failed to open saved bundle: {e}"))?;

    let action = if needs_reinstall {
        progress.send("Registering device...", 25);
        session
            .qh_ensure_device(&team_id, &device.name, &device.udid)
            .await
            .map_err(|e| format!("Failed to ensure device: {e}"))?;

        let mut signer = Signer::new(Some(identity), options);
        progress.send("Registering app...", 40);
        signer
            .register_bundle(&bundle, &session, &team_id, true)
            .await
            .map_err(|e| format!("Failed to register bundle: {e}"))?;
        progress.send("Signing...", 55);
        signer
            .sign_bundle(&bundle)
            .await
            .map_err(|e| format!("Failed to sign bundle: {e}"))?;

        if !device.is_mac {
            progress.send("Installing...", 70);
            install_with_progress(device, &app.path, progress, 70, 25).await?;
        } else {
            progress.send("Installing...", 90);
            plume_utils::install_app_mac(&app.path)
                .await
                .map_err(|e| format!("Failed to install app on Mac: {e}"))?;
        }
        "resigned_and_reinstalled"
    } else {
        drop(identity);
        progress.send("Updating provisioning profiles...", 40);
        let mut signer = Signer::new(None, options);
        signer
            .register_bundle(&bundle, &session, &team_id, true)
            .await
            .map_err(|e| format!("Failed to register bundle: {e}"))?;

        progress.send("Installing provisioning profiles...", 80);
        for provision in &signer.provisioning_files {
            device
                .install_profile(provision)
                .await
                .map_err(|e| format!("Failed to install profile: {e}"))?;
        }
        "profiles_updated"
    };

    let next = update_refresh_schedule(refresh_device, app).await?;
    progress.send(format!("Refreshed {label}"), 100);
    log::info!("Refreshed {label} on {}; next refresh {next}", refresh_device.name);

    Ok(json!({
        "app": label,
        "bundle_id": app.bundle_id,
        "device": refresh_device.name,
        "action": action,
        "certificate_was_new": identity_is_new,
        "was_installed": is_installed,
        "next_refresh": next,
    }))
}

/// Recomputes the next refresh from the (re-registered) embedded profile and
/// persists it. Reloads the store so concurrent changes aren't overwritten.
async fn update_refresh_schedule(
    refresh_device: &RefreshDevice,
    app: &RefreshApp,
) -> Result<String, String> {
    let prov_path = app.path.join("embedded.mobileprovision");
    if !prov_path.exists() {
        return Err("embedded.mobileprovision not found".to_string());
    }
    let provision = MobileProvision::load_with_path(&prov_path)
        .map_err(|e| format!("Failed to load mobile provision: {e}"))?;
    let scheduled_refresh = scheduled_refresh_from_provision(&provision);

    let mut store = common::load_store().await?;
    let mut updated = store
        .get_refresh_device(&refresh_device.udid)
        .cloned()
        .unwrap_or_else(|| refresh_device.clone());

    if let Some(existing) = updated.apps.iter_mut().find(|a| a.path == app.path) {
        existing.scheduled_refresh = scheduled_refresh;
    }

    store
        .add_or_update_refresh_device_sync(updated)
        .map_err(|e| format!("Failed to update refresh schedule: {e}"))?;

    Ok(scheduled_refresh.to_rfc3339())
}
