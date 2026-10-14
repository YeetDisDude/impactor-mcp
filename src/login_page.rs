//! Local sign-in page for `account_login`.
//!
//! Serves a small page on 127.0.0.1 where the user types their Apple ID,
//! password and 2FA code, so the password never passes through the MCP client
//! (which, for an AI agent, would put it in the conversation). The page lives
//! under a random path and only answers requests addressed to the loopback
//! host, so other websites cannot drive it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plume_core::auth::TwoFactorAction;
use serde_json::{Value, json};

use crate::tools::accounts::{self, LoginShared, Phase};

const PAGE: &str = include_str!("login_page.html");

/// The page stops being served after this long, whatever the login state.
const LIFETIME: Duration = Duration::from_secs(30 * 60);
/// Keeps serving the final state for a while so the page can show it.
const GRACE: Duration = Duration::from_secs(60);
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024;

/// Starts serving the sign-in page for login `generation` and returns its URL.
pub fn start(generation: u64, shared: Arc<LoginShared>) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("http://127.0.0.1:{port}/{token}/");

    std::thread::Builder::new()
        .name("impactor-mcp-login-page".into())
        .spawn(move || serve(listener, port, &token, generation, &shared))
        .map_err(|e| e.to_string())?;

    Ok(url)
}

/// Opens `url` in the default browser. Child output is discarded: stdout
/// carries the MCP protocol.
pub fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    let child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(e) => {
            log::warn!("Failed to open the browser: {e}");
            false
        }
    }
}

fn serve(listener: TcpListener, port: u16, token: &str, generation: u64, shared: &LoginShared) {
    let started = Instant::now();
    let mut finished_at: Option<Instant> = None;

    while started.elapsed() < LIFETIME {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle(stream, port, token, generation, shared) {
                    log::debug!("Sign-in page request failed: {e}");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::warn!("Sign-in page accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        let finished = matches!(shared.snapshot().phase, Phase::Done(_))
            || !accounts::page_is_current(generation);
        if finished && finished_at.get_or_insert_with(Instant::now).elapsed() > GRACE {
            break;
        }
    }
    log::info!("Sign-in page closed");
}

struct Request {
    method: String,
    path: String,
    host: String,
    content_type: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut header_bytes = 0;
    let mut line = String::new();

    let mut read_line = |line: &mut String| -> Result<(), String> {
        line.clear();
        let n = reader.read_line(line).map_err(|e| e.to_string())?;
        header_bytes += n;
        if n == 0 || header_bytes > MAX_HEADER_BYTES {
            return Err("bad request".to_string());
        }
        Ok(())
    };

    read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let (mut host, mut content_type, mut content_length) = (String::new(), String::new(), 0usize);
    loop {
        read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "host" => host = value.to_string(),
                "content-type" => content_type = value.to_ascii_lowercase(),
                "content-length" => content_length = value.parse().unwrap_or(usize::MAX),
                _ => {}
            }
        }
    }
    drop(read_line);

    if content_length > MAX_BODY_BYTES {
        return Err("body too large".to_string());
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;

    Ok(Request { method, path, host, content_type, body })
}

fn handle(
    mut stream: TcpStream,
    port: u16,
    token: &str,
    generation: u64,
    shared: &LoginShared,
) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;

    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => return respond(&mut stream, 400, "text/plain", e.as_bytes()),
    };

    // Rejects DNS-rebinding requests that reach the port under another name.
    if request.host != format!("127.0.0.1:{port}") && request.host != format!("localhost:{port}") {
        return respond(&mut stream, 403, "text/plain", b"forbidden");
    }

    let base = format!("/{token}");
    let Some(route) = request.path.strip_prefix(&base) else {
        return respond(&mut stream, 404, "text/plain", b"not found");
    };

    match (request.method.as_str(), route) {
        ("GET", "") => {
            let location = format!("{base}/");
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes()).map_err(|e| e.to_string())
        }
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/status") => {
            let status = accounts::page_status(generation, shared);
            respond_json(&mut stream, 200, &status)
        }
        ("POST", "/credentials" | "/code") => {
            // A JSON content type cannot be sent cross-site without a CORS
            // preflight, which this server never approves.
            if !request.content_type.starts_with("application/json") {
                return respond_json(&mut stream, 415, &json!({ "error": "expected JSON" }));
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let result = if route == "/credentials" {
                submit_credentials(generation, &body)
            } else {
                submit_code(generation, &body)
            };
            match result {
                Ok(()) => respond_json(&mut stream, 200, &json!({ "ok": true })),
                Err(e) => respond_json(&mut stream, 409, &json!({ "error": e })),
            }
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn submit_credentials(generation: u64, body: &Value) -> Result<(), String> {
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let email = field("email").ok_or("Enter your Apple ID email.")?;
    let password = field("password").ok_or("Enter your password.")?;
    accounts::page_submit_credentials(generation, email.trim().to_string(), password)
}

fn submit_code(generation: u64, body: &Value) -> Result<(), String> {
    let action = if let Some(id) = body.get("sms_phone_id").and_then(Value::as_u64) {
        TwoFactorAction::SendSms(u32::try_from(id).map_err(|_| "Invalid phone id.")?)
    } else {
        let code = body
            .get("code")
            .and_then(Value::as_str)
            .map(|c| c.replace([' ', '-'], ""))
            .filter(|c| !c.is_empty())
            .ok_or("Enter the verification code.")?;
        TwoFactorAction::SubmitCode(code)
    };
    accounts::page_submit_action(generation, action)
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<(), String> {
    respond(stream, status, "application/json", body.to_string().as_bytes())
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Frame-Options: DENY\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
         style-src 'unsafe-inline'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}
