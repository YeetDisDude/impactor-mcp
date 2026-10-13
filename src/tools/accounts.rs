//! Apple ID account management: login (with multi-call 2FA), switching,
//! removal, team selection and app settings.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plume_core::auth::{Account, TwoFactorAction, TwoFactorMethod, TwoFactorRequest};
use plume_store::AccountStore;
use serde_json::{Value, json};

use crate::common::{self, Ctx, ToolResult, opt_str, opt_u64, req_str, schema, tool};

/// How long the login thread waits for the user to supply a 2FA code.
const TFA_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a single MCP call waits for the next login event before returning
/// `in_progress`.
const CALL_WAIT: Duration = Duration::from_secs(3 * 60);

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "account_list",
            "List the Apple ID accounts saved in Impactor (shared with the GUI and plumesign CLI): \
             email, first name, stored team id and which one is currently selected. Also returns \
             the configured UI locale. Does not contact Apple.",
            schema(json!({}), &[]),
        ),
        tool(
            "account_login",
            "Log in with an Apple ID and save it as the selected account. If Apple requires \
             two-factor authentication the result has status \"2fa_required\": ask the user for the \
             code (pushed to their trusted devices, or by SMS) and call `account_submit_2fa`. On \
             success the result has status \"logged_in\". Starting a new login cancels any pending \
             one. Use an Apple ID dedicated to sideloading if possible.",
            schema(
                json!({
                    "email": { "type": "string", "description": "Apple ID email address." },
                    "password": { "type": "string", "description": "Apple ID password." }
                }),
                &["email", "password"],
            ),
        ),
        tool(
            "account_submit_2fa",
            "Continue a pending `account_login`. Pass `code` with the 6-digit verification code, \
             or `sms_phone_id` (an id from `trusted_phone_numbers`) to have Apple text a code to that \
             number (then call again with `code`). With neither argument it just waits for the \
             login's next state (use after an \"in_progress\" result). Returns the same shape as \
             `account_login`.",
            schema(
                json!({
                    "code": { "type": "string", "description": "2FA verification code." },
                    "sms_phone_id": { "type": "integer", "description": "Trusted phone number id to send an SMS code to." }
                }),
                &[],
            ),
        ),
        tool(
            "account_cancel_login",
            "Cancel a pending `account_login` (e.g. waiting for a 2FA code).",
            schema(json!({}), &[]),
        ),
        tool(
            "account_switch",
            "Select a different saved account; subsequent tools use it by default.",
            schema(
                json!({ "email": { "type": "string", "description": "Email of a saved account (see `account_list`)." } }),
                &["email"],
            ),
        ),
        tool(
            "account_remove",
            "Log out: remove a saved account (its session token) from Impactor. Does not affect \
             the Apple ID itself or anything on Apple's servers.",
            schema(
                json!({ "email": { "type": "string", "description": "Email of the saved account to remove." } }),
                &["email"],
            ),
        ),
        tool(
            "account_teams",
            "List the developer teams available to an account (name, team_id, type, status) and \
             show which team is stored as the account's default for signing.",
            schema(
                json!({ "email": { "type": "string", "description": "Account email; defaults to the selected account." } }),
                &[],
            ),
        ),
        tool(
            "account_set_team",
            "Set the default developer team used for signing/portal operations for an account. \
             The team id is validated against `account_teams`.",
            schema(
                json!({
                    "team_id": { "type": "string", "description": "Team id from `account_teams`." },
                    "email": { "type": "string", "description": "Account email; defaults to the selected account." }
                }),
                &["team_id"],
            ),
        ),
        tool(
            "settings_set_locale",
            "Set the Impactor GUI language (e.g. \"en\", \"fr\", \"zh-CN\"). Omit or pass an empty \
             string to follow the system locale.",
            schema(
                json!({ "locale": { "type": "string", "description": "Locale code, or empty for system default." } }),
                &[],
            ),
        ),
    ]
}

pub async fn call(name: &str, args: &Value, _ctx: &Ctx) -> Option<ToolResult> {
    let result = match name {
        "account_list" => account_list().await,
        "account_login" => account_login(args).await,
        "account_submit_2fa" => account_submit_2fa(args).await,
        "account_cancel_login" => account_cancel_login(),
        "account_switch" => account_switch(args).await,
        "account_remove" => account_remove(args).await,
        "account_teams" => account_teams(args).await,
        "account_set_team" => account_set_team(args).await,
        "settings_set_locale" => settings_set_locale(args).await,
        _ => return None,
    };
    Some(result)
}

// ---------------------------------------------------------------------------
// Simple store operations
// ---------------------------------------------------------------------------

async fn account_list() -> ToolResult {
    let store = common::load_store().await?;
    let selected = store.selected_account().map(|a| a.email().clone());

    let mut accounts: Vec<_> = store.accounts().values().collect();
    accounts.sort_by(|a, b| a.email().cmp(b.email()));

    let list: Vec<Value> = accounts
        .into_iter()
        .map(|a| {
            json!({
                "email": a.email(),
                "first_name": a.first_name(),
                "team_id": a.team_id(),
                "selected": Some(a.email()) == selected.as_ref(),
            })
        })
        .collect();

    Ok(json!({
        "accounts": list,
        "selected": selected,
        "locale": store.locale().unwrap_or("system"),
    }))
}

async fn account_switch(args: &Value) -> ToolResult {
    let email = req_str(args, "email")?;
    let mut store = common::load_store().await?;
    if store.get_account(&email).is_none() {
        return Err(format!(
            "No saved account for {email}. Use `account_list` to see saved accounts."
        ));
    }
    store
        .account_select(&email)
        .await
        .map_err(|e| format!("Failed to select account: {e}"))?;
    Ok(json!({ "selected": email }))
}

async fn account_remove(args: &Value) -> ToolResult {
    let email = req_str(args, "email")?;
    let mut store = common::load_store().await?;
    if store.get_account(&email).is_none() {
        return Err(format!(
            "No saved account for {email}. Use `account_list` to see saved accounts."
        ));
    }
    store
        .accounts_remove(&email)
        .await
        .map_err(|e| format!("Failed to remove account: {e}"))?;

    let remaining: Vec<String> = store.accounts().keys().cloned().collect();
    let selected = store.selected_account().map(|a| a.email().clone());
    let mut out = json!({
        "removed": email,
        "selected": selected,
        "remaining_accounts": remaining,
    });
    if selected.is_none() && !remaining.is_empty() {
        out["note"] = json!("No account is selected now; use `account_switch` to pick one.");
    }
    Ok(out)
}

async fn account_teams(args: &Value) -> ToolResult {
    let (account, session) = common::open_session(args).await?;
    let teams = session
        .qh_list_teams()
        .await
        .map_err(|e| format!("Failed to list teams: {e}"))?
        .teams;

    let stored = account.team_id().clone();
    let stored_valid = !stored.is_empty() && teams.iter().any(|t| t.team_id == stored);
    let effective = if stored_valid {
        Some(stored.clone())
    } else if stored.is_empty() {
        teams.first().map(|t| t.team_id.clone())
    } else {
        None
    };

    let list: Vec<Value> = teams
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "team_id": t.team_id,
                "type": t._type,
                "status": t.status,
                "is_default": Some(&t.team_id) == effective.as_ref(),
            })
        })
        .collect();

    let mut out = json!({
        "email": account.email(),
        "teams": list,
        "stored_team_id": stored,
        "effective_team_id": effective,
    });
    if !stored.is_empty() && !stored_valid {
        out["warning"] = json!(format!(
            "Stored team '{stored}' is not available to this account anymore; fix it with `account_set_team`."
        ));
    }
    Ok(out)
}

async fn account_set_team(args: &Value) -> ToolResult {
    let team_id = req_str(args, "team_id")?;
    let (account, session) = common::open_session(args).await?;
    let teams = session
        .qh_list_teams()
        .await
        .map_err(|e| format!("Failed to list teams: {e}"))?
        .teams;

    let Some(team) = teams.iter().find(|t| t.team_id == team_id) else {
        let valid: Vec<String> = teams
            .iter()
            .map(|t| format!("{} ({})", t.team_id, t.name))
            .collect();
        return Err(format!(
            "Team '{team_id}' is not available to {}. Valid teams: {}",
            account.email(),
            valid.join(", ")
        ));
    };

    let mut store = common::load_store().await?;
    store
        .update_account_team(account.email(), team_id.clone())
        .await
        .map_err(|e| format!("Failed to save team selection: {e}"))?;

    Ok(json!({
        "email": account.email(),
        "team_id": team_id,
        "team_name": team.name,
    }))
}

async fn settings_set_locale(args: &Value) -> ToolResult {
    let locale = opt_str(args, "locale").filter(|l| !l.eq_ignore_ascii_case("system"));
    let mut store = common::load_store().await?;
    store
        .set_locale_sync(locale.clone())
        .map_err(|e| format!("Failed to save locale: {e}"))?;
    Ok(json!({
        "locale": locale.unwrap_or_else(|| "system".to_string()),
        "note": "Takes effect the next time the Impactor GUI starts."
    }))
}

// ---------------------------------------------------------------------------
// Login with multi-call 2FA
// ---------------------------------------------------------------------------

enum LoginEvent {
    NeedsCode(TwoFactorRequest),
    Done(Result<Value, String>),
}

struct PendingLogin {
    generation: u64,
    email: String,
    action_tx: Sender<TwoFactorAction>,
    event_rx: Arc<Mutex<Receiver<LoginEvent>>>,
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);
static GENERATION: Mutex<u64> = Mutex::new(0);

fn lock_pending() -> std::sync::MutexGuard<'static, Option<PendingLogin>> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clears the pending login only if it is still the one identified by `generation`.
fn clear_pending(generation: u64) {
    let mut guard = lock_pending();
    if guard.as_ref().is_some_and(|p| p.generation == generation) {
        *guard = None;
    }
}

async fn account_login(args: &Value) -> ToolResult {
    let email = req_str(args, "email")?;
    let password = args
        .get("password")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Missing required argument `password`".to_string())?;

    let generation = {
        let mut g = GENERATION.lock().unwrap_or_else(|p| p.into_inner());
        *g += 1;
        *g
    };

    let (event_tx, event_rx) = mpsc::channel::<LoginEvent>();
    let (action_tx, action_rx) = mpsc::channel::<TwoFactorAction>();
    let event_rx = Arc::new(Mutex::new(event_rx));

    {
        // Replacing drops the previous `action_tx`, which makes the previous
        // login's 2FA prompt fail with "cancelled".
        let mut guard = lock_pending();
        if let Some(old) = guard.take() {
            log::info!("Cancelling pending login for {}", old.email);
        }
        *guard = Some(PendingLogin {
            generation,
            email: email.clone(),
            action_tx,
            event_rx: event_rx.clone(),
        });
    }

    let thread_email = email.clone();
    let spawn = std::thread::Builder::new()
        .name("impactor-mcp-login".into())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(run_login(
                    thread_email,
                    password,
                    event_tx.clone(),
                    action_rx,
                )),
                Err(e) => Err(format!("Failed to start login runtime: {e}")),
            };
            let _ = event_tx.send(LoginEvent::Done(result));
        });

    if let Err(e) = spawn {
        clear_pending(generation);
        return Err(format!("Failed to start login thread: {e}"));
    }

    wait_for_event(&event_rx, generation, &email)
}

async fn run_login(
    email: String,
    password: String,
    event_tx: Sender<LoginEvent>,
    action_rx: Receiver<TwoFactorAction>,
) -> Result<Value, String> {
    let creds_email = email.clone();
    let appleid_closure = move || -> Result<(String, String), String> {
        Ok((creds_email.clone(), password.clone()))
    };

    let tfa_closure = move |req: TwoFactorRequest| -> Result<TwoFactorAction, String> {
        event_tx
            .send(LoginEvent::NeedsCode(req))
            .map_err(|_| "Login cancelled".to_string())?;
        match action_rx.recv_timeout(TFA_TIMEOUT) {
            Ok(action) => Ok(action),
            Err(RecvTimeoutError::Timeout) => {
                Err("Timed out waiting for the 2FA code (10 minutes)".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => Err("Login cancelled".to_string()),
        }
    };

    let account = Account::login(appleid_closure, tfa_closure, common::anisette_config())
        .await
        .map_err(|e| format!("Login failed: {e}. Start over with `account_login`."))?;

    let mut store = AccountStore::load(&Some(common::store_path()))
        .await
        .map_err(|e| format!("Logged in, but failed to load account store: {e}"))?;
    store
        .accounts_add_from_session(email.clone(), account)
        .await
        .map_err(|e| format!("Logged in, but failed to save the account: {e}"))?;

    let saved = store.get_account(&email);
    Ok(json!({
        "status": "logged_in",
        "email": email,
        "first_name": saved.map(|a| a.first_name().clone()),
        "team_id": saved.map(|a| a.team_id().clone()),
        "selected": true,
    }))
}

/// Blocks (this call's runtime has nothing else to do) until the login thread
/// reports its next state.
fn wait_for_event(
    event_rx: &Arc<Mutex<Receiver<LoginEvent>>>,
    generation: u64,
    email: &str,
) -> ToolResult {
    let rx = event_rx.lock().unwrap_or_else(|p| p.into_inner());
    match rx.recv_timeout(CALL_WAIT) {
        Ok(LoginEvent::NeedsCode(req)) => {
            let phones: Vec<Value> = req
                .trusted_phone_numbers
                .iter()
                .map(|p| json!({ "id": p.id, "last_two_digits": p.last_two_digits }))
                .collect();
            let (method, next) = match req.method {
                TwoFactorMethod::Device if !phones.is_empty() => (
                    "device",
                    "Ask the user for the code shown on their trusted Apple device and call \
                     `account_submit_2fa` with `code`, or call it with `sms_phone_id` to get a \
                     code by SMS instead.",
                ),
                TwoFactorMethod::Device => (
                    "device",
                    "Ask the user for the code shown on their trusted Apple device and call \
                     `account_submit_2fa` with `code`.",
                ),
                TwoFactorMethod::Sms => (
                    "sms",
                    "Ask the user for the code sent by SMS and call `account_submit_2fa` with `code`.",
                ),
            };
            Ok(json!({
                "status": "2fa_required",
                "email": email,
                "method": method,
                "trusted_phone_numbers": phones,
                "next": next,
            }))
        }
        Ok(LoginEvent::Done(result)) => {
            clear_pending(generation);
            result
        }
        Err(RecvTimeoutError::Timeout) => Ok(json!({
            "status": "in_progress",
            "email": email,
            "next": "Login is still running. Call `account_submit_2fa` with no arguments to keep waiting, or `account_cancel_login`.",
        })),
        Err(RecvTimeoutError::Disconnected) => {
            clear_pending(generation);
            Err("Login thread ended unexpectedly. Start over with `account_login`.".to_string())
        }
    }
}

async fn account_submit_2fa(args: &Value) -> ToolResult {
    let code = opt_str(args, "code");
    let sms_phone_id = opt_u64(args, "sms_phone_id");

    let (generation, email, action_tx, event_rx) = {
        let guard = lock_pending();
        let Some(p) = guard.as_ref() else {
            return Err("No login is pending. Start one with `account_login`.".to_string());
        };
        (
            p.generation,
            p.email.clone(),
            p.action_tx.clone(),
            p.event_rx.clone(),
        )
    };

    let action = match (code, sms_phone_id) {
        (Some(_), Some(_)) => {
            return Err("Pass either `code` or `sms_phone_id`, not both.".to_string());
        }
        (Some(code), None) => Some(TwoFactorAction::SubmitCode(code.replace([' ', '-'], ""))),
        (None, Some(id)) => Some(TwoFactorAction::SendSms(
            u32::try_from(id).map_err(|_| format!("Invalid `sms_phone_id`: {id}"))?,
        )),
        (None, None) => None,
    };

    if let Some(action) = action {
        if action_tx.send(action).is_err() {
            // The login thread already finished; its result (if any) is still
            // in the event channel and is returned below.
            log::warn!("Login thread no longer accepts 2FA input");
        }
    }
    drop(action_tx);

    wait_for_event(&event_rx, generation, &email)
}

fn account_cancel_login() -> ToolResult {
    match lock_pending().take() {
        Some(p) => Ok(json!({ "cancelled": true, "email": p.email })),
        None => Ok(json!({ "cancelled": false, "message": "No login was pending." })),
    }
}
