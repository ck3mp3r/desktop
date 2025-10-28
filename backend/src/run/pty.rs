use std::{collections::HashMap, sync::Arc};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use eyre::Result;
use minijinja::Environment;

use crate::{pty::PtyMetadata, runtime::pty_store::PtyStoreHandle, state::AtuinState};
use tauri::{async_runtime::block_on, Emitter, Manager, State};

pub const PTY_OPEN_CHANNEL: &str = "pty_open";
pub const PTY_KILL_CHANNEL: &str = "pty_kill";

async fn update_badge_count(app: &tauri::AppHandle, store: PtyStoreHandle) -> Result<()> {
    let len = store.len().await?;
    let len = if len == 0 { None } else { Some(len as i64) };

    app.webview_windows()
        .values()
        .next()
        .expect("no window found")
        .set_badge_count(len)?;

    Ok(())
}

#[tauri::command]
pub async fn pty_open(
    app: tauri::AppHandle,
    state: State<'_, AtuinState>,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
    runbook: Uuid,
    block: String,
    shell: Option<String>,
) -> Result<uuid::Uuid, String> {
    let id = uuid::Uuid::new_v4();

    let nanoseconds_now = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let metadata = PtyMetadata {
        pid: id,
        runbook,
        block,
        created_at: nanoseconds_now as u64,
    };
    let cwd = cwd.map(|c| shellexpand::tilde(c.as_str()).to_string());

    let pty = match crate::pty::Pty::open(
        24,
        80,
        cwd,
        env.unwrap_or_default(),
        metadata.clone(),
        shell,
    )
    .await
    {
        Ok(pty) => pty,
        Err(e) => {
            return Err(format!("Failed to open terminal: {e}"));
        }
    };

    // Wait for shell to be ready before proceeding
    if let Err(e) = pty.wait_for_shell_ready().await {
        log::warn!("Shell readiness check failed: {}", e);
        // Continue anyway to avoid breaking existing functionality
    }

    // Add a small delay after readiness check to ensure shell is fully ready
    sleep(Duration::from_millis(500)).await;

    let reader = pty.reader.clone();
    let app_inner = app.clone();
    let pty_store = state.pty_store();
    let channel = format!("pty-{id}");

    tauri::async_runtime::spawn_blocking(move || loop {
        let mut buf = [0u8; 512];

        match reader.lock().unwrap().read(&mut buf) {
            // EOF
            Ok(0) => {
                println!("reader loop hit eof");
                match block_on(remove_pty(app_inner.clone(), id, pty_store)) {
                    Ok(_) => {
                        break;
                    }
                    Err(e) => {
                        println!("failed to remove pty: {e}");
                        break;
                    }
                }
            }

            Ok(_n) => {
                // TODO: sort inevitable encoding issues
                let out = String::from_utf8_lossy(&buf).to_string();
                let out = out.trim_matches(char::from(0));

                app_inner.emit(channel.as_str(), out).unwrap();
            }

            Err(e) => {
                println!("failed to read: {e}");
                break;
            }
        }
    });

    let env = Environment::new();
    state
        .template_state
        .write()
        .await
        .insert(runbook, Arc::new(env));

    state
        .pty_store()
        .add_pty(Box::new(pty))
        .await
        .map_err(|e| e.to_string())?;
    update_badge_count(&app, state.pty_store())
        .await
        .map_err(|e| e.to_string())?;

    app.emit(PTY_OPEN_CHANNEL, metadata)
        .map_err(|e| e.to_string())?;

    Ok(id)
}

#[tauri::command]
pub(crate) async fn pty_write(
    pid: uuid::Uuid,
    data: String,
    state: tauri::State<'_, AtuinState>,
) -> Result<(), String> {
    let bytes = data.as_bytes().to_vec();
    state
        .pty_store()
        .write_pty(pid, bytes.into())
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub(crate) async fn pty_resize(
    pid: uuid::Uuid,
    rows: u16,
    cols: u16,
    state: tauri::State<'_, AtuinState>,
) -> Result<(), String> {
    state
        .pty_store()
        .resize_pty(pid, rows, cols)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

pub(crate) async fn remove_pty(
    app: tauri::AppHandle,
    pid: uuid::Uuid,
    store: PtyStoreHandle,
) -> Result<(), String> {
    let pty_meta = store.get_pty_meta(pid).await.map_err(|e| e.to_string())?;
    store.remove_pty(pid).await.map_err(|e| e.to_string())?;

    if let Some(pty_meta) = pty_meta {
        app.emit(PTY_KILL_CHANNEL, pty_meta)
            .map_err(|e| e.to_string())?;
    }

    update_badge_count(&app, store.clone())
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub(crate) async fn pty_kill(
    app: tauri::AppHandle,
    pid: uuid::Uuid,
    state: tauri::State<'_, AtuinState>,
) -> Result<(), String> {
    remove_pty(app, pid, state.pty_store()).await
}

#[tauri::command]
pub(crate) async fn pty_list(
    state: tauri::State<'_, AtuinState>,
) -> Result<Vec<PtyMetadata>, String> {
    let ptys = state
        .pty_store()
        .list_pty_meta()
        .await
        .map_err(|e| e.to_string())?;

    Ok(ptys)
}
