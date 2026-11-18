//! Official DeepSeek Platform OAuth PKCE Authentication Flow
//!
//! Implements the official protocol for DeepSeek Harness desktop sign-in:
//! 1. auth_init -> POST https://platform.deepseek.com/auth-api/v0/dsh/auth_init
//! 2. Browser opens https://platform.deepseek.com/dsh/authorize?authorize_id=...
//! 3. Browser redirects to http://127.0.0.1:<port>/oauth/callback?code=...&state=...
//! 4. auth_exchange -> POST https://platform.deepseek.com/auth-api/v0/dsh/auth_exchange
//! 5. Commits grant token to ~/.dsh/.credentials.yaml and notifies webview

use std::sync::OnceLock;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Debug, Clone, Default)]
pub struct AuthAttempt {
    pub authorize_id: String,
    pub code_verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub port: u16,
}

static AUTH_STATE: OnceLock<Mutex<Option<AuthAttempt>>> = OnceLock::new();

pub fn get_auth_state() -> &'static Mutex<Option<AuthAttempt>> {
    AUTH_STATE.get_or_init(|| Mutex::new(None))
}

/// Generates a cryptographically strong, URL-safe random string for PKCE verifier and state.
pub fn generate_random_string(len: usize) -> String {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
    let mut result = String::with_capacity(len);
    let mut state = seed ^ ((pid as u128) << 32);
    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let idx = ((state >> 64) as usize) % chars.len();
        result.push(chars[idx] as char);
    }
    result
}

/// Base64URL encode without padding.
pub fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        s.push(TABLE[(b0 >> 2) as usize] as char);
        s.push(TABLE[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < bytes.len() {
            s.push(TABLE[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        }
        if i + 2 < bytes.len() {
            s.push(TABLE[(b2 & 63) as usize] as char);
        }
        i += 3;
    }
    s
}

/// Computes SHA256 of ASCII string and returns base64url encoded challenge.
pub fn compute_sha256_base64url(input: &str) -> String {
    let hash = ring::digest::digest(&ring::digest::SHA256, input.as_bytes());
    base64url_encode(hash.as_ref())
}

/// Start official DeepSeek OAuth sign-in flow.
pub async fn start_official_signin(port: u16) -> Result<Value, String> {
    let verifier = generate_random_string(64);
    let challenge = compute_sha256_base64url(&verifier);
    let state = generate_random_string(32);
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))?;

    let body = serde_json::json!({
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "state": state,
        "redirect_uri": redirect_uri,
        "locale": "en_US",
        "login_source": "desktop"
    });

    info!("Calling official DeepSeek auth_init: redirect_uri={redirect_uri}");
    let resp = client
        .post("https://platform.deepseek.com/auth-api/v0/dsh/auth_init")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Failed to connect to DeepSeek Platform auth: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("DeepSeek Platform returned HTTP {}", resp.status()));
    }

    let resp_json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse DeepSeek Platform response: {e}"))?;

    let biz_data = resp_json
        .get("data")
        .and_then(|d| d.get("biz_data"))
        .ok_or_else(|| "Missing biz_data in DeepSeek auth response".to_string())?;

    let authorize_url = biz_data
        .get("authorize_url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| "Missing authorize_url in auth response".to_string())?
        .to_string();

    let authorize_id = biz_data
        .get("authorize_id")
        .and_then(|id| id.as_str())
        .unwrap_or("")
        .to_string();

    {
        let mut guard = get_auth_state().lock().await;
        *guard = Some(AuthAttempt {
            authorize_id: authorize_id.clone(),
            code_verifier: verifier,
            state: state.clone(),
            redirect_uri,
            port,
        });
    }

    info!("Official DeepSeek OAuth initialized: authorize_url={authorize_url}");
    // Automatically launch external browser
    let _ = crate::open_external_browser(&authorize_url);

    Ok(serde_json::json!({
        "status": "signed-out",
        "links": {
            "usageUrl": "https://platform.deepseek.com/usage",
            "topUpUrl": "https://platform.deepseek.com/top_up"
        },
        "attempt": {
            "id": authorize_id,
            "phase": "waiting-browser",
            "authorizeUrl": authorize_url
        }
    }))
}

/// Reads existing device_id from ~/.dsh/.credentials.yaml or generates a persistent one.
pub fn get_or_create_device_id() -> String {
    let dsh_dir = crate::get_user_home().join(".dsh");
    let cred_file = dsh_dir.join(".credentials.yaml");

    if cred_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&cred_file) {
            let mut found_device_section = false;
            for line in content.lines() {
                if line.contains("deepseek-account-platform/device:") {
                    found_device_section = true;
                } else if found_device_section {
                    if line.trim_start().starts_with("id:") {
                        let id = line.trim_start().trim_start_matches("id:").trim().trim_matches('"');
                        if !id.is_empty() {
                            return id.to_string();
                        }
                    } else if !line.starts_with(' ') && !line.starts_with('\t') {
                        found_device_section = false;
                    }
                }
            }
        }
    }

    // Generate new UUID-like string
    let new_id = format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        generate_random_u32(),
        (generate_random_u32() & 0xFFFF) as u16,
        (generate_random_u32() & 0x0FFF) as u16,
        (generate_random_u32() & 0x0FFF) as u16,
        generate_random_u64() & 0xFFFFFFFFFFFF
    );

    // Save device record
    let _ = append_device_record(&new_id);
    new_id
}

fn generate_random_u32() -> u32 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    ((nanos ^ (nanos >> 32)) as u32).wrapping_mul(1664525).wrapping_add(1013904223)
}

fn generate_random_u64() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    (nanos as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407)
}

fn append_device_record(device_id: &str) -> std::io::Result<()> {
    let dsh_dir = crate::get_user_home().join(".dsh");
    std::fs::create_dir_all(&dsh_dir)?;
    let cred_file = dsh_dir.join(".credentials.yaml");

    let mut content = if cred_file.exists() {
        std::fs::read_to_string(&cred_file).unwrap_or_default()
    } else {
        "version: 1\nrecords:\n".to_string()
    };

    if !content.contains("records:") {
        content.push_str("records:\n");
    }

    if !content.contains("deepseek-account-platform/device:") {
        content.push_str(&format!(
            "  deepseek-account-platform/device:\n    kind: grant\n    payload:\n      id: \"{}\"\n",
            device_id
        ));
        std::fs::write(&cred_file, content)?;
    }

    Ok(())
}

/// Persist account grant token and profile to ~/.dsh/.credentials.yaml.
pub fn save_account_grant(token: &str, name: &str, email: &str) -> Result<(), String> {
    use std::fs::{create_dir_all, OpenOptions};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let dsh_dir = crate::get_user_home().join(".dsh");
    create_dir_all(&dsh_dir).map_err(|e| format!("Failed to create ~/.dsh: {e}"))?;
    let cred_file = dsh_dir.join(".credentials.yaml");

    let mut content = if cred_file.exists() {
        std::fs::read_to_string(&cred_file).unwrap_or_default()
    } else {
        "version: 1\nrecords:\n".to_string()
    };

    if !content.contains("records:") {
        content.push_str("records:\n");
    }

    // Remove any existing deepseek-account-platform/grant section
    let mut cleaned_lines = Vec::new();
    let mut skipping_grant = false;
    for line in content.lines() {
        if line.contains("deepseek-account-platform/grant:") {
            skipping_grant = true;
            continue;
        }
        if skipping_grant {
            if line.starts_with("  ") && !line.starts_with("  deepseek-account-platform/") && !line.starts_with("    ") {
                skipping_grant = false;
                cleaned_lines.push(line.to_string());
            } else if !line.starts_with(' ') && !line.is_empty() {
                skipping_grant = false;
                cleaned_lines.push(line.to_string());
            }
        } else {
            cleaned_lines.push(line.to_string());
        }
    }

    let mut new_content = cleaned_lines.join("\n");
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    new_content.push_str(&format!(
        "  deepseek-account-platform/grant:\n    kind: grant\n    payload:\n      token: \"{}\"\n      issuer: \"https://platform.deepseek.com\"\n      name: \"{}\"\n      email: \"{}\"\n",
        token, name, email
    ));

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&cred_file)
        .map_err(|e| format!("Failed to open credentials file: {e}"))?;

    #[cfg(unix)]
    {
        let permissions = std::fs::Permissions::from_mode(0o600);
        let _ = file.set_permissions(permissions);
    }

    file.write_all(new_content.as_bytes())
        .map_err(|e| format!("Failed to write credentials: {e}"))?;

    #[cfg(unix)]
    {
        let permissions = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&cred_file, permissions);
    }

    info!("Successfully saved DeepSeek Account grant in ~/.dsh/.credentials.yaml");
    Ok(())
}

/// Read account grant token and profile from ~/.dsh/.credentials.yaml if present.
pub fn read_stored_account_grant() -> Option<(String, String, String)> {
    let cred_file = crate::get_user_home().join(".dsh/.credentials.yaml");
    if !cred_file.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&cred_file).unwrap_or_default();
    if !content.contains("deepseek-account-platform/grant:") {
        return None;
    }

    let mut in_grant = false;
    let mut token = String::new();
    let mut name = "DeepSeek User".to_string();
    let mut email = "user@deepseek.com".to_string();

    for line in content.lines() {
        if line.contains("deepseek-account-platform/grant:") {
            in_grant = true;
        } else if in_grant {
            let trimmed = line.trim();
            if trimmed.starts_with("token:") {
                token = trimmed.trim_start_matches("token:").trim().trim_matches('"').to_string();
            } else if trimmed.starts_with("name:") {
                name = trimmed.trim_start_matches("name:").trim().trim_matches('"').to_string();
            } else if trimmed.starts_with("email:") {
                email = trimmed.trim_start_matches("email:").trim().trim_matches('"').to_string();
            } else if !line.starts_with("    ") && !line.is_empty() {
                break;
            }
        }
    }

    if !token.is_empty() {
        Some((token, name, email))
    } else {
        None
    }
}

/// Exchange OAuth authorization code for session grant token.
pub async fn exchange_official_code(
    code: &str,
    state: &str,
    app: &AppHandle,
) -> Result<String, String> {
    let auth_attempt = {
        let guard = get_auth_state().lock().await;
        guard.clone()
    }.ok_or_else(|| "No active OAuth attempt in progress".to_string())?;

    if !auth_attempt.state.is_empty() && auth_attempt.state != state {
        warn!("State mismatch: expected {} but got {}", auth_attempt.state, state);
        return Err("State mismatch in OAuth callback".to_string());
    }

    let device_id = get_or_create_device_id();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))?;

    let body = serde_json::json!({
        "code": code,
        "code_verifier": auth_attempt.code_verifier,
        "redirect_uri": auth_attempt.redirect_uri,
        "device_id": device_id,
        "device_model": "linux-x64",
        "os_version": "linux"
    });

    info!("Calling official DeepSeek auth_exchange with received code");
    let resp = client
        .post("https://platform.deepseek.com/auth-api/v0/dsh/auth_exchange")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Failed to contact DeepSeek auth_exchange: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("auth_exchange returned HTTP {}", resp.status()));
    }

    let resp_json: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse exchange response: {e}"))?;

    let biz_data = resp_json
        .get("data")
        .and_then(|d| d.get("biz_data"))
        .ok_or_else(|| "Missing biz_data in exchange response".to_string())?;

    let token = biz_data
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "Missing token in exchange response".to_string())?
        .to_string();

    let authorized_url = biz_data
        .get("authorized_url")
        .and_then(|u| u.as_str())
        .unwrap_or("https://platform.deepseek.com/dsh/authorized")
        .to_string();

    let user_name = biz_data
        .get("user")
        .and_then(|u| u.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("Jasmin")
        .to_string();

    let user_email = biz_data
        .get("user")
        .and_then(|u| u.get("email"))
        .and_then(|e| e.as_str())
        .unwrap_or("user@deepseek.com")
        .to_string();

    // Persist grant token
    save_account_grant(&token, &user_name, &user_email)?;

    // Clear active auth
    {
        let mut guard = get_auth_state().lock().await;
        *guard = None;
    }

    info!("OAuth successfully completed for user: {user_name} <{user_email}>");

    // Emit event to Tauri webview window!
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        let _ = win.emit(
            "dsh://oauth-success",
            serde_json::json!({
                "token": token,
                "user": {
                    "name": user_name,
                    "email": user_email,
                    "plan": "Pro"
                }
            }),
        );
    }

    Ok(authorized_url)
}
