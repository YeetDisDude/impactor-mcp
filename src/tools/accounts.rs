//! Apple ID account management: login (with multi-call 2FA), switching,
//! removal, team selection and app settings.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use plume_core::auth::{Account, TwoFactorAction, TwoFactorMethod, TwoFactorRequest};
use plume_store::AccountStore;
use serde_json::{Value, json};

use crate::common::{self, Ctx, ToolResult, bool_or, opt_str, opt_u64, req_str, schema, tool};
use crate::login_page;

/// How long the login thread waits for the user to supply a 2FA code.
const TFA_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a single MCP call waits for the next login event before returning
/// `in_progress`.
const CALL_WAIT: Duration = Duration::from_secs(3 * 60);
/// How long the sign-in page waits for the user to submit their Apple ID.
const CREDENTIALS_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Password attempts allowed on one sign-in page. Kept low because repeated
/// logins make Apple reject even the correct password for a while.
const PAGE_ATTEMPTS: u32 = 3;

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
            "Log in with an Apple ID and save it as the selected account. Call it WITHOUT a \
             password (the normal way): it opens a sign-in page on this computer in the user's \
             browser, where they type their Apple ID, password and 2FA code themselves, so the \
             password never passes through the agent. The result has status \"awaiting_user\" and \
             a `sign_in_url`; then call `account_submit_2fa` with no arguments to wait until it \
             finishes. Only when the user explicitly wants to and the client allows it, pass \
             `email` and `password` directly; then a \"2fa_required\" result means asking for the \
             code and calling `account_submit_2fa` with it. On success the status is \
             \"logged_in\". Starting a new login cancels any pending one. Use an Apple ID \
             dedicated to sideloading if possible.",
            schema(
                json!({
                    "email": { "type": "string", "description": "Apple ID email address. Optional with the sign-in page (pre-fills it)." },
                    "password": { "type": "string", "description": "Apple ID password. Omit to use the sign-in page." },
                    "open_browser": { "type": "boolean", "description": "Open the sign-in page in the default browser (default true)." }
                }),
                &[],
            ),
        ),
        tool(
            "account_submit_2fa",
            "Continue a pending `account_login`. Pass `code` with the 6-digit verification code, \
             or `sms_phone_id` (an id from `trusted_phone_numbers`) to have Apple text a code to that \
             number (then call again with `code`). With neither argument it just waits for the \
             login's next state (use after an \"awaiting_user\" or \"in_progress\" result, or \
             while the user finishes on the sign-in page). Returns the same shape as \
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
//
// A login runs on its own thread and publishes its progress in a
// `LoginShared`. The MCP tools and the local sign-in page (`login_page.rs`)
// both read that state, so either one can supply the 2FA code. With the page,
// the Apple ID password is typed into the user's browser and never passes
// through the MCP client.

#[derive(Clone)]
pub(crate) enum Phase {
    /// Waiting for the user to submit email and password on the sign-in page.
    AwaitingCredentials { error: Option<String> },
    /// Talking to Apple.
    SigningIn,
    NeedsCode(TwoFactorRequest),
    Done(Result<Value, String>),
}

impl Phase {
    /// Phases that wait for someone or are final, as opposed to Apple round trips.
    fn is_settled(&self) -> bool {
        !matches!(self, Phase::SigningIn)
    }
}

#[derive(Clone)]
pub(crate) struct LoginSnapshot {
    pub version: u64,
    pub phase: Phase,
    pub email: Option<String>,
}

pub(crate) struct LoginShared {
    state: Mutex<LoginSnapshot>,
    changed: Condvar,
}

impl LoginShared {
    fn new(phase: Phase, email: Option<String>) -> Self {
        Self {
            state: Mutex::new(LoginSnapshot { version: 0, phase, email }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LoginSnapshot> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set(&self, phase: Phase) {
        let mut state = self.lock();
        state.version += 1;
        state.phase = phase;
        self.changed.notify_all();
    }

    fn set_email(&self, email: &str) {
        self.lock().email = Some(email.to_string());
    }

    pub(crate) fn snapshot(&self) -> LoginSnapshot {
        self.lock().clone()
    }

    /// Blocks until there is a settled phase newer than `after`, or `timeout`.
    fn wait_settled(&self, after: u64, timeout: Duration) -> LoginSnapshot {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while !(state.version > after && state.phase.is_settled()) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        state.clone()
    }
}

struct PendingLogin {
    generation: u64,
    shared: Arc<LoginShared>,
    action_tx: Sender<TwoFactorAction>,
    /// Set when the credentials come from the sign-in page.
    creds_tx: Option<Sender<(String, String)>>,
    page_url: Option<String>,
    /// Last state version returned to the MCP client.
    reported: u64,
}

enum Credentials {
    Given { email: String, password: String },
    FromPage(Receiver<(String, String)>),
}

static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);
static GENERATION: Mutex<u64> = Mutex::new(0);

fn lock_pending() -> MutexGuard<'static, Option<PendingLogin>> {
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
    let email = opt_str(args, "email");
    let password = args
        .get("password")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let generation = {
        let mut g = GENERATION.lock().unwrap_or_else(|p| p.into_inner());
        *g += 1;
        *g
    };

    let (action_tx, action_rx) = mpsc::channel::<TwoFactorAction>();
    let (credentials, creds_tx, shared) = match password {
        Some(password) => {
            let email = email.ok_or("Missing required argument `email`")?;
            let shared = LoginShared::new(Phase::SigningIn, Some(email.clone()));
            (Credentials::Given { email, password }, None, shared)
        }
        None => {
            let (tx, rx) = mpsc::channel();
            let shared = LoginShared::new(Phase::AwaitingCredentials { error: None }, email);
            (Credentials::FromPage(rx), Some(tx), shared)
        }
    };
    let shared = Arc::new(shared);
    let use_page = creds_tx.is_some();

    {
        // Replacing drops the previous login's senders, which makes its
        // credential or 2FA wait fail with "cancelled".
        let mut guard = lock_pending();
        if let Some(old) = guard.take() {
            log::info!("Cancelling pending login (generation {})", old.generation);
        }
        *guard = Some(PendingLogin {
            generation,
            shared: shared.clone(),
            action_tx,
            creds_tx,
            page_url: None,
            reported: 0,
        });
    }

    if let Err(e) = spawn_login(shared.clone(), credentials, action_rx) {
        clear_pending(generation);
        return Err(e);
    }

    if !use_page {
        return wait_and_report(generation, &shared, 0);
    }

    let url = match login_page::start(generation, shared.clone()) {
        Ok(url) => url,
        Err(e) => {
            lock_pending().take();
            return Err(format!("Failed to start the local sign-in page: {e}"));
        }
    };
    if let Some(p) = lock_pending().as_mut().filter(|p| p.generation == generation) {
        p.page_url = Some(url.clone());
    }
    let browser_opened = bool_or(args, "open_browser", true) && login_page::open_browser(&url);

    Ok(json!({
        "status": "awaiting_user",
        "sign_in_url": url,
        "browser_opened": browser_opened,
        "next": "A sign-in page on this computer is waiting for the user (opened in their browser \
                 if browser_opened is true; otherwise give them sign_in_url). They enter their \
                 Apple ID, password and 2FA code there; do not ask for the password. Then call \
                 `account_submit_2fa` with no arguments to wait for the result.",
    }))
}

fn spawn_login(
    shared: Arc<LoginShared>,
    credentials: Credentials,
    action_rx: Receiver<TwoFactorAction>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("impactor-mcp-login".into())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(run_login(&shared, credentials, action_rx)),
                Err(e) => Err(format!("Failed to start login runtime: {e}")),
            };
            shared.set(Phase::Done(result));
        })
        .map(|_| ())
        .map_err(|e| format!("Failed to start login thread: {e}"))
}

async fn run_login(
    shared: &LoginShared,
    credentials: Credentials,
    action_rx: Receiver<TwoFactorAction>,
) -> Result<Value, String> {
    let tfa_closure = |req: TwoFactorRequest| -> Result<TwoFactorAction, String> {
        shared.set(Phase::NeedsCode(req));
        let action = match action_rx.recv_timeout(TFA_TIMEOUT) {
            Ok(action) => action,
            Err(RecvTimeoutError::Timeout) => {
                return Err("Timed out waiting for the 2FA code (10 minutes)".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => return Err("Login cancelled".to_string()),
        };
        shared.set(Phase::SigningIn);
        Ok(action)
    };

    let mut last_error = None;
    for attempt in 1..=PAGE_ATTEMPTS {
        let (email, password) = match &credentials {
            Credentials::Given { email, password } => (email.clone(), password.clone()),
            Credentials::FromPage(rx) => {
                if attempt > 1 {
                    shared.set(Phase::AwaitingCredentials { error: last_error.take() });
                }
                match rx.recv_timeout(CREDENTIALS_TIMEOUT) {
                    Ok(creds) => creds,
                    Err(RecvTimeoutError::Timeout) => {
                        return Err("Timed out waiting for the user to sign in on the page \
                                    (15 minutes). Start over with `account_login`."
                            .to_string());
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("Login cancelled".to_string());
                    }
                }
            }
        };
        shared.set_email(&email);
        shared.set(Phase::SigningIn);

        let appleid_closure = || -> Result<(String, String), String> {
            Ok((email.clone(), password.clone()))
        };
        match Account::login(appleid_closure, &tfa_closure, common::anisette_config()).await {
            Ok(account) => return save_account(email, account).await,
            Err(e) => {
                let message = format!("Login failed: {e}");
                if matches!(credentials, Credentials::Given { .. }) || attempt == PAGE_ATTEMPTS {
                    return Err(format!("{message}. Start over with `account_login`."));
                }
                last_error = Some(message);
            }
        }
    }
    Err("Login failed. Start over with `account_login`.".to_string())
}

async fn save_account(email: String, account: Account) -> Result<Value, String> {
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

/// Waits for the login's next settled state after version `after` and turns it
/// into the tool result.
fn wait_and_report(generation: u64, shared: &LoginShared, after: u64) -> ToolResult {
    let snapshot = shared.wait_settled(after, CALL_WAIT);
    let page_url = {
        let mut guard = lock_pending();
        match guard.as_mut().filter(|p| p.generation == generation) {
            Some(p) => {
                p.reported = p.reported.max(snapshot.version);
                p.page_url.clone()
            }
            None => None,
        }
    };
    let email = snapshot.email.clone().unwrap_or_default();

    match snapshot.phase {
        Phase::Done(result) => {
            clear_pending(generation);
            result
        }
        Phase::NeedsCode(req) => {
            let phones = phones_json(&req);
            let (method, mut next) = match req.method {
                TwoFactorMethod::Device if !phones.is_empty() => (
                    "device",
                    "Ask the user for the code shown on their trusted Apple device and call \
                     `account_submit_2fa` with `code`, or call it with `sms_phone_id` to get a \
                     code by SMS instead."
                        .to_string(),
                ),
                TwoFactorMethod::Device => (
                    "device",
                    "Ask the user for the code shown on their trusted Apple device and call \
                     `account_submit_2fa` with `code`."
                        .to_string(),
                ),
                TwoFactorMethod::Sms => (
                    "sms",
                    "Ask the user for the code sent by SMS and call `account_submit_2fa` with `code`."
                        .to_string(),
                ),
            };
            if page_url.is_some() {
                next = "The user can type the 2FA code on the sign-in page; call \
                        `account_submit_2fa` with no arguments to wait for the result. \
                        Alternatively: "
                    .to_string()
                    + &next;
            }
            Ok(json!({
                "status": "2fa_required",
                "email": email,
                "method": method,
                "trusted_phone_numbers": phones,
                "sign_in_url": page_url,
                "next": next,
            }))
        }
        Phase::AwaitingCredentials { error } => Ok(json!({
            "status": "awaiting_user",
            "sign_in_url": page_url,
            "last_error": error,
            "next": "Still waiting for the user to sign in on the page. Call `account_submit_2fa` \
                     with no arguments to keep waiting, or `account_cancel_login`.",
        })),
        Phase::SigningIn => Ok(json!({
            "status": "in_progress",
            "email": email,
            "next": "Login is still running. Call `account_submit_2fa` with no arguments to keep \
                     waiting, or `account_cancel_login`.",
        })),
    }
}

fn phones_json(req: &TwoFactorRequest) -> Vec<Value> {
    req.trusted_phone_numbers
        .iter()
        .map(|p| json!({ "id": p.id, "last_two_digits": p.last_two_digits }))
        .collect()
}

async fn account_submit_2fa(args: &Value) -> ToolResult {
    let code = opt_str(args, "code");
    let sms_phone_id = opt_u64(args, "sms_phone_id");

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

    let (generation, shared, after) = {
        let guard = lock_pending();
        let Some(p) = guard.as_ref() else {
            return Err("No login is pending. Start one with `account_login`.".to_string());
        };
        let after = match action {
            Some(action) => {
                let current = p.shared.snapshot().version;
                if p.action_tx.send(action).is_err() {
                    // The login thread already finished; its result is
                    // returned below.
                    log::warn!("Login thread no longer accepts 2FA input");
                }
                current
            }
            None => p.reported,
        };
        (p.generation, p.shared.clone(), after)
    };

    wait_and_report(generation, &shared, after)
}

fn account_cancel_login() -> ToolResult {
    match lock_pending().take() {
        Some(p) => Ok(json!({ "cancelled": true, "email": p.shared.snapshot().email })),
        None => Ok(json!({ "cancelled": false, "message": "No login was pending." })),
    }
}

// ---------------------------------------------------------------------------
// Sign-in page hooks (called from `login_page.rs`)
// ---------------------------------------------------------------------------

const NOT_CURRENT: &str = "This sign-in was cancelled or replaced. Ask your agent to start a new one.";

/// Whether `generation` is still the pending login.
pub(crate) fn page_is_current(generation: u64) -> bool {
    lock_pending().as_ref().is_some_and(|p| p.generation == generation)
}

pub(crate) fn page_submit_credentials(
    generation: u64,
    email: String,
    password: String,
) -> Result<(), String> {
    let guard = lock_pending();
    let p = guard
        .as_ref()
        .filter(|p| p.generation == generation)
        .ok_or(NOT_CURRENT)?;
    if !matches!(p.shared.snapshot().phase, Phase::AwaitingCredentials { .. }) {
        return Err("The sign-in is not waiting for a password right now.".to_string());
    }
    p.creds_tx
        .as_ref()
        .ok_or(NOT_CURRENT)?
        .send((email, password))
        .map_err(|_| NOT_CURRENT.to_string())
}

pub(crate) fn page_submit_action(generation: u64, action: TwoFactorAction) -> Result<(), String> {
    let guard = lock_pending();
    let p = guard
        .as_ref()
        .filter(|p| p.generation == generation)
        .ok_or(NOT_CURRENT)?;
    if !matches!(p.shared.snapshot().phase, Phase::NeedsCode(_)) {
        return Err("The sign-in is not waiting for a 2FA code right now.".to_string());
    }
    p.action_tx
        .send(action)
        .map_err(|_| NOT_CURRENT.to_string())
}

/// State shown by the sign-in page. Never includes the password.
pub(crate) fn page_status(generation: u64, shared: &LoginShared) -> Value {
    let snapshot = shared.snapshot();
    let base = json!({ "version": snapshot.version, "email": snapshot.email });
    let extra = match snapshot.phase {
        Phase::Done(Ok(v)) => json!({ "phase": "done", "first_name": v.get("first_name") }),
        Phase::Done(Err(e)) => json!({ "phase": "failed", "error": e }),
        _ if !page_is_current(generation) => json!({ "phase": "failed", "error": NOT_CURRENT }),
        Phase::AwaitingCredentials { error } => json!({ "phase": "credentials", "error": error }),
        Phase::SigningIn => json!({ "phase": "signing_in" }),
        Phase::NeedsCode(req) => json!({
            "phase": "code",
            "method": if req.method == TwoFactorMethod::Sms { "sms" } else { "device" },
            "phones": phones_json(&req),
        }),
    };
    let mut out = base;
    if let (Some(o), Value::Object(e)) = (out.as_object_mut(), extra) {
        o.extend(e);
    }
    out
}
