//! Apple Developer portal operations (certificates, devices, app ids, app
//! groups, provisioning profiles) for the selected (or given) account/team.

use std::path::PathBuf;

use plist::Dictionary;
use plume_core::{CertificateIdentity, MobileProvision, developer::DeveloperSession, developer_endpoint};
use plume_store::GsaAccount;
use serde_json::{Value, json};

use crate::common::{self, Ctx, ToolResult, bool_or, dict_to_json, opt_str, req_str, schema, tool};

fn account_props() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "email".into(),
        json!({ "type": "string", "description": "Account email; defaults to the selected account." }),
    );
    m.insert(
        "team_id".into(),
        json!({ "type": "string", "description": "Team id; defaults to the account's stored team (see `account_teams`)." }),
    );
    m
}

fn props(extra: Value) -> Value {
    let mut m = account_props();
    if let Value::Object(extra) = extra {
        m.extend(extra);
    }
    Value::Object(m)
}

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "portal_account_info",
            "Show Apple Developer account details (name, email, developer status) for a team.",
            schema(props(json!({})), &[]),
        ),
        tool(
            "portal_list_certificates",
            "List the team's development certificates (name, serial number, status, expiration, \
             machine name). Free accounts are limited to a few active certificates.",
            schema(props(json!({})), &[]),
        ),
        tool(
            "portal_revoke_certificate",
            "DESTRUCTIVE: revoke a development certificate by serial number. Apps signed with it \
             (including SideStore/AltStore installs) stop launching. Confirm with the user first.",
            schema(
                props(json!({
                    "serial_number": { "type": "string", "description": "Serial number from `portal_list_certificates`." }
                })),
                &["serial_number"],
            ),
        ),
        tool(
            "portal_list_devices",
            "List devices registered to the team in the developer portal (name, UDID, platform, status).",
            schema(props(json!({})), &[]),
        ),
        tool(
            "portal_register_device",
            "Register a device UDID with the team. If the UDID is already registered, returns the \
             existing registration instead of adding a duplicate.",
            schema(
                props(json!({
                    "udid": { "type": "string", "description": "Device UDID." },
                    "name": { "type": "string", "description": "Display name for the device." }
                })),
                &["udid", "name"],
            ),
        ),
        tool(
            "portal_list_app_ids",
            "List the team's App IDs (app_id_id, bundle identifier, name, features). Free accounts \
             can create a limited number of App IDs per week.",
            schema(props(json!({})), &[]),
        ),
        tool(
            "portal_delete_app_id",
            "DESTRUCTIVE: delete an App ID by its `appIdId` (from `portal_list_app_ids`, not the \
             bundle identifier). Note: on free accounts deleting does not give back App ID quota. \
             Confirm with the user first.",
            schema(
                props(json!({
                    "app_id_id": { "type": "string", "description": "The `appIdId` value from `portal_list_app_ids`." }
                })),
                &["app_id_id"],
            ),
        ),
        tool(
            "portal_list_app_groups",
            "List the team's application groups (group identifiers used for app/extension data sharing).",
            schema(props(json!({})), &[]),
        ),
        tool(
            "portal_download_profile",
            "Download the team provisioning profile for an existing App ID (by bundle identifier) \
             and write it to `output_path` (.mobileprovision). Reports its expiration date.",
            schema(
                props(json!({
                    "bundle_id": { "type": "string", "description": "Bundle identifier of an existing App ID." },
                    "output_path": { "type": "string", "description": "Destination file path (e.g. C:\\\\tmp\\\\app.mobileprovision)." }
                })),
                &["bundle_id", "output_path"],
            ),
        ),
        tool(
            "portal_export_certificate",
            "Export the signing certificate + private key Impactor uses for this team as a .p12 \
             file written to `output_path` (for use with other signing tools). Creates a \
             certificate if none is stored locally. If the account already has the maximum number \
             of certificates, this fails unless `allow_certificate_reset` is true, which REVOKES \
             existing certificates (breaking SideStore/AltStore installs) - only set it with the \
             user's explicit consent.",
            schema(
                props(json!({
                    "output_path": { "type": "string", "description": "Destination .p12 file path." },
                    "allow_certificate_reset": { "type": "boolean", "default": false, "description": "Allow revoking existing certificates if a new one is required." }
                })),
                &["output_path"],
            ),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, ctx: &Ctx) -> Option<ToolResult> {
    let result = match name {
        "portal_account_info" => account_info(args).await,
        "portal_list_certificates" => list_certificates(args).await,
        "portal_revoke_certificate" => revoke_certificate(args).await,
        "portal_list_devices" => list_devices(args).await,
        "portal_register_device" => register_device(args).await,
        "portal_list_app_ids" => list_app_ids(args).await,
        "portal_delete_app_id" => delete_app_id(args).await,
        "portal_list_app_groups" => list_app_groups(args).await,
        "portal_download_profile" => download_profile(args).await,
        "portal_export_certificate" => export_certificate(args, ctx).await,
        _ => return None,
    };
    Some(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Opens a session for the requested account and resolves the team.
async fn open(args: &Value) -> Result<(GsaAccount, DeveloperSession, String), String> {
    let (account, session) = common::open_session(args).await?;
    let team_id = common::resolve_team(&session, &account, opt_str(args, "team_id")).await?;
    Ok((account, session, team_id))
}

/// Sends a raw QH request with `teamId` (plus `extra`) and returns the
/// response converted to JSON.
async fn qh_raw(
    session: &DeveloperSession,
    endpoint: &str,
    team_id: &str,
    extra: &[(&str, &str)],
) -> Result<Value, String> {
    let url = developer_endpoint!(endpoint);
    let mut body = Dictionary::new();
    body.insert("teamId".to_string(), plist::Value::String(team_id.to_string()));
    for (k, v) in extra {
        body.insert((*k).to_string(), plist::Value::String((*v).to_string()));
    }
    let response = session
        .qh_send_request(&url, Some(body))
        .await
        .map_err(|e| format!("Developer portal request {endpoint} failed: {e}"))?;
    Ok(dict_to_json(&response))
}

/// Extracts the array at `key`, removing bulky / noisy fields from each item.
fn extract_list(mut response: Value, key: &str, drop: &[&str]) -> Value {
    let mut list = response
        .get_mut(key)
        .map(Value::take)
        .unwrap_or_else(|| Value::Array(Vec::new()));
    if let Value::Array(items) = &mut list {
        for item in items.iter_mut() {
            if let Value::Object(obj) = item {
                for k in drop {
                    obj.remove(*k);
                }
            }
        }
    }
    list
}

fn count(list: &Value) -> usize {
    list.as_array().map(Vec::len).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

async fn account_info(args: &Value) -> ToolResult {
    let (account, session, team_id) = open(args).await?;
    let mut response = qh_raw(&session, "/QH65B2/viewDeveloper.action", &team_id, &[]).await?;
    let developer = response
        .get_mut("developer")
        .map(Value::take)
        .unwrap_or(Value::Null);
    Ok(json!({
        "email": account.email(),
        "team_id": team_id,
        "developer": developer,
    }))
}

async fn list_certificates(args: &Value) -> ToolResult {
    let (_, session, team_id) = open(args).await?;
    let response = qh_raw(
        &session,
        "/QH65B2/ios/listAllDevelopmentCerts.action",
        &team_id,
        &[],
    )
    .await?;
    let certs = extract_list(response, "certificates", &["certContent"]);
    Ok(json!({ "team_id": team_id, "count": count(&certs), "certificates": certs }))
}

async fn revoke_certificate(args: &Value) -> ToolResult {
    let serial_number = req_str(args, "serial_number")?;
    let (_, session, team_id) = open(args).await?;
    session
        .qh_revoke_cert(&team_id, &serial_number)
        .await
        .map_err(|e| format!("Failed to revoke certificate {serial_number}: {e}"))?;
    Ok(json!({ "team_id": team_id, "revoked_serial_number": serial_number }))
}

async fn list_devices(args: &Value) -> ToolResult {
    let (_, session, team_id) = open(args).await?;
    let response = qh_raw(&session, "/QH65B2/ios/listDevices.action", &team_id, &[]).await?;
    let devices = extract_list(response, "devices", &[]);
    Ok(json!({ "team_id": team_id, "count": count(&devices), "devices": devices }))
}

async fn register_device(args: &Value) -> ToolResult {
    let udid = req_str(args, "udid")?;
    let name = req_str(args, "name")?;
    let (_, session, team_id) = open(args).await?;

    let response = qh_raw(&session, "/QH65B2/ios/listDevices.action", &team_id, &[]).await?;
    let devices = extract_list(response, "devices", &[]);
    if let Some(existing) = devices.as_array().and_then(|list| {
        list.iter().find(|d| {
            d.get("deviceNumber")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(&udid))
        })
    }) {
        return Ok(json!({
            "team_id": team_id,
            "already_registered": true,
            "device": existing,
        }));
    }

    let mut response = qh_raw(
        &session,
        "/QH65B2/ios/addDevice.action",
        &team_id,
        &[("name", name.as_str()), ("deviceNumber", udid.as_str())],
    )
    .await?;
    let device = response
        .get_mut("device")
        .map(Value::take)
        .unwrap_or(Value::Null);
    Ok(json!({
        "team_id": team_id,
        "already_registered": false,
        "device": device,
    }))
}

async fn list_app_ids(args: &Value) -> ToolResult {
    let (_, session, team_id) = open(args).await?;
    let response = qh_raw(&session, "/QH65B2/ios/listAppIds.action", &team_id, &[]).await?;
    let app_ids = extract_list(response, "appIds", &[]);
    Ok(json!({ "team_id": team_id, "count": count(&app_ids), "app_ids": app_ids }))
}

async fn delete_app_id(args: &Value) -> ToolResult {
    let app_id_id = req_str(args, "app_id_id")?;
    let (_, session, team_id) = open(args).await?;
    session
        .qh_delete_app_id(&team_id, &app_id_id)
        .await
        .map_err(|e| format!("Failed to delete App ID {app_id_id}: {e}"))?;
    Ok(json!({ "team_id": team_id, "deleted_app_id_id": app_id_id }))
}

async fn list_app_groups(args: &Value) -> ToolResult {
    let (_, session, team_id) = open(args).await?;
    let response = qh_raw(
        &session,
        "/QH65B2/ios/listApplicationGroups.action",
        &team_id,
        &[],
    )
    .await?;
    let groups = extract_list(response, "applicationGroupList", &[]);
    Ok(json!({ "team_id": team_id, "count": count(&groups), "app_groups": groups }))
}

async fn download_profile(args: &Value) -> ToolResult {
    let bundle_id = req_str(args, "bundle_id")?;
    let output_path = PathBuf::from(req_str(args, "output_path")?);
    let (_, session, team_id) = open(args).await?;

    let app_id = session
        .qh_get_app_id(&team_id, &bundle_id)
        .await
        .map_err(|e| format!("Failed to list App IDs: {e}"))?
        .ok_or_else(|| {
            format!(
                "No App ID with bundle identifier '{bundle_id}' in team {team_id}. \
                 Use `portal_list_app_ids` to see existing ones."
            )
        })?;

    let profiles = session
        .qh_get_profile(&team_id, &app_id.app_id_id)
        .await
        .map_err(|e| format!("Failed to download provisioning profile: {e}"))?;
    let data: Vec<u8> = profiles.provisioning_profile.encoded_profile.as_ref().to_vec();

    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    tokio::fs::write(&output_path, &data)
        .await
        .map_err(|e| format!("Failed to write {}: {e}", output_path.display()))?;

    let expiration = MobileProvision::load_with_bytes(data.clone())
        .map(|p| p.expiration_date().to_xml_format())
        .ok();

    Ok(json!({
        "team_id": team_id,
        "bundle_id": bundle_id,
        "app_id_id": app_id.app_id_id,
        "output_path": output_path.display().to_string(),
        "size_bytes": data.len(),
        "filename": profiles.provisioning_profile.filename,
        "expiration_date": expiration,
    }))
}

async fn export_certificate(args: &Value, ctx: &Ctx) -> ToolResult {
    let output_path = PathBuf::from(req_str(args, "output_path")?);
    let allow_reset = bool_or(args, "allow_certificate_reset", false);
    let (_, session, team_id) = open(args).await?;

    ctx.progress.send("Fetching signing certificate", 10);

    let mut on_reset = common::cert_reset_callback(allow_reset, &ctx.progress);
    let identity = CertificateIdentity::new_with_session(
        &session,
        common::data_path(),
        None,
        &team_id,
        true,
        Some(&mut on_reset),
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("Certificate reset cancelled") {
            common::CERT_RESET_HINT.to_string()
        } else {
            format!("Failed to obtain certificate: {msg}")
        }
    })?;

    let p12 = identity
        .p12_data
        .ok_or_else(|| "Certificate was obtained but no .p12 data was produced".to_string())?;

    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    tokio::fs::write(&output_path, &p12)
        .await
        .map_err(|e| format!("Failed to write {}: {e}", output_path.display()))?;

    ctx.progress.send("Certificate exported", 100);

    Ok(json!({
        "team_id": team_id,
        "output_path": output_path.display().to_string(),
        "size_bytes": p12.len(),
        "p12_password": "",
        "note": "The exported .p12 has an empty password.",
        "serial_number": identity.serial_number,
        "machine_id": identity.machine_id,
        "newly_created": identity.new,
    }))
}
