//! Mach-O tools: inspect load commands / entitlements and edit dylib load
//! commands or the SDK version of a binary.

use std::path::{Path, PathBuf};

use plume_core::{MachO, MachOExt};
use plume_utils::{Bundle, PlistInfoTrait};
use serde_json::{Value, json};

use crate::common::{self, Ctx, ToolResult, req_str, schema, tool};

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "macho_info",
            "Inspect a Mach-O binary: per-architecture slices (CPU type, platform, minimum OS and SDK \
             version), dylib load commands (LC_LOAD_DYLIB, weak, reexport, ...), embedded code-signing \
             entitlements (as JSON and as an XML plist string) and app groups. `path` may be the \
             binary itself or an .app bundle directory (its main executable is resolved from \
             Info.plist). For an .ipa, extract it first (e.g. with `package_inspect`).",
            schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Absolute path to a Mach-O executable/dylib/framework binary, or to an .app directory."
                    }
                }),
                &["path"],
            ),
        ),
        tool(
            "macho_edit",
            "Modify a Mach-O binary IN PLACE (all architecture slices; the file is overwritten - copy it \
             first if you need the original). Operations: \"add_dylib\" adds a weak LC_LOAD_WEAK_DYLIB \
             for `dylib` (e.g. @rpath/Tweak.dylib or @executable_path/Frameworks/x.dylib; no-op if \
             present; fails if there is no header padding left); \"replace_dylib\" rewrites \
             `old_dylib` to `new_dylib` (new path must fit in the existing command); \
             \"set_sdk_version\" sets the LC_BUILD_VERSION SDK \
             to `sdk_version` (X.Y.Z, e.g. 26.0.0). `path` may be the binary or an .app directory \
             (edits its main executable). Editing invalidates the code signature: the bundle must \
             be re-signed before installing.",
            schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the Mach-O binary, or an .app directory (main executable is edited)."
                    },
                    "operation": {
                        "type": "string",
                        "enum": ["add_dylib", "replace_dylib", "set_sdk_version"],
                        "description": "Edit to perform."
                    },
                    "dylib": {
                        "type": "string",
                        "description": "Dylib install path for add_dylib."
                    },
                    "old_dylib": {
                        "type": "string",
                        "description": "Existing dylib load path to replace (replace_dylib)."
                    },
                    "new_dylib": {
                        "type": "string",
                        "description": "Replacement dylib load path (replace_dylib)."
                    },
                    "sdk_version": {
                        "type": "string",
                        "description": "New SDK version as X.Y.Z, e.g. 26.0.0 (set_sdk_version)."
                    }
                }),
                &["path", "operation"],
            ),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, _ctx: &Ctx) -> Option<ToolResult> {
    let result = match name {
        "macho_info" => macho_info(args),
        "macho_edit" => macho_edit(args),
        _ => return None,
    };
    Some(result)
}

/// Resolves `path` to a Mach-O file: either the file itself or, for a
/// bundle directory, its main executable.
fn resolve_binary(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }

    let bundle = Bundle::new(path.to_path_buf()).map_err(|e| {
        format!(
            "{} is a directory but not a valid bundle ({e}). Pass the binary path directly.",
            path.display()
        )
    })?;
    let executable = bundle.get_executable().ok_or_else(|| {
        format!(
            "CFBundleExecutable missing from {}/Info.plist. Pass the binary path directly.",
            path.display()
        )
    })?;

    let binary = bundle.bundle_dir().join(&executable);
    if !binary.is_file() {
        return Err(format!(
            "Bundle executable '{executable}' not found at {}",
            binary.display()
        ));
    }
    Ok(binary)
}

fn open_macho(binary: &Path) -> Result<MachO, String> {
    MachO::new(binary).map_err(|e| {
        format!(
            "Failed to parse {} as a Mach-O binary: {e}",
            binary.display()
        )
    })
}

fn cpu_name(cputype: u32, cpusubtype: u32) -> String {
    const ABI64: u32 = 0x0100_0000;
    const ABI64_32: u32 = 0x0200_0000;
    match cputype {
        12 => "armv7".to_string(),
        t if t == 12 | ABI64 => {
            if cpusubtype & 0x00ff_ffff == 2 {
                "arm64e".to_string()
            } else {
                "arm64".to_string()
            }
        }
        t if t == 12 | ABI64_32 => "arm64_32".to_string(),
        7 => "i386".to_string(),
        t if t == 7 | ABI64 => "x86_64".to_string(),
        other => format!("cputype {other:#x}"),
    }
}

fn macho_info(args: &Value) -> ToolResult {
    let path = common::req_existing_path(args, "path")?;
    let binary = resolve_binary(&path)?;
    let macho = open_macho(&binary)?;

    let file = macho.macho_file();
    let mut slices = Vec::new();
    for (i, slice) in file.iter_macho().enumerate() {
        let header = &slice.macho.header;
        let dylibs = match slice.dylib_load_paths() {
            Ok(d) => json!(d),
            Err(e) => json!(format!("<failed to read dylibs: {e}>")),
        };
        let targeting = match slice.find_targeting() {
            Ok(Some(t)) => json!({
                "platform": t.platform.to_string(),
                "minimum_os_version": t.minimum_os_version.to_string(),
                "sdk_version": t.sdk_version.to_string(),
            }),
            Ok(None) => Value::Null,
            Err(e) => json!(format!("<failed to read targeting: {e}>")),
        };
        let signed = matches!(slice.code_signature(), Ok(Some(_)));
        slices.push(json!({
            "index": i,
            "arch": cpu_name(header.cputype, header.cpusubtype),
            "signed": signed,
            "targeting": targeting,
            "dylib_load_paths": dylibs,
        }));
    }

    let (entitlements_json, entitlements_xml) = match macho.entitlements() {
        Some(ent) => {
            let mut buf = Vec::new();
            let xml = plist::Value::Dictionary(ent.clone())
                .to_writer_xml(&mut buf)
                .ok()
                .and_then(|_| String::from_utf8(buf).ok());
            (common::dict_to_json(ent), json!(xml))
        }
        None => (Value::Null, Value::Null),
    };

    Ok(json!({
        "binary": binary.display().to_string(),
        "is_fat": file.is_fat(),
        "slice_count": slices.len(),
        "slices": slices,
        "entitlements": entitlements_json,
        "entitlements_xml": entitlements_xml,
        "app_groups": macho.app_groups_for_entitlements().unwrap_or_default(),
        "note": "Entitlements are read from the first slice's code signature (null when unsigned)."
    }))
}

fn dylib_paths_slice0(macho: &MachO) -> Vec<String> {
    macho
        .macho_file()
        .nth_macho(0)
        .ok()
        .and_then(|m| m.dylib_load_paths().ok())
        .unwrap_or_default()
}

fn sdk_version_slice0(macho: &MachO) -> Option<String> {
    macho
        .macho_file()
        .nth_macho(0)
        .ok()
        .and_then(|m| m.find_targeting().ok().flatten())
        .map(|t| t.sdk_version.to_string())
}

fn macho_edit(args: &Value) -> ToolResult {
    let path = common::req_existing_path(args, "path")?;
    let operation = req_str(args, "operation")?;
    let binary = resolve_binary(&path)?;
    let mut macho = open_macho(&binary)?;

    let before = dylib_paths_slice0(&macho);

    let detail = match operation.as_str() {
        "add_dylib" => {
            let dylib = req_str(args, "dylib")?;
            if before.contains(&dylib) {
                return Ok(json!({
                    "binary": binary.display().to_string(),
                    "operation": operation,
                    "changed": false,
                    "message": format!("'{dylib}' is already a load command; nothing changed."),
                    "dylib_load_paths": before,
                }));
            }
            macho.add_dylib(&dylib).map_err(|e| {
                format!(
                    "Failed to add '{dylib}': {e}. There may not be enough free space after \
                     the load commands in the header."
                )
            })?;
            format!("Added weak load command for '{dylib}'")
        }
        "replace_dylib" => {
            let old = req_str(args, "old_dylib")?;
            let new = req_str(args, "new_dylib")?;
            if !before.contains(&old) {
                return Err(format!(
                    "'{old}' is not a dylib load path of this binary. Current paths: {}",
                    before.join(", ")
                ));
            }
            macho.replace_dylib(&old, &new).map_err(|e| {
                format!(
                    "Failed to replace '{old}' with '{new}': {e}. The new path must fit in \
                     the existing load command (use a path no longer than the old one)."
                )
            })?;
            format!("Replaced '{old}' with '{new}'")
        }
        // plume_core's `remove_dylib` (v2.6.5) shifts and truncates everything
        // after the load commands, corrupting the binary, so it is not exposed.
        "remove_dylib" => {
            return Err(
                "remove_dylib is not supported: Impactor's implementation corrupts the binary. \
                 Use replace_dylib to point the load command at a different path instead."
                    .to_string(),
            );
        }
        "set_sdk_version" => {
            let version = req_str(args, "sdk_version")?;
            let parts: Vec<&str> = version.split('.').collect();
            if parts.len() != 3 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
                return Err(format!(
                    "Invalid sdk_version '{version}': expected X.Y.Z, e.g. 26.0.0"
                ));
            }
            macho
                .replace_sdk_version(&version)
                .map_err(|e| format!("Failed to set SDK version to {version}: {e}"))?;
            format!("Set SDK version to {version}")
        }
        other => {
            return Err(format!(
                "Unknown operation '{other}'. Use add_dylib, replace_dylib or set_sdk_version."
            ));
        }
    };

    // Re-read from disk to report the resulting state.
    let after = open_macho(&binary)?;

    Ok(json!({
        "binary": binary.display().to_string(),
        "operation": operation,
        "changed": true,
        "message": detail,
        "dylib_load_paths": dylib_paths_slice0(&after),
        "sdk_version": sdk_version_slice0(&after),
        "note": "The binary was modified in place and its code signature is now invalid; re-sign the bundle (e.g. with `sign_and_install` / signing tools) before installing."
    }))
}
