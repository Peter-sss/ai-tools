use std::time::Instant;
use tauri::{AppHandle, Emitter};

use crate::models::cursor::CursorAccount;
use crate::modules::{cursor_account, cursor_instance, cursor_oauth, logger, process};

#[tauri::command]
pub fn list_cursor_accounts() -> Result<Vec<CursorAccount>, String> {
    cursor_account::list_accounts_checked()
}

#[tauri::command]
pub fn delete_cursor_account(account_id: String) -> Result<(), String> {
    cursor_account::remove_account(&account_id)
}

#[tauri::command]
pub fn delete_cursor_accounts(account_ids: Vec<String>) -> Result<(), String> {
    cursor_account::remove_accounts(&account_ids)
}

#[tauri::command]
pub fn import_cursor_from_json(json_content: String) -> Result<Vec<CursorAccount>, String> {
    cursor_account::import_from_json(&json_content)
}

#[tauri::command]
pub fn import_cursor_from_local(app: AppHandle) -> Result<Vec<CursorAccount>, String> {
    match cursor_account::import_from_local()? {
        Some(account) => {
            let _ = crate::modules::tray::update_tray_menu(&app);
            Ok(vec![account])
        }
        None => Err("未找到本地 Cursor 登录信息".to_string()),
    }
}

#[tauri::command]
pub fn export_cursor_accounts(account_ids: Vec<String>) -> Result<String, String> {
    cursor_account::export_accounts(&account_ids)
}

#[tauri::command]
pub fn export_cursor_accounts_text(account_ids: Vec<String>) -> Result<String, String> {
    cursor_account::export_accounts_text(&account_ids)
}

#[tauri::command]
pub async fn refresh_cursor_token(
    app: AppHandle,
    account_id: String,
) -> Result<CursorAccount, String> {
    let started_at = Instant::now();
    logger::log_info(&format!(
        "[Cursor Command] 手动刷新账号开始: account_id={}",
        account_id
    ));

    match cursor_account::refresh_account_async(&account_id).await {
        Ok(account) => {
            if let Err(e) = cursor_account::run_quota_alert_if_needed() {
                logger::log_warn(&format!("[QuotaAlert][Cursor] 预警检查失败: {}", e));
            }
            let _ = crate::modules::tray::update_tray_menu(&app);
            logger::log_info(&format!(
                "[Cursor Command] 刷新完成: account_id={}, email={}, elapsed={}ms",
                account.id,
                account.email,
                started_at.elapsed().as_millis()
            ));
            Ok(account)
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Command] 刷新失败: account_id={}, elapsed={}ms, error={}",
                account_id,
                started_at.elapsed().as_millis(),
                err
            ));
            Err(err)
        }
    }
}

#[tauri::command]
pub async fn refresh_all_cursor_tokens(app: AppHandle) -> Result<i32, String> {
    let started_at = Instant::now();
    logger::log_info("[Cursor Command] 批量刷新开始");

    let results = cursor_account::refresh_all_tokens().await?;
    let success_count = results.iter().filter(|(_, r)| r.is_ok()).count();

    if success_count > 0 {
        if let Err(e) = cursor_account::run_quota_alert_if_needed() {
            logger::log_warn(&format!(
                "[QuotaAlert][Cursor] 全量刷新后预警检查失败: {}",
                e
            ));
        }
    }

    let _ = crate::modules::tray::update_tray_menu(&app);
    logger::log_info(&format!(
        "[Cursor Command] 批量刷新完成: success={}, elapsed={}ms",
        success_count,
        started_at.elapsed().as_millis()
    ));
    Ok(success_count as i32)
}

#[tauri::command]
pub fn add_cursor_account_with_token(
    app: AppHandle,
    access_token: String,
) -> Result<Vec<CursorAccount>, String> {
    let accounts = cursor_account::import_from_token_text(&access_token)?;
    let _ = crate::modules::tray::update_tray_menu(&app);
    Ok(accounts)
}

#[tauri::command]
pub async fn update_cursor_account_tags(
    account_id: String,
    tags: Vec<String>,
) -> Result<CursorAccount, String> {
    cursor_account::update_account_tags(&account_id, tags)
}

#[tauri::command]
pub fn get_cursor_accounts_index_path() -> Result<String, String> {
    cursor_account::accounts_index_path_string()
}

#[tauri::command]
pub fn cursor_oauth_login_start() -> Result<cursor_oauth::CursorOAuthStartResponse, String> {
    logger::log_info("[Cursor Command] OAuth 登录开始");
    cursor_oauth::start_login()
}

#[tauri::command]
pub async fn cursor_oauth_login_complete(
    app: AppHandle,
    login_id: String,
) -> Result<CursorAccount, String> {
    logger::log_info(&format!(
        "[Cursor Command] OAuth 等待完成: login_id={}",
        login_id
    ));
    let payload = cursor_oauth::complete_login(&login_id).await?;
    let mut account = cursor_account::upsert_account(payload)?;

    match cursor_account::refresh_account_async(&account.id).await {
        Ok(refreshed) => account = refreshed,
        Err(e) => {
            logger::log_warn(&format!("[Cursor OAuth] 登录后自动刷新配额失败: {}", e));
        }
    }

    let _ = crate::modules::tray::update_tray_menu(&app);
    logger::log_info(&format!(
        "[Cursor Command] OAuth 登录完成: account_id={}, email={}",
        account.id, account.email
    ));
    Ok(account)
}

#[tauri::command]
pub fn cursor_oauth_login_cancel(login_id: Option<String>) -> Result<(), String> {
    logger::log_info(&format!(
        "[Cursor Command] OAuth 取消: login_id={}",
        login_id.as_deref().unwrap_or("<none>")
    ));
    cursor_oauth::cancel_login(login_id.as_deref())
}

#[tauri::command]
pub async fn open_cursor_account_in_chrome(account_id: String) -> Result<(), String> {
    logger::log_info(&format!(
        "[Cursor Command] Chrome 无痕登录: account_id={}",
        account_id
    ));
    cursor_account::open_account_in_chrome(&account_id).await
}

#[tauri::command]
pub async fn inject_cursor_account(app: AppHandle, account_id: String) -> Result<String, String> {
    let started_at = Instant::now();
    logger::log_info(&format!(
        "[Cursor Switch] 开始切换账号: account_id={}",
        account_id
    ));

    let account = cursor_account::load_account(&account_id)
        .ok_or_else(|| format!("Cursor account not found: {}", account_id))?;

    let default_dir = cursor_instance::get_default_cursor_user_data_dir()?;
    cursor_instance::close_cursor(&[default_dir.to_string_lossy().to_string()], 20)?;

    cursor_account::inject_to_cursor(&account_id)?;
    if let Err(err) = cursor_instance::strip_empty_restored_windows() {
        logger::log_warn(&format!("清理 Cursor 空恢复窗口失败: {}", err));
    }
    crate::modules::provider_current_state::set_current_account_id(
        "cursor",
        Some(account_id.as_str()),
    )?;

    if let Err(err) = cursor_instance::update_default_settings(
        Some(Some(account_id.clone())),
        None,
        Some(false),
    ) {
        logger::log_warn(&format!("更新 Cursor 默认实例绑定账号失败: {}", err));
    }

    let launch_warning = match relaunch_default_cursor_restoring_windows() {
        Ok(()) => None,
        Err(err) => {
            if err.starts_with("APP_PATH_NOT_FOUND:") || err.contains("启动 Cursor 失败") {
                logger::log_warn(&format!("Cursor 默认实例启动失败: {}", err));
                if err.starts_with("APP_PATH_NOT_FOUND:") || err.contains("APP_PATH_NOT_FOUND:") {
                    let _ = app.emit(
                        "app:path_missing",
                        serde_json::json!({ "app": "cursor", "retry": { "kind": "default" } }),
                    );
                }
                Some(err)
            } else {
                return Err(err);
            }
        }
    };

    let _ = crate::modules::tray::update_tray_menu(&app);

    if let Some(err) = launch_warning {
        logger::log_warn(&format!(
            "[Cursor Switch] 切号完成但启动失败: account_id={}, email={}, elapsed={}ms, error={}",
            account.id,
            account.email,
            started_at.elapsed().as_millis(),
            err
        ));
        Ok(format!("切换完成，但 Cursor 启动失败: {}", err))
    } else {
        logger::log_info(&format!(
            "[Cursor Switch] 切号成功: account_id={}, email={}, elapsed={}ms",
            account.id,
            account.email,
            started_at.elapsed().as_millis()
        ));
        Ok(format!("切换完成: {}", account.email))
    }
}

fn relaunch_default_cursor_restoring_windows() -> Result<(), String> {
    cursor_instance::ensure_cursor_launch_path_configured()?;
    let default_settings = cursor_instance::load_default_settings()?;
    let extra_args = process::parse_extra_args(&default_settings.extra_args);
    let pid = cursor_instance::start_cursor_default_with_args_with_new_window(&extra_args, false)?;
    let _ = cursor_instance::update_default_pid(Some(pid))?;
    Ok(())
}
