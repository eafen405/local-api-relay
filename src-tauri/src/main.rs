#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;

use anyhow::{bail, Context, Result};
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{CheckMenuItem, CheckMenuItemBuilder, Menu, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

const MENU_AUTOSTART: &str = "autostart";
const MENU_SHOW: &str = "show";
const MENU_QUIT: &str = "quit";
const SIDECAR_NAME: &str = "local-api-relay";
const READY_SUFFIX: &str = "/ready";

#[cfg(windows)]
type JobHandle = win32job::Job;
#[cfg(not(windows))]
type JobHandle = ();

struct AppState {
    sidecar: Mutex<Option<CommandChild>>,
    job: Mutex<Option<JobHandle>>,
    autostart_item: Mutex<Option<CheckMenuItem<Wry>>>,
}

#[derive(Debug)]
struct RelaySnapshot {
    port: u16,
    ready_url: String,
}

#[cfg(windows)]
fn create_kill_on_close_job() -> Result<JobHandle> {
    use win32job::{ExtendedLimitInfo, Job};

    let job = Job::create_with_limit_info(ExtendedLimitInfo::new().limit_kill_on_job_close())
        .context("could not create Windows Job Object")?;
    job.assign_current_process()
        .context("could not assign shell process to Job Object")?;
    Ok(job)
}

#[cfg(not(windows))]
fn create_kill_on_close_job() -> Result<JobHandle> {
    Ok(())
}

fn sidecar_command(app: &tauri::AppHandle) -> Result<tauri_plugin_shell::process::Command> {
    app.shell()
        .sidecar(SIDECAR_NAME)
        .context("sidecar is not configured")
}

fn parse_relay_snapshot(stdout: &[u8]) -> Result<RelaySnapshot> {
    let snapshot: serde_json::Value = serde_json::from_slice(stdout)
        .context("`local-api-relay check --json` returned invalid JSON")?;
    let port = snapshot
        .get("port")
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
        .context("`local-api-relay check --json` did not contain a valid port")?;
    let ready_url = snapshot
        .get("ready_url")
        .and_then(|value| value.as_str())
        .context("`local-api-relay check --json` did not contain ready_url")?
        .to_owned();
    Ok(RelaySnapshot { port, ready_url })
}

async fn resolve_relay(app: &tauri::AppHandle) -> Result<RelaySnapshot> {
    let output = sidecar_command(app)?
        .args(["check", "--json"])
        .output()
        .await
        .context("could not run `local-api-relay check --json`")?;

    if !output.status.success() {
        bail!(
            "`local-api-relay check --json` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_relay_snapshot(&output.stdout)
}

async fn spawn_sidecar(app: &tauri::AppHandle, relay: &RelaySnapshot) -> Result<CommandChild> {
    let port_arg = relay.port.to_string();
    let (mut events, child) = sidecar_command(app)?
        .args(["serve", "--port", port_arg.as_str()])
        .spawn()
        .context("could not spawn relay sidecar")?;

    let client = reqwest::Client::new();
    for _ in 0..200 {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(CommandEvent::Terminated(_)) => {
                        bail!("relay sidecar exited before becoming ready");
                    }
                    Some(CommandEvent::Error(message)) => {
                        bail!("relay sidecar error: {message}");
                    }
                    _ => {}
                }
            }
            result = client.get(relay.ready_url.as_str()).send() => {
                if let Ok(response) = result {
                    if response.status().is_success() {
                        tauri::async_runtime::spawn(
                            async move { while events.recv().await.is_some() {} },
                        );
                        return Ok(child);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = child.kill();
    bail!("relay sidecar did not become ready at {}", relay.ready_url);
}

fn kill_sidecar(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Some(child) = state.sidecar.lock().unwrap().take() {
            let _ = child.kill();
        }
    }
}

fn handle_tray_menu_event(app: &tauri::AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        MENU_SHOW => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        MENU_QUIT => {
            kill_sidecar(app);
            app.exit(0);
        }
        MENU_AUTOSTART => {
            let enabled = autostart::is_enabled();
            let new_state = !enabled;
            if let Err(error) = autostart::set_enabled(new_state) {
                eprintln!("could not toggle autostart: {error:#}");
                return;
            }
            if let Some(state) = app.try_state::<AppState>() {
                if let Some(item) = state.autostart_item.lock().unwrap().as_ref() {
                    let _ = item.set_checked(new_state);
                }
            }
        }
        _ => {}
    }
}

fn setup_app(app: &mut tauri::App) -> Result<()> {
    let job = create_kill_on_close_job().context("could not install sidecar cleanup job")?;

    let app_handle = app.handle().clone();
    let relay = tauri::async_runtime::block_on(resolve_relay(&app_handle))
        .context("could not determine relay settings")?;
    let sidecar = tauri::async_runtime::block_on(spawn_sidecar(&app_handle, &relay))
        .context("could not start relay sidecar")?;

    let base_url = relay
        .ready_url
        .strip_suffix(READY_SUFFIX)
        .context("relay ready_url must end with /ready")?;
    let url = format!("{base_url}/")
        .parse::<url::Url>()
        .context("relay URL is invalid")?;
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Local API Relay")
        .inner_size(1280.0, 800.0)
        .build()
        .context("could not create main window")?;

    let autostart_enabled = autostart::is_enabled();
    let autostart_item = CheckMenuItemBuilder::with_id(MENU_AUTOSTART, "开机启动")
        .checked(autostart_enabled)
        .build(app)?;
    let show_item = MenuItemBuilder::with_id(MENU_SHOW, "显示").build(app)?;
    let quit_item = MenuItemBuilder::with_id(MENU_QUIT, "退出").build(app)?;
    let menu = Menu::with_items(app, &[&autostart_item, &show_item, &quit_item])?;

    TrayIconBuilder::new()
        .icon(
            app.default_window_icon()
                .cloned()
                .expect("default window icon"),
        )
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(handle_tray_menu_event)
        .build(app)
        .context("could not create system tray icon")?;

    app.manage(AppState {
        sidecar: Mutex::new(Some(sidecar)),
        job: Mutex::new(Some(job)),
        autostart_item: Mutex::new(Some(autostart_item)),
    });

    Ok(())
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            if let Err(error) = setup_app(app) {
                eprintln!("local-api-relay shell setup failed: {error:#}");
                return Err(Box::new(std::io::Error::other(error.to_string())));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building local-api-relay shell");

    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { .. } => kill_sidecar(app_handle),
        RunEvent::Exit => kill_sidecar(app_handle),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relay_snapshot() {
        let snapshot = parse_relay_snapshot(
            br#"{"version":"0.1.0","port":8787,"ready_url":"http://127.0.0.1:8787/ready"}"#,
        )
        .unwrap();
        assert_eq!(snapshot.port, 8787);
        assert_eq!(snapshot.ready_url, "http://127.0.0.1:8787/ready");
    }

    #[test]
    fn rejects_snapshot_without_ready_url() {
        let error = parse_relay_snapshot(br#"{"port":8787}"#).unwrap_err();
        assert!(error.to_string().contains("ready_url"));
    }

    #[test]
    fn derives_base_url_from_ready_url() {
        let ready = "http://127.0.0.1:8787/ready";
        let base = ready.strip_suffix(READY_SUFFIX).unwrap();
        assert_eq!(format!("{base}/"), "http://127.0.0.1:8787/");
    }
}
