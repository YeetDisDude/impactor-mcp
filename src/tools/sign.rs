//! Package inspection and the main sign / install / export pipeline.
//!
//! `sign_and_install` mirrors the GUI's `run_installation`
//! (apps/plumeimpactor/src/subscriptions.rs) and plumesign's `sign` command.

use std::cell::Cell;
use std::fmt::Display;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use plume_core::{CertificateIdentity, MobileProvision};
use plume_utils::{
    Bundle, Device, Package, PlistInfoTrait, Signer, SignerApp, SignerInstallMode, SignerMode,
    SignerOptions,
};
use serde_json::{Value, json};

use crate::common::{
    self, CERT_RESET_HINT, Ctx, Progress, ToolResult, bool_or, data_path, opt_bool, opt_path,
    opt_str, req_existing_path, str_list,
};

pub fn definitions() -> Vec<Value> {
    vec![
        common::tool(
            "package_inspect",
            "Inspect an iOS app package (.ipa file or .app directory) without modifying it. Returns \
             display name, bundle identifier, version, build number, executable, the detected \
             special app type (e.g. SideStore, LiveContainer, Feather, StikDebug - these get \
             special handling when signing), whether 'single_profile' is enabled by default for it, \
             and whether a device pairing file can be installed into the app after installation \
             (and where). Optionally writes the app icon as PNG to `icon_output_path`.",
            common::schema(
                json!({
                    "path": { "type": "string", "description": "Path to a .ipa file or a .app bundle directory." },
                    "icon_output_path": { "type": "string", "description": "Optional file path where the app icon (PNG) is written, if the package has one." }
                }),
                &["path"],
            ),
        ),
        common::tool(
            "sign_and_install",
            "Sign an iOS app (.ipa or .app) and install it on a device and/or export the signed .ipa. \
             The original file is never modified (it is staged in a temp dir). Modes: \
             'apple_id' (default; uses the saved Apple account to get a development certificate, \
             registers the device and app IDs on the developer portal and fetches provisioning \
             profiles; bundle id gets the team id appended unless custom_identifier is given), \
             'adhoc' (ad-hoc signature, no certificate - only useful for jailbroken/TrollStore-like \
             setups or Mac targets), 'pem' (sign with your own certificate/key PEM files and an \
             optional .mobileprovision; no portal access), 'none' (install/export the package \
             unmodified). Set `install` (default true) and/or `output_path`. Use `device_list` to \
             find the udid (optional when exactly one device is connected). `refresh: true` saves \
             the signed app for automatic re-signing before the 7-day free-account profile \
             expires (apple_id mode + install only). Leave `allow_certificate_reset` false unless \
             the user explicitly agreed to revoke existing certificates. Long running; reports \
             progress.",
            common::schema(
                json!({
                    "package_path": { "type": "string", "description": "Path to the .ipa file or .app directory to sign." },
                    "mode": { "type": "string", "enum": ["apple_id", "adhoc", "pem", "none"], "description": "Signing mode (default apple_id)." },
                    "udid": { "type": "string", "description": "Target device UDID (or usbmuxd device id, or 'mac' for this Apple Silicon Mac). Optional if exactly one device is connected." },
                    "install": { "type": "boolean", "description": "Install on the device after signing (default true)." },
                    "output_path": { "type": "string", "description": "Export the signed .ipa to this file path (or into this existing directory)." },
                    "email": { "type": "string", "description": "Apple account email (apple_id mode). Defaults to the selected account." },
                    "team_id": { "type": "string", "description": "Developer team id (apple_id mode). Defaults to the account's stored team, else the first team." },
                    "custom_name": { "type": "string", "description": "Override the app display name." },
                    "custom_identifier": { "type": "string", "description": "Override the bundle identifier (embedded extensions are renamed to match)." },
                    "custom_version": { "type": "string", "description": "Override the version string." },
                    "custom_icon": { "type": "string", "description": "Path to an image file to use as the new app icon." },
                    "custom_entitlements": { "type": "string", "description": "Path to an entitlements .plist to sign the main app with. Only used when single_profile is true." },
                    "single_profile": { "type": "boolean", "description": "Register/provision only the main app (not app extensions). Default depends on the app type (true for LiveContainer)." },
                    "tweaks": { "type": "array", "items": { "type": "string" }, "description": "Paths of tweaks to inject: .deb, .dylib, .framework, .bundle or .appex. Injecting tweaks also injects ElleKit." },
                    "support_minimum_os_version": { "type": "boolean", "description": "Set MinimumOSVersion to 7.0 so the app installs on older iOS versions." },
                    "support_file_sharing": { "type": "boolean", "description": "Enable Files app / iTunes file sharing (UIFileSharingEnabled + UISupportsDocumentBrowser)." },
                    "support_ipad_fullscreen": { "type": "boolean", "description": "Set UIRequiresFullScreen (iPad full screen)." },
                    "support_game_mode": { "type": "boolean", "description": "Set GCSupportsGameMode." },
                    "support_pro_motion": { "type": "boolean", "description": "Enable 120Hz ProMotion on iPhone (CADisableMinimumFrameDurationOnPhone)." },
                    "support_liquid_glass": { "type": "boolean", "description": "Opt into the iOS 26 Liquid Glass design (patches the executable's SDK version to 26.0)." },
                    "support_ellekit": { "type": "boolean", "description": "Inject ElleKit (tweak loader) even without tweaks." },
                    "refresh": { "type": "boolean", "description": "Save the signed app for automatic refresh (apple_id mode with install only). Default false." },
                    "install_pairing_file": { "type": "boolean", "description": "After install, copy the device pairing file into apps that support it (SideStore, Feather, StikDebug, ...). Default true." },
                    "pem_files": { "type": "array", "items": { "type": "string" }, "description": "pem mode: PEM files containing the signing certificate and private key." },
                    "provisioning_profile": { "type": "string", "description": "pem mode: path to a .mobileprovision to embed." },
                    "allow_certificate_reset": { "type": "boolean", "description": "Allow revoking existing development certificates if a new one is needed and the limit is reached (breaks existing SideStore/AltStore installs). Default false." }
                }),
                &["package_path"],
            ),
        ),
        common::tool(
            "sign_list_app_types",
            "List the special app types Impactor recognizes (by bundle id or name) and the extra \
             handling each gets when signing: certificate embedding (SideStore/AltStore), default \
             single_profile, and pairing-file installation path.",
            common::schema(json!({}), &[]),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult> {
    match name {
        "package_inspect" => Some(package_inspect(args)),
        "sign_and_install" => Some(sign_and_install(args, ctx).await),
        "sign_list_app_types" => Some(Ok(list_app_types())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by refresh.rs)
// ---------------------------------------------------------------------------

fn e<E: Display>(context: &'static str) -> impl Fn(E) -> String {
    move |err| format!("{context}: {err}")
}

/// Maps a certificate-step error, turning a declined reset into the shared hint.
pub(crate) fn map_cert_err(err: impl Display) -> String {
    let s = err.to_string();
    if s.contains("Certificate reset cancelled") {
        CERT_RESET_HINT.to_string()
    } else {
        format!("Failed to obtain a signing certificate: {s}")
    }
}

/// Converts a provisioning profile's expiration date to UTC.
pub(crate) fn provision_expiration(provision: &MobileProvision) -> Option<DateTime<Utc>> {
    provision
        .expiration_date()
        .to_xml_format()
        .parse::<DateTime<Utc>>()
        .ok()
}

/// Expiration of `<app_dir>/embedded.mobileprovision`, if present and readable.
pub(crate) fn bundle_profile_expiration(app_dir: &Path) -> Option<DateTime<Utc>> {
    let path = app_dir.join("embedded.mobileprovision");
    if !path.exists() {
        return None;
    }
    MobileProvision::load_with_path(&path)
        .ok()
        .and_then(|p| provision_expiration(&p))
}

/// Same rule as the GUI: refresh 3 days before the profile expires (falls back
/// to now + 4 days when the date can't be parsed).
pub(crate) fn scheduled_refresh_from_provision(provision: &MobileProvision) -> DateTime<Utc> {
    provision_expiration(provision).unwrap_or_else(|| Utc::now() + chrono::Duration::days(4))
        - chrono::Duration::days(3)
}

/// Installs an app directory on an iOS device, forwarding install progress
/// (mapped into `base..=base+span`) to `progress`.
pub(crate) async fn install_with_progress(
    device: &Device,
    app_dir: &PathBuf,
    progress: &Progress,
    base: i32,
    span: i32,
) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<i32>();
    let done = Cell::new(false);

    let install = async {
        let result = device
            .install_app(app_dir, move |p: i32| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(p);
                }
            })
            .await;
        done.set(true);
        result
    };

    let reporter = async {
        let mut last = -1;
        loop {
            let finished = done.get();
            while let Ok(p) = rx.try_recv() {
                let pct = base + (p.clamp(0, 100) * span / 100);
                if pct != last {
                    last = pct;
                    progress.send("Installing...", pct);
                }
            }
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };

    let (result, ()) = futures::future::join(install, reporter).await;
    result.map_err(|err| format!("Install error: {err}"))
}

/// Saves a signed bundle for auto-refresh exactly like the GUI: copies it to
/// `<data>/refresh_store/<name>-<uuid>.app` and upserts the `RefreshDevice`
/// entry (keyed by UDID) with the app and its next scheduled refresh.
pub(crate) async fn save_for_refresh(
    bundle: &Bundle,
    device: &Device,
    account_email: &str,
) -> Result<Value, String> {
    let store_dir = data_path().join("refresh_store");
    tokio::fs::create_dir_all(&store_dir)
        .await
        .map_err(e("Failed to create refresh store"))?;

    let original_name = bundle
        .bundle_dir()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "App.app".to_string());
    let uuid = uuid::Uuid::new_v4();
    let dest_name = match original_name.rfind('.') {
        Some(dot) => {
            let (name, ext) = original_name.split_at(dot);
            format!("{name}-{uuid}{ext}")
        }
        None => format!("{original_name}-{uuid}"),
    };
    let dest_path = store_dir.join(dest_name);

    plume_utils::copy_dir_recursively(bundle.bundle_dir(), &dest_path)
        .await
        .map_err(e("Failed to copy app into refresh store"))?;

    let prov_path = dest_path.join("embedded.mobileprovision");
    if !prov_path.exists() {
        return Err(format!(
            "Saved app has no embedded.mobileprovision; cannot schedule refresh ({})",
            dest_path.display()
        ));
    }
    let provision =
        MobileProvision::load_with_path(&prov_path).map_err(e("Failed to read provisioning profile"))?;
    let scheduled_refresh = scheduled_refresh_from_provision(&provision);

    let refresh_app = plume_store::RefreshApp {
        name: bundle.get_name(),
        bundle_id: bundle.get_bundle_identifier(),
        path: dest_path.clone(),
        scheduled_refresh,
    };

    let mut store = common::load_store().await?;
    let mut refresh_device = store
        .get_refresh_device(&device.udid)
        .cloned()
        .unwrap_or_else(|| plume_store::RefreshDevice {
            udid: device.udid.clone(),
            name: device.name.clone(),
            account: account_email.to_string(),
            apps: Vec::new(),
            is_mac: device.is_mac,
        });
    // The newest install decides which account refreshes the device's apps.
    refresh_device.account = account_email.to_string();

    let mut replaced_path = None;
    if let Some(existing) = refresh_device
        .apps
        .iter_mut()
        .find(|a| a.bundle_id == refresh_app.bundle_id)
    {
        replaced_path = Some(existing.path.clone());
        *existing = refresh_app;
    } else {
        refresh_device.apps.push(refresh_app);
    }

    store
        .add_or_update_refresh_device_sync(refresh_device)
        .map_err(e("Failed to save refresh entry"))?;

    // Remove the previous saved copy of the same app (it is no longer referenced).
    if let Some(old) = replaced_path {
        if old != dest_path && old.starts_with(&store_dir) && old.exists() {
            fs::remove_dir_all(&old).ok();
        }
    }

    Ok(json!({
        "saved_path": dest_path.display().to_string(),
        "scheduled_refresh": scheduled_refresh.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// Package loading (.ipa or .app)
// ---------------------------------------------------------------------------

/// Loads a package. A `.app` directory is zipped (read-only) into a temporary
/// `.ipa` first so everything goes through `plume_utils::Package`.
fn load_package(path: &Path) -> Result<Package, String> {
    if path.is_dir() {
        if !path.join("Info.plist").exists() {
            return Err(format!(
                "{} is a directory but not an app bundle (no Info.plist)",
                path.display()
            ));
        }
        let tmp = std::env::temp_dir().join(format!("impactor-mcp_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).map_err(e("Failed to create temp dir"))?;
        let ipa = tmp.join("input.ipa");
        let result = zip_app_dir(path, &ipa).and_then(|_| {
            Package::new(ipa.clone()).map_err(e("Failed to open package"))
        });
        fs::remove_dir_all(&tmp).ok();
        result
    } else {
        Package::new(path.to_path_buf()).map_err(e("Failed to open package"))
    }
}

fn zip_app_dir(app: &Path, out: &Path) -> Result<(), String> {
    let mut app_name = app
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| "Invalid .app path".to_string())?;
    if !app_name.to_ascii_lowercase().ends_with(".app") {
        app_name.push_str(".app");
    }

    let file = fs::File::create(out).map_err(e("Failed to create temp ipa"))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let prefix = format!("Payload/{app_name}");
    zip.add_directory("Payload/", opts).map_err(e("Zip error"))?;
    zip.add_directory(format!("{prefix}/"), opts)
        .map_err(e("Zip error"))?;
    add_dir_to_zip(&mut zip, app, &prefix, opts)?;
    zip.finish().map_err(e("Zip error"))?;
    Ok(())
}

fn add_dir_to_zip(
    zip: &mut zip::ZipWriter<fs::File>,
    dir: &Path,
    prefix: &str,
    opts: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(e("Failed to read app directory"))? {
        let entry = entry.map_err(e("Failed to read app directory"))?;
        let path = entry.path();
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        // Follows symlinks.
        let meta = fs::metadata(&path).map_err(e("Failed to read file metadata"))?;

        if meta.is_dir() {
            zip.add_directory(format!("{name}/"), opts)
                .map_err(e("Zip error"))?;
            add_dir_to_zip(zip, &path, &name, opts)?;
        } else {
            #[allow(unused_mut)]
            let mut file_opts = opts.large_file(meta.len() >= u32::MAX as u64);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file_opts = file_opts.unix_permissions(meta.permissions().mode());
            }
            zip.start_file(name, file_opts).map_err(e("Zip error"))?;
            let mut f = fs::File::open(&path).map_err(e("Failed to read file"))?;
            std::io::copy(&mut f, zip).map_err(e("Failed to write zip"))?;
        }
    }
    zip.flush().ok();
    Ok(())
}

fn app_type_name(app: SignerApp) -> String {
    match app {
        SignerApp::LiveContainerAndSideStore => "LiveContainer + SideStore".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// package_inspect
// ---------------------------------------------------------------------------

fn package_inspect(args: &Value) -> ToolResult {
    let path = req_existing_path(args, "path")?;
    let package = load_package(&path)?;

    let result = (|| {
        let mut options = SignerOptions::default();
        package.load_into_signer_options(&mut options);

        let mut icon_written = Value::Null;
        if let Some(out) = opt_path(args, "icon_output_path") {
            match &package.app_icon_data {
                Some(data) => {
                    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                        fs::create_dir_all(parent).ok();
                    }
                    fs::write(&out, data).map_err(e("Failed to write icon"))?;
                    icon_written = json!(out.display().to_string());
                }
                None => icon_written = json!("package has no icon"),
            }
        }

        Ok(json!({
            "name": package.get_name(),
            "bundle_name": package.get_bundle_name(),
            "bundle_identifier": package.get_bundle_identifier(),
            "version": package.get_version(),
            "build": package.get_build_version(),
            "executable": package.get_executable(),
            "app_type": app_type_name(options.app),
            "app_type_id": format!("{:?}", options.app),
            "single_profile_default": options.embedding.single_profile,
            "supports_pairing_file": options.app.supports_pairing_file(),
            "pairing_file_path": options.app.pairing_file_path(),
            "has_icon": package.app_icon_data.is_some(),
            "icon_output_path": icon_written,
        }))
    })();

    package.remove_package_stage();
    result
}

// ---------------------------------------------------------------------------
// sign_list_app_types
// ---------------------------------------------------------------------------

fn list_app_types() -> Value {
    use SignerApp::*;
    let entries: &[(SignerApp, &[&str])] = &[
        (Default, &[]),
        (Antrag, &["thewonderofyou.antrag2"]),
        (Feather, &["thewonderofyou.Feather"]),
        (Protokolle, &["thewonderofyou.syslog"]),
        (AltStore, &["com.rileytestut.AltStore"]),
        (SideStore, &["com.SideStore.SideStore"]),
        (LiveContainer, &["com.kdt.livecontainer"]),
        (LiveContainerAndSideStore, &["(LiveContainer bundle containing SideStoreApp.framework)"]),
        (StikDebug, &["com.stik.sj"]),
        (SparseBox, &["com.kdt.SparseBox"]),
        (EnsWilde, &["com.yangjiii.EnsWilde"]),
        (ByeTunes, &["com.EduAlexxis.MusicManager"]),
        (StikStore, &["me.stik.store", "app.stik.store"]),
        (Reynard, &["com.minh-ton.Reynard"]),
        (Ksign, &["nya.asami.ksign"]),
        (AutoCapture, &["com.halfeatentoast.devcapture"]),
    ];

    let list: Vec<Value> = entries
        .iter()
        .map(|(app, ids)| {
            let defaults = SignerOptions::new_for_app(*app);
            json!({
                "app_type": app_type_name(*app),
                "app_type_id": format!("{app:?}"),
                "detected_by_bundle_id_containing": ids,
                "single_profile_default": defaults.embedding.single_profile,
                "embeds_signing_certificate": matches!(app, SideStore | AltStore | LiveContainerAndSideStore),
                "sets_alt_app_groups": matches!(app, SideStore | AltStore),
                "supports_pairing_file": app.supports_pairing_file(),
                "pairing_file_path": app.pairing_file_path(),
            })
        })
        .collect();

    json!({
        "app_types": list,
        "notes": "Detection uses the package's bundle identifier (substring match). Apps that \
                  embed the signing certificate (ALTCertificate.p12 + ALTCertificateID) can \
                  re-sign apps on-device. Pairing files are installed only for iOS device \
                  installs when install_pairing_file is true."
    })
}

// ---------------------------------------------------------------------------
// sign_and_install
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    AppleId,
    Adhoc,
    Pem,
    None,
}

impl Mode {
    fn parse(s: Option<String>) -> Result<Self, String> {
        match s.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("apple_id") | Some("appleid") | Some("apple-id") => Ok(Mode::AppleId),
            Some("adhoc") | Some("ad-hoc") | Some("ad_hoc") => Ok(Mode::Adhoc),
            Some("pem") => Ok(Mode::Pem),
            Some("none") | Some("no_modify") => Ok(Mode::None),
            Some(other) => Err(format!(
                "Unknown mode '{other}'. Use apple_id, adhoc, pem or none."
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Mode::AppleId => "apple_id",
            Mode::Adhoc => "adhoc",
            Mode::Pem => "pem",
            Mode::None => "none",
        }
    }
}

async fn sign_and_install(args: &Value, ctx: &Ctx) -> ToolResult {
    let package_path = req_existing_path(args, "package_path")?;
    let mode = Mode::parse(opt_str(args, "mode"))?;
    let install = bool_or(args, "install", true);
    let output_path = opt_path(args, "output_path");
    let refresh = bool_or(args, "refresh", false);

    if !install && output_path.is_none() {
        return Err("Nothing to do: set `install: true` and/or `output_path`.".into());
    }
    if refresh && (mode != Mode::AppleId || !install) {
        return Err(
            "`refresh` is only supported in apple_id mode when installing to a device.".into(),
        );
    }

    let pem_files: Vec<PathBuf> = str_list(args, "pem_files")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if mode == Mode::Pem && pem_files.is_empty() {
        return Err("mode 'pem' requires `pem_files` (certificate and private key PEM files).".into());
    }
    for p in &pem_files {
        if !p.exists() {
            return Err(format!("PEM file not found: {}", p.display()));
        }
    }
    let provisioning_profile = opt_path(args, "provisioning_profile");
    if let Some(p) = &provisioning_profile {
        if !p.exists() {
            return Err(format!("Provisioning profile not found: {}", p.display()));
        }
    }

    let tweaks: Vec<PathBuf> = str_list(args, "tweaks")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    for t in &tweaks {
        if !t.exists() {
            return Err(format!("Tweak not found: {}", t.display()));
        }
    }
    for key in ["custom_icon", "custom_entitlements"] {
        if let Some(p) = opt_path(args, key) {
            if !p.exists() {
                return Err(format!("`{key}`: file not found: {}", p.display()));
            }
        }
    }

    // Resolve the device up front so we fail before any signing work.
    let udid_arg = opt_str(args, "udid");
    let device = if install || (mode == Mode::AppleId && udid_arg.is_some()) {
        let device = common::find_device(udid_arg.as_deref()).await?;
        if !device.is_mac {
            common::require_usb_device(&device)?;
        }
        Some(device)
    } else {
        None
    };

    ctx.progress.send("Preparing package...", 10);
    let package = load_package(&package_path)?;

    let result = run_pipeline(
        args,
        ctx,
        &package,
        mode,
        device.as_ref(),
        install,
        output_path,
        refresh,
        pem_files,
        provisioning_profile,
        tweaks,
    )
    .await;

    package.remove_package_stage();
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    args: &Value,
    ctx: &Ctx,
    package: &Package,
    mode: Mode,
    device: Option<&Device>,
    install: bool,
    output_path: Option<PathBuf>,
    refresh: bool,
    pem_files: Vec<PathBuf>,
    provisioning_profile: Option<PathBuf>,
    tweaks: Vec<PathBuf>,
) -> ToolResult {
    let progress = &ctx.progress;
    let mut notes: Vec<String> = Vec::new();

    // Options: start from the app-type defaults, then apply overrides.
    let mut options = SignerOptions::default();
    package.load_into_signer_options(&mut options);
    options.mode = match mode {
        Mode::AppleId | Mode::Pem => SignerMode::Pem,
        Mode::Adhoc => SignerMode::Adhoc,
        Mode::None => SignerMode::None,
    };
    options.install_mode = if install {
        SignerInstallMode::Install
    } else {
        SignerInstallMode::Export
    };
    options.custom_name = opt_str(args, "custom_name");
    options.custom_identifier = opt_str(args, "custom_identifier");
    options.custom_version = opt_str(args, "custom_version");
    options.custom_icon = opt_path(args, "custom_icon");
    options.custom_entitlements = opt_path(args, "custom_entitlements");
    if let Some(sp) = opt_bool(args, "single_profile") {
        options.embedding.single_profile = sp;
    }
    if options.custom_entitlements.is_some() && !options.embedding.single_profile {
        notes.push("custom_entitlements ignored because single_profile is false".into());
    }
    options.tweaks = if tweaks.is_empty() { None } else { Some(tweaks) };
    {
        let f = &mut options.features;
        let set = |key: &str, field: &mut bool| {
            if let Some(v) = opt_bool(args, key) {
                *field = v;
            }
        };
        set("support_minimum_os_version", &mut f.support_minimum_os_version);
        set("support_file_sharing", &mut f.support_file_sharing);
        set("support_ipad_fullscreen", &mut f.support_ipad_fullscreen);
        set("support_game_mode", &mut f.support_game_mode);
        set("support_pro_motion", &mut f.support_pro_motion);
        set("support_liquid_glass", &mut f.support_liquid_glass);
        set("support_ellekit", &mut f.support_ellekit);
    }
    options.refresh = refresh;
    if mode == Mode::None
        && (options.custom_name.is_some()
            || options.custom_identifier.is_some()
            || options.custom_version.is_some()
            || options.custom_icon.is_some()
            || options.tweaks.is_some())
    {
        notes.push("mode 'none' installs the package unmodified; customizations were ignored".into());
    }

    let mut account_email: Option<String> = None;
    let mut team: Option<String> = None;

    let bundle: Bundle = match mode {
        Mode::AppleId => {
            progress.send("Ensuring account is valid...", 20);
            let (account, session) = common::open_session(args).await?;
            let team_id = common::resolve_team(&session, &account, opt_str(args, "team_id")).await?;

            let allow_reset = bool_or(args, "allow_certificate_reset", false);
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

            progress.send("Ensuring device is registered...", 30);
            if let Some(dev) = device {
                session
                    .qh_ensure_device(&team_id, &dev.name, &dev.udid)
                    .await
                    .map_err(e("Failed to register device"))?;
            }

            progress.send("Extracting package...", 50);
            let mut signer = Signer::new(Some(identity), options.clone());
            let bundle = package
                .get_package_bundle()
                .map_err(e("Failed to extract package"))?;

            progress.send("Signing package...", 70);
            signer
                .modify_bundle(&bundle, &Some(team_id.clone()))
                .await
                .map_err(e("Failed to modify bundle"))?;
            signer
                .register_bundle(&bundle, &session, &team_id, false)
                .await
                .map_err(e("Failed to register app on the developer portal"))?;
            signer
                .sign_bundle(&bundle)
                .await
                .map_err(e("Failed to sign bundle"))?;

            options = signer.options.clone();
            account_email = Some(account.email().clone());
            team = Some(team_id);
            bundle
        }
        Mode::Pem => {
            progress.send("Loading certificate...", 20);
            let identity = CertificateIdentity::new_with_paths(Some(pem_files))
                .await
                .map_err(e("Failed to load PEM files"))?;
            if identity.cert.is_none() || identity.key.is_none() {
                notes.push(
                    "PEM files did not contain both a certificate and a private key".into(),
                );
            }

            let mut signer = Signer::new(Some(identity), options.clone());
            if let Some(prov_path) = &provisioning_profile {
                let prov = MobileProvision::load_with_path(prov_path)
                    .map_err(e("Failed to load provisioning profile"))?;
                signer.provisioning_files.push(prov);
            } else {
                notes.push("No provisioning_profile given; signed without one".into());
            }

            progress.send("Extracting package...", 50);
            let bundle = package
                .get_package_bundle()
                .map_err(e("Failed to extract package"))?;

            progress.send("Signing package...", 70);
            signer
                .modify_bundle(&bundle, &None)
                .await
                .map_err(e("Failed to modify bundle"))?;
            signer
                .sign_bundle(&bundle)
                .await
                .map_err(e("Failed to sign bundle"))?;

            options = signer.options.clone();
            bundle
        }
        Mode::Adhoc => {
            progress.send("Extracting package...", 50);
            let mut signer = Signer::new(None, options.clone());
            let bundle = package
                .get_package_bundle()
                .map_err(e("Failed to extract package"))?;

            progress.send("Signing package...", 70);
            signer
                .modify_bundle(&bundle, &None)
                .await
                .map_err(e("Failed to modify bundle"))?;
            signer
                .sign_bundle(&bundle)
                .await
                .map_err(e("Failed to sign bundle"))?;

            options = signer.options.clone();
            bundle
        }
        Mode::None => {
            progress.send("Extracting package...", 50);
            package
                .get_package_bundle()
                .map_err(e("Failed to extract package"))?
        }
    };

    let final_bundle_id = bundle.get_bundle_identifier();
    let final_name = bundle.get_name();

    // Install
    let mut installed = false;
    let mut pairing_result = Value::Null;
    if install {
        let dev = device.ok_or_else(|| "No device available for installation".to_string())?;
        if !dev.is_mac {
            progress.send("Sending to device...", 70);
            install_with_progress(dev, bundle.bundle_dir(), progress, 70, 20).await?;
            installed = true;

            if !bool_or(args, "install_pairing_file", true) {
                pairing_result = json!("skipped (install_pairing_file=false)");
            } else if !options.app.supports_pairing_file() {
                pairing_result = json!("not supported by this app type");
            } else {
                let identifier = options.custom_identifier.clone().or(final_bundle_id.clone());
                match (identifier, options.app.pairing_file_path()) {
                    (Some(id), Some(path)) => {
                        progress.send("Installing pairing file...", 92);
                        pairing_result = match dev.install_pairing_record(&id, path).await {
                            Ok(()) => json!({ "installed": true, "path": path }),
                            Err(err) => json!({ "installed": false, "path": path, "error": err.to_string() }),
                        };
                    }
                    _ => pairing_result = json!("skipped (unknown bundle id or pairing path)"),
                }
            }
        } else {
            progress.send("Installing...", 90);
            plume_utils::install_app_mac(bundle.bundle_dir())
                .await
                .map_err(e("Failed to install app on this Mac"))?;
            installed = true;
        }
    }

    // Export
    let mut exported: Option<String> = None;
    if let Some(out) = output_path {
        progress.send("Exporting...", 93);
        let archive = package
            .get_archive_based_on_path(bundle.bundle_dir())
            .map_err(e("Failed to archive signed package"))?;
        let dest = if out.is_dir() {
            let file_name = format!(
                "{}.ipa",
                final_name.clone().unwrap_or_else(|| "signed".to_string())
            );
            out.join(file_name)
        } else {
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            out
        };
        tokio::fs::copy(&archive, &dest)
            .await
            .map_err(e("Failed to write exported .ipa"))?;
        exported = Some(dest.display().to_string());
    }

    // Save for refresh (apple_id + device install only, enforced earlier).
    let mut refresh_result = Value::Bool(false);
    if refresh && installed {
        if let (Some(dev), Some(email)) = (device, account_email.as_deref()) {
            progress.send("Saving for refresh...", 96);
            refresh_result = match save_for_refresh(&bundle, dev, email).await {
                Ok(v) => v,
                Err(err) => json!({ "error": err }),
            };
        }
    }

    let profile_expiration = bundle_profile_expiration(bundle.bundle_dir()).map(|d| d.to_rfc3339());

    progress.send("Finished!", 100);

    Ok(json!({
        "success": true,
        "mode": mode.as_str(),
        "name": final_name,
        "bundle_identifier": final_bundle_id,
        "version": bundle.get_version(),
        "app_type": app_type_name(options.app),
        "team_id": team,
        "account": account_email,
        "device": device.map(common::device_json),
        "installed": installed,
        "exported_path": exported,
        "pairing_file": pairing_result,
        "refresh": refresh_result,
        "profile_expiration": profile_expiration,
        "notes": notes,
    }))
}
