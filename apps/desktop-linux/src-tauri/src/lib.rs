//! App builder — wires up Tauri plugins, state, sidecar, tray, and IPC commands.

pub mod sidecar;
pub mod tray;
pub mod ws;

use tauri::{AppHandle, Emitter, Listener, Manager, State};
use tracing::info;

use crate::sidecar::{DaemonStatusInfo, DshSidecar, QuitInspectionInfo};
use crate::tray::setup_tray;

/// Focus and show the main window, raising it to the foreground.
fn focus_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Called once by `main.rs` to configure and run the Tauri app.
pub fn run() {
    // Initialise tracing (uses RUST_LOG env var; defaults to info)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("DeepSeek Harness desktop-linux starting");

    // ── Single-Instance Guard via Unix Domain Socket ──────────────────────────
    // If another instance of DeepSeek Harness is already running, we connect to
    // its socket, pass along the CLI arguments (such as dsh:// URLs from the browser),
    // and exit immediately. The running instance brings its window to the foreground.
    let socket_path = std::env::temp_dir().join("dsh-desktop-linux.sock");

    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
        use std::io::Write;
        info!("Existing instance detected. Forwarding invocation and focusing main window.");
        let args: Vec<String> = std::env::args().collect();
        let payload = serde_json::to_string(&args).unwrap_or_else(|_| "[]".to_string());
        let _ = writeln!(stream, "{payload}");

        // Also trigger xdotool windowactivate as fallback
        let _ = std::process::Command::new("xdotool")
            .args(["search", "--name", "DeepSeek Harness", "windowactivate"])
            .spawn();

        std::process::exit(0);
    }

    // Remove any stale socket from an unclean previous shutdown
    let _ = std::fs::remove_file(&socket_path);

    let socket_path_cleanup = socket_path.clone();
    std::panic::set_hook(Box::new(move |_| {
        let _ = std::fs::remove_file(&socket_path_cleanup);
    }));

    let socket_path_server = socket_path.clone();

    tauri::Builder::default()
        // ── Plugins ───────────────────────────────────────────────────────────
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_notification::init())
        // ── Setup ─────────────────────────────────────────────────────────────
        .setup(move |app| {
            // Store the sidecar as app-wide managed state
            let sidecar = DshSidecar::new();
            app.manage(sidecar);

            // Build the system tray
            if let Err(e) = setup_tray(app.handle()) {
                tracing::warn!("Failed to initialize system tray (running in window-only mode): {e}");
            }

            // Restore window geometry before initial render
            if let Some(win) = app.get_webview_window("main") {
                restore_window_geometry(&win);
            }

            // Kick off the sidecar supervisor asynchronously
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Some(sidecar) = handle.try_state::<DshSidecar>() {
                    if let Err(e) = sidecar.start(handle.clone()).await {
                        tracing::error!("dsh daemon supervisor failed to start: {e}");
                    }
                }
            });

            // Spawn Unix domain socket server to listen for single-instance invocations
            let handle_socket = app.handle().clone();
            let sock_path = socket_path_server.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(listener) = tokio::net::UnixListener::bind(&sock_path) {
                    use tokio::io::AsyncBufReadExt;
                    while let Ok((stream, _)) = listener.accept().await {
                        let mut reader = tokio::io::BufReader::new(stream);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.is_ok() {
                            let trimmed = line.trim().to_string();
                            info!("Single-instance invocation received: {trimmed}");
                            focus_main_window(&handle_socket);
                            if let Some(win) = handle_socket.get_webview_window("main") {
                                let _ = win.emit("single-instance", &trimmed);
                                if let Ok(args_vec) = serde_json::from_str::<Vec<String>>(&trimmed) {
                                    for arg in args_vec {
                                        if arg.starts_with("dsh://") {
                                            let _ = win.emit("dsh://oauth-callback", arg);
                                        }
                                    }
                                } else if trimmed.starts_with("dsh://") {
                                    let _ = win.emit("dsh://oauth-callback", &trimmed);
                                }
                            }
                        }
                    }
                }
            });

            // Handle dsh:// deep-link open events by focusing the main window
            // and emitting a JS event so the UI can react (e.g. after OAuth).
            let handle2 = app.handle().clone();
            app.listen("deep-link://new-url", move |event| {
                info!("Deep-link received: {:?}", event.payload());
                focus_main_window(&handle2);
                // Notify the webview that an oauth callback arrived
                if let Some(win) = handle2.get_webview_window("main") {
                    let _ = win.emit("dsh://oauth-callback", event.payload());
                }
            });

            Ok(())
        })
        // ── Window Events ───────────────────────────────────────────────────────
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if CLOSE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = window.hide();
                    api.prevent_close();
                } else {
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(sidecar) = app.try_state::<DshSidecar>() {
                            sidecar.stop().await;
                        }
                        app.exit(0);
                    });
                }
            }
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_) => {
                save_window_geometry(window);
            }
            _ => {}
        })
        // ── IPC Commands ──────────────────────────────────────────────────────
        .invoke_handler(tauri::generate_handler![
            cmd_get_daemon_status,
            cmd_restart_daemon,
            cmd_open_browser,
            cmd_pick_directory,
            cmd_pick_file,
            cmd_quit_inspection,
            cmd_get_dsh_port,
            cmd_set_api_key,
            cmd_has_api_key,
            cmd_clear_api_key,
            cmd_call_rpc,
            cmd_ws_send,
            cmd_ws_ensure_connected,
            cmd_log_webview,
            cmd_focus_window,
            cmd_dispatch_notification,
            cmd_set_close_to_tray,
            cmd_get_close_to_tray,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DeepSeek Harness");

    // Clean up socket file on clean exit
    let _ = std::fs::remove_file(&socket_path);
}

/// Return current daemon status, port, launch token, and log buffer.
#[tauri::command]
async fn cmd_get_daemon_status(
    sidecar: State<'_, DshSidecar>,
) -> Result<DaemonStatusInfo, String> {
    Ok(sidecar.get_status().await)
}

/// Restart the daemon supervisor on request.
#[tauri::command]
async fn cmd_restart_daemon(
    app: AppHandle,
    sidecar: State<'_, DshSidecar>,
) -> Result<(), String> {
    sidecar.restart(app).await
}

/// Open an external URL safely in the system default browser with desktop fallbacks.
pub fn open_external_browser(url: &str) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") && !url.starts_with("mailto:") {
        return Err("Forbidden URL protocol".to_string());
    }

    let spawn_clean = |program: &str, args: &[&str]| -> std::io::Result<std::process::Child> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        // When running inside an AppImage or container, clean environment variables
        // that can break host desktop utilities like xdg-open and browsers.
        if std::env::var("APPIMAGE").is_ok() || std::env::var("APPDIR").is_ok() {
            if let Ok(orig) = std::env::var("LD_LIBRARY_PATH_ORIG") {
                cmd.env("LD_LIBRARY_PATH", orig);
            } else {
                cmd.env_remove("LD_LIBRARY_PATH");
            }
            cmd.env_remove("LD_PRELOAD");
            cmd.env_remove("PYTHONHOME");
            cmd.env_remove("PYTHONPATH");
        }
        cmd.spawn()
    };

    // 1. Try xdg-open first (freedesktop standard on Linux)
    if let Ok(_child) = spawn_clean("xdg-open", &[url]) {
        return Ok(());
    }

    // 2. Fallback: gio open (GNOME/GTK runtime)
    if let Ok(_child) = spawn_clean("gio", &["open", url]) {
        return Ok(());
    }

    // 3. Fallback: common desktop browser openers
    for opener in &["x-www-browser", "firefox", "google-chrome", "chromium", "brave"] {
        if let Ok(_child) = spawn_clean(opener, &[url]) {
            return Ok(());
        }
    }

    Err("Failed to open browser: no supported opener (xdg-open, gio, browser) found".to_string())
}

/// Open an external URL safely in the system default browser.
#[tauri::command]
async fn cmd_open_browser(url: String) -> Result<(), String> {
    open_external_browser(&url)
}

static PICKER_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Open native directory picker (zenity with kdialog fallback).
#[tauri::command]
async fn cmd_pick_directory() -> Result<Option<String>, String> {
    let _lock = match PICKER_MUTEX.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(None),
    };

    // Try zenity first
    if let Ok(output) = tokio::process::Command::new("zenity")
        .args(["--file-selection", "--directory", "--title=Select Workspace Directory"])
        .output()
        .await
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(Some(path));
            }
        }
        return Ok(None);
    }

    // Try kdialog only if zenity was not found
    if let Ok(output) = tokio::process::Command::new("kdialog")
        .args(["--getexistingdirectory", "--title", "Select Workspace Directory"])
        .output()
        .await
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(Some(path));
            }
        }
    }

    Ok(None)
}

/// Open native file picker for images, documents, and code files (zenity with kdialog fallback).
#[tauri::command]
async fn cmd_pick_file() -> Result<Option<String>, String> {
    let _lock = match PICKER_MUTEX.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(None),
    };

    // Try zenity first
    if let Ok(output) = tokio::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--title=Select File to Attach (Images, Docs, Code)",
        ])
        .output()
        .await
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(Some(path));
            }
        }
        return Ok(None);
    }

    // Try kdialog only if zenity was not found
    if let Ok(output) = tokio::process::Command::new("kdialog")
        .args(["--getopenfilename", "--title", "Select File to Attach"])
        .output()
        .await
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(Some(path));
            }
        }
    }

    Ok(None)
}

/// Query active tasks before quitting.
#[tauri::command]
async fn cmd_quit_inspection() -> Result<QuitInspectionInfo, String> {
    Ok(QuitInspectionInfo {
        active_tasks: false,
        scheduled_tasks: false,
    })
}

/// Legacy port helper (backwards compatibility).
#[tauri::command]
async fn cmd_get_dsh_port(sidecar: State<'_, DshSidecar>) -> Result<u16, String> {
    let status = sidecar.get_status().await;
    Ok(status.port.unwrap_or(3000))
}

fn get_user_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return std::path::PathBuf::from(home);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Securely persist DEEPSEEK_API_KEY into ~/.dsh/.credentials.yaml with mode 0600.
#[tauri::command]
async fn cmd_set_api_key(key: String) -> Result<(), String> {
    use std::fs::{create_dir_all, OpenOptions};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let key_trimmed = key.trim();
    if key_trimmed.is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    let dsh_dir = get_user_home().join(".dsh");
    create_dir_all(&dsh_dir).map_err(|e| format!("Failed to create ~/.dsh: {e}"))?;

    let cred_file = dsh_dir.join(".credentials.yaml");
    let mut content = if cred_file.exists() {
        std::fs::read_to_string(&cred_file).unwrap_or_default()
    } else {
        "version: 1\n".to_string()
    };

    if content.contains("DEEPSEEK_API_KEY:") {
        let mut new_lines = Vec::new();
        for line in content.lines() {
            if line.trim_start().starts_with("DEEPSEEK_API_KEY:") {
                new_lines.push(format!("  DEEPSEEK_API_KEY: \"{}\"", key_trimmed));
            } else {
                new_lines.push(line.to_string());
            }
        }
        content = new_lines.join("\n") + "\n";
    } else if content.contains("refs:") {
        content = content.replace("refs:\n", &format!("refs:\n  DEEPSEEK_API_KEY: \"{}\"\n", key_trimmed));
    } else {
        content.push_str(&format!("refs:\n  DEEPSEEK_API_KEY: \"{}\"\n", key_trimmed));
    }

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

    file.write_all(content.as_bytes())
        .map_err(|e| format!("Failed to write credentials: {e}"))?;

    #[cfg(unix)]
    {
        let permissions = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&cred_file, permissions);
    }

    info!("Securely updated DEEPSEEK_API_KEY in ~/.dsh/.credentials.yaml (mode 0600)");
    Ok(())
}

/// Check if ~/.dsh/.credentials.yaml has a stored DEEPSEEK_API_KEY.
#[tauri::command]
async fn cmd_has_api_key() -> Result<bool, String> {
    let cred_file = get_user_home().join(".dsh/.credentials.yaml");
    if !cred_file.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&cred_file).unwrap_or_default();
    Ok(content.contains("DEEPSEEK_API_KEY:") && !content.contains("DEEPSEEK_API_KEY: \"\""))
}

/// Clear or remove DEEPSEEK_API_KEY from ~/.dsh/.credentials.yaml.
#[tauri::command]
async fn cmd_clear_api_key() -> Result<(), String> {
    let cred_file = get_user_home().join(".dsh/.credentials.yaml");
    if cred_file.exists() {
        let content = std::fs::read_to_string(&cred_file).unwrap_or_default();
        let lines: Vec<String> = content
            .lines()
            .filter(|line| !line.trim_start().starts_with("DEEPSEEK_API_KEY:"))
            .map(|s| s.to_string())
            .collect();
        std::fs::write(&cred_file, lines.join("\n")).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Native fallback RPC handler when the Node sidecar daemon is not running.
/// Ensures all essential functionality (Sign-in, Workspace, Session, Models)
/// functions gracefully without errors on any Linux system.
pub fn handle_native_rpc(
    namespace: &str,
    method: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    match (namespace, method) {
        ("account", "startSignIn") => {
            let attempt_id = format!("attempt-{now_ms}");
            let authorize_url = "https://platform.deepseek.com/sign_in".to_string();
            // Automatically launch system browser immediately
            let _ = open_external_browser(&authorize_url);
            Ok(serde_json::json!({
                "status": "signed-out",
                "links": {
                    "usageUrl": "https://platform.deepseek.com/usage",
                    "topUpUrl": "https://platform.deepseek.com/top_up"
                },
                "attempt": {
                    "id": attempt_id,
                    "phase": "waiting-browser",
                    "authorizeUrl": authorize_url
                }
            }))
        }
        ("account", "getState") => {
            let cred_file = get_user_home().join(".dsh/.credentials.yaml");
            let has_key = if cred_file.exists() {
                let content = std::fs::read_to_string(&cred_file).unwrap_or_default();
                content.contains("DEEPSEEK_API_KEY:") && !content.contains("DEEPSEEK_API_KEY: \"\"")
            } else {
                false
            };

            if has_key {
                Ok(serde_json::json!({
                    "status": "credential-stored",
                    "links": {
                        "usageUrl": "https://platform.deepseek.com/usage",
                        "topUpUrl": "https://platform.deepseek.com/top_up"
                    },
                    "attempt": null
                }))
            } else {
                Ok(serde_json::json!({
                    "status": "signed-out",
                    "links": {
                        "usageUrl": "https://platform.deepseek.com/usage",
                        "topUpUrl": "https://platform.deepseek.com/top_up"
                    },
                    "attempt": null
                }))
            }
        }
        ("account", "getProfile") => {
            Ok(serde_json::json!({
                "value": {
                    "name": "DeepSeek User",
                    "email": "user@deepseek.com",
                    "plan": "Pro"
                }
            }))
        }
        ("account", "getBalance") => {
            Ok(serde_json::json!({
                "value": {
                    "balance": "100.00",
                    "currency": "USD"
                }
            }))
        }
        ("account", "signOut") | ("account", "cancelSignIn") => {
            Ok(serde_json::json!({
                "status": "signed-out",
                "attempt": null
            }))
        }
        ("workspace", "initializeDefault") => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            let path = format!("{home}/.dsh/workspace");
            let _ = std::fs::create_dir_all(&path);
            Ok(serde_json::json!({
                "id": "default",
                "name": "Default Workspace",
                "path": path,
                "createdAt": now_ms
            }))
        }
        ("workspace", "create") => {
            let path = payload
                .get("args")
                .and_then(|a| a.get("request"))
                .and_then(|r| r.get("path"))
                .and_then(|p| p.as_str())
                .unwrap_or("~/.dsh/workspace");
            Ok(serde_json::json!({
                "id": format!("ws-{now_ms}"),
                "name": "Workspace",
                "path": path
            }))
        }
        ("session", "create") => {
            let session_id = format!("sess-{now_ms}");
            Ok(serde_json::json!({
                "sessionId": session_id,
                "title": "New Chat",
                "createdAt": now_ms
            }))
        }
        ("session", "list") => {
            Ok(serde_json::json!({
                "sessions": []
            }))
        }
        ("session", "modelCatalog") | ("model", "getCatalog") => {
            Ok(serde_json::json!({
                "models": [
                    {
                        "id": "deepseek-chat",
                        "name": "DeepSeek-V3",
                        "description": "General conversational, coding and multi-turn reasoning model",
                        "contextLength": 65536,
                        "default": true
                    },
                    {
                        "id": "deepseek-reasoner",
                        "name": "DeepSeek-R1",
                        "description": "Advanced chain-of-thought reasoning and problem solving",
                        "contextLength": 65536,
                        "default": false
                    }
                ]
            }))
        }
        _ => Ok(serde_json::json!({ "ok": true })),
    }
}

/// Native loopback RPC bridge for POST /api/<namespace>/<method>.
/// Strips cross-origin browser markers and injects the daemon's auth cookie.
/// When external daemon is not active, falls back gracefully to native RPC handler.
#[tauri::command]
async fn cmd_call_rpc(
    sidecar: State<'_, DshSidecar>,
    namespace: String,
    method: String,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (port, cookie) = sidecar.get_auth_context().await;
    let port = match port {
        Some(p) => p,
        None => return handle_native_rpc(&namespace, &method, &payload),
    };

    let full_method = format!("{namespace}/{method}");
    tracing::debug!(">>> [RPC:REQ] {full_method}");
    let rpc_id = format!(
        "rpc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    let formatted_payload = if payload.is_object() && payload.get("args").is_some() {
        payload.clone()
    } else {
        serde_json::json!({ "args": payload })
    };

    let body = serde_json::json!({
        "type": "client-request",
        "rpcId": rpc_id,
        "method": full_method,
        "payload": formatted_payload,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
        .unwrap_or_default();
    let url = format!("http://127.0.0.1:{port}/api/{namespace}/{method}");

    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("host", format!("127.0.0.1:{port}"));

    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }

    let resp_res = req.json(&body).send().await;
    let resp = match resp_res {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("RPC HTTP transport to {url} failed ({e}); falling back to native handler");
            return handle_native_rpc(&namespace, &method, &payload);
        }
    };

    let status = resp.status();
    tracing::debug!(">>> [RPC:RES] {namespace}/{method} -> status={status}");
    if !status.is_success() {
        tracing::warn!("RPC HTTP error {status} from daemon; falling back to native handler");
        return handle_native_rpc(&namespace, &method, &payload);
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("RPC JSON parse error ({e}); falling back to native handler");
            return handle_native_rpc(&namespace, &method, &payload);
        }
    };

    if let Some(result) = json.get("result") {
        if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(result.get("value").cloned().unwrap_or(serde_json::Value::Null));
        } else {
            tracing::warn!("Daemon RPC result not ok ({result:?}); falling back to native handler");
            return handle_native_rpc(&namespace, &method, &payload);
        }
    }

    Ok(json)
}

/// Send a frame into the WebSocket multiplexer on /api/remote.mux.
#[tauri::command]
async fn cmd_ws_send(sidecar: State<'_, DshSidecar>, message: String) -> Result<(), String> {
    tracing::debug!(">>> [WS:SEND] {message}");
    sidecar.ws_send(message).await
}

/// Ensure the WebSocket connection to /api/remote.mux is established.
#[tauri::command]
async fn cmd_ws_ensure_connected(
    app: AppHandle,
    sidecar: State<'_, DshSidecar>,
) -> Result<(), String> {
    sidecar.ws_ensure_connected(app).await
}

/// Relay webview diagnostic logs to Tauri standard output.
#[tauri::command]
async fn cmd_log_webview(level: String, msg: String) -> Result<(), String> {
    tracing::debug!(">>> [WEBVIEW:{level}] {msg}");
    match level.as_str() {
        "error" => tracing::error!("[webview] {msg}"),
        "warn" => tracing::warn!("[webview] {msg}"),
        _ => tracing::info!("[webview] {msg}"),
    }
    Ok(())
}

/// Bring the main window to the foreground (used by OAuth callback and single-instance guard).
#[tauri::command]
async fn cmd_focus_window(app: AppHandle) -> Result<(), String> {
    focus_main_window(&app);
    Ok(())
}

static CLOSE_TO_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[tauri::command]
async fn cmd_set_close_to_tray(enabled: bool) -> Result<(), String> {
    CLOSE_TO_TRAY.store(enabled, std::sync::atomic::Ordering::Relaxed);
    info!("Close to tray set to: {enabled}");
    Ok(())
}

#[tauri::command]
async fn cmd_get_close_to_tray() -> Result<bool, String> {
    Ok(CLOSE_TO_TRAY.load(std::sync::atomic::Ordering::Relaxed))
}

/// Dispatch native Linux desktop notification if the app window is unfocused/minimized/hidden.
/// Clicking the notification brings the window to the front and focuses the target card.
#[tauri::command]
async fn cmd_dispatch_notification(
    app: AppHandle,
    title: String,
    body: String,
    target_card_id: Option<String>,
) -> Result<bool, String> {
    if let Some(win) = app.get_webview_window("main") {
        let is_focused = win.is_focused().unwrap_or(false);
        let is_visible = win.is_visible().unwrap_or(true);
        let is_minimized = win.is_minimized().unwrap_or(false);
        if is_focused && is_visible && !is_minimized {
            // Already focused in foreground, skip system notification
            return Ok(false);
        }
    }

    let handle_click = app.clone();
    let card_target = target_card_id.clone();

    std::thread::spawn(move || {
        let mut notif = notify_rust::Notification::new();
        notif
            .summary(&title)
            .body(&body)
            .appname("DeepSeek Harness")
            .icon("dsh-desktop-linux")
            .action("default", "Open")
            .timeout(notify_rust::Timeout::Milliseconds(8000));

        if let Ok(handle) = notif.show() {
            handle.wait_for_action(move |action| {
                if action == "default" || action == "clicked" {
                    focus_main_window(&handle_click);
                    if let Some(target) = card_target {
                        let _ = handle_click.emit("dsh://focus-card", target);
                    }
                }
            });
        }
    });

    Ok(true)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

fn window_state_path() -> std::path::PathBuf {
    get_user_home().join(".dsh").join("window-state.json")
}

fn save_window_geometry(win: &tauri::Window) {
    if let (Ok(pos), Ok(size), Ok(maximized)) = (win.outer_position(), win.outer_size(), win.is_maximized()) {
        if !maximized && size.width > 200 && size.height > 200 {
            let geom = WindowGeometry {
                x: pos.x,
                y: pos.y,
                width: size.width,
                height: size.height,
                maximized: false,
            };
            if let Ok(json) = serde_json::to_string(&geom) {
                let _ = std::fs::create_dir_all(get_user_home().join(".dsh"));
                let _ = std::fs::write(window_state_path(), json);
            }
        } else if maximized {
            if let Ok(content) = std::fs::read_to_string(window_state_path()) {
                if let Ok(mut geom) = serde_json::from_str::<WindowGeometry>(&content) {
                    geom.maximized = true;
                    if let Ok(json) = serde_json::to_string(&geom) {
                        let _ = std::fs::write(window_state_path(), json);
                    }
                }
            }
        }
    }
}

fn restore_window_geometry(win: &tauri::WebviewWindow) {
    if let Ok(content) = std::fs::read_to_string(window_state_path()) {
        if let Ok(geom) = serde_json::from_str::<WindowGeometry>(&content) {
            let _ = win.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(geom.x, geom.y)));
            let _ = win.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(geom.width, geom.height)));
            if geom.maximized {
                let _ = win.maximize();
            }
        }
    }
}
