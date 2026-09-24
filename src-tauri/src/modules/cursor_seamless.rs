//! Cursor 单账号无感切号。
//!
//! 补丁写在 Cursor.app 的 workbench.desktop.main.js 里。运行中的 Cursor 每 5 秒
//! 读取同目录的 cp_token.json，并用 window.store.set 更新内存登录态。
//! 同时把同一套固定字段写入默认 state.vscdb，重启后仍保持该账号。
//! 不改应用多开使用的 inject_to_cursor_at_path。

use base64::Engine as _;
use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::cursor::CursorAccount;
use crate::modules::{config, cursor_account, cursor_instance, logger};

const PATCH_MARKER: &str = "/* __COCKPIT_SEAMLESS_PATCH__ */";
const FORGE_MARKER: &str = "/* __SEAMLESS_PATCHED__ */";
const INJECT_ANCHOR: &str = "this.database.getItems()))";
const NOTIFY_COMPACT: &str = "_showNotification(){";
const NOTIFY_SPACED: &str = "_showNotification() {";
const XOR_KEY: &[u8] = b"CursorPro2024!@#";

const STORE_HOOK: &str = r#"
;await(async function(e){
  if(e.get('releaseNotes/lastVersion')){
    window.store=e;
    console.log('[CT] Store ready');
  }
})(this)
"#;

const POLLER_SCRIPT: &str = r#"
;(function(){
  var _0x=(function(){var a=[99,117,114,115,111,114,65,117,116,104,47];function d(c){return c.map(function(x){return String.fromCharCode(x)}).join('')}return{a:d(a.concat([97,99,99,101,115,115,84,111,107,101,110])),b:d(a.concat([114,101,102,114,101,115,104,84,111,107,101,110])),c:d(a.concat([99,97,99,104,101,100,69,109,97,105,108]))}})();
  window.__ctSwitch=function(_d){
    if(!_d||!_d.accessToken||!window.store)return!1;
    try{
      window.store.set(_0x.a,_d.accessToken,-1);
      window.store.set(_0x.b,_d.refreshToken||_d.accessToken,-1);
      if(_d.email)window.store.set(_0x.c,_d.email,-1);
      console.log('[CT] Switched:',_d.email);
      return!0;
    }catch(_e){console.error('[CT] Switch error:',_e);return!1}
  };
  var _XK=[67,117,114,115,111,114,80,114,111,50,48,50,52,33,64,35].map(function(c){return String.fromCharCode(c)}).join('');
  function _xDec(t){try{var k=_XK,d=atob(t),r='';for(var i=0;i<d.length;i++)r+=String.fromCharCode(d.charCodeAt(i)^k.charCodeAt(i%k.length));return r}catch(e){return''}}
  var _tUrl=location.href.replace(/workbench\.html.*$/,'cp_token.json');
  var _lastTs=0;
  function _poll(){
    if(!window.store){return}
    fetch(_tUrl+'?t='+Date.now()).then(function(r){
      if(!r.ok){return}
      return r.text();
    }).then(function(raw){
      if(!raw)return;
      var data=JSON.parse(raw);
      if(!data||!data.encrypted){return}
      if(data.ts&&data.ts<=_lastTs){return}
      _lastTs=data.ts||0;
      var dec=JSON.parse(_xDec(data.encrypted));
      window.__ctSwitch({accessToken:dec.accessToken,refreshToken:dec.refreshToken,email:data.email});
    }).catch(function(ex){console.log('[CT] poll error:',ex)});
  }
  setTimeout(_poll,3000);
  setInterval(_poll,5000);
  console.log('[CT] File poller started:',_tUrl);
})()
"#;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSeamlessStatus {
    pub installed: bool,
    pub available: bool,
    pub workbench_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSeamlessInstallResult {
    pub installed: bool,
    pub newly_installed: bool,
    pub message: String,
}

pub fn seamless_status() -> Result<CursorSeamlessStatus, String> {
    let path = workbench_js_path()?;
    let source = fs::read_to_string(&path).map_err(|err| explain_io_error(&path, &err))?;
    let installed = patch_installed(&source);
    let available = installed || source.contains(INJECT_ANCHOR);
    Ok(CursorSeamlessStatus {
        installed,
        available,
        workbench_path: path.to_string_lossy().to_string(),
    })
}

pub fn install_seamless() -> Result<CursorSeamlessInstallResult, String> {
    let path = workbench_js_path()?;
    let source = fs::read_to_string(&path).map_err(|err| explain_io_error(&path, &err))?;
    if patch_installed(&source) {
        return Ok(CursorSeamlessInstallResult {
            installed: true,
            newly_installed: false,
            message: "无感换号补丁已安装".to_string(),
        });
    }

    let patched = patch_workbench_source(&source)?;
    let backup = seamless_backup_path(&path);
    if !backup.exists() {
        copy_file_allowing_auth(&path, &backup).map_err(|err| {
            format!("备份 workbench.desktop.main.js 失败: {}", err)
        })?;
    }
    write_file_allowing_auth(&path, &patched)?;
    logger::log_info(&format!(
        "[Cursor Seamless] 补丁已写入: {}",
        path.display()
    ));
    Ok(CursorSeamlessInstallResult {
        installed: true,
        newly_installed: true,
        message: "无感换号补丁已安装。请重启一次 Cursor 使补丁生效。".to_string(),
    })
}

pub fn hot_switch_account(account_id: &str) -> Result<CursorAccount, String> {
    let account = cursor_account::load_account(account_id)
        .ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    let status = seamless_status()?;
    if !status.installed {
        return Err("当前未安装无感换号补丁，请先开启无感切号并重启 Cursor".to_string());
    }
    write_fixed_auth_to_default_db(&account)?;
    write_cp_token(&account)?;
    logger::log_info(&format!(
        "[Cursor Seamless] 热切号已写入: id={}, email={}",
        account.id, account.email
    ));
    Ok(account)
}

pub fn write_fixed_auth_to_default_db(account: &CursorAccount) -> Result<(), String> {
    let db_path = cursor_account::get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Err(format!("Cursor state.vscdb 不存在: {}", db_path.display()));
    }
    let conn = Connection::open(&db_path)
        .map_err(|err| format!("打开 Cursor 本地数据库失败: {}", err))?;
    write_fixed_auth_conn(&conn, account)?;
    if let Err(err) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        logger::log_warn(&format!(
            "[Cursor Seamless] WAL checkpoint 失败: id={}, error={}",
            account.id, err
        ));
    }
    Ok(())
}

fn write_fixed_auth_conn(conn: &Connection, account: &CursorAccount) -> Result<(), String> {
    if account.email.trim().is_empty() || account.access_token.trim().is_empty() {
        return Err("缺少切号所需的邮箱或 access_token".to_string());
    }
    conn.execute_batch("PRAGMA busy_timeout=5000;")
        .map_err(|err| format!("设置数据库超时失败: {}", err))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
    )
    .map_err(|err| format!("初始化 Cursor 数据表失败: {}", err))?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|err| format!("开启事务失败: {}", err))?;
    upsert_item(&tx, "cursorAuth/accessToken", account.access_token.trim())?;
    let refresh = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(account.access_token.trim());
    upsert_item(&tx, "cursorAuth/refreshToken", refresh)?;
    upsert_item(&tx, "cursorAuth/cachedEmail", account.email.trim())?;
    let sign_up = account
        .sign_up_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Auth_0");
    upsert_item(&tx, "cursorAuth/cachedSignUpType", sign_up)?;
    if let Some(value) = non_empty(account.membership_type.as_deref()) {
        upsert_item(&tx, "cursorAuth/stripeMembershipType", value)?;
    }
    if let Some(value) = non_empty(account.subscription_status.as_deref()) {
        upsert_item(&tx, "cursorAuth/stripeSubscriptionStatus", value)?;
    }
    if let Some(user_id) = user_id_from_access_token(&account.access_token) {
        upsert_item(&tx, "cursorAuth/userId", &user_id)?;
    }
    upsert_item(&tx, "cursor.accessToken", account.access_token.trim())?;
    upsert_item(&tx, "cursor.email", account.email.trim())?;
    tx.commit()
        .map_err(|err| format!("提交写入失败: {}", err))?;
    Ok(())
}

fn upsert_item(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
        (key, value),
    )
    .map_err(|err| format!("写入 {} 失败: {}", key, err))?;
    Ok(())
}

fn write_cp_token(account: &CursorAccount) -> Result<(), String> {
    let token_path = workbench_html_dir()?.join("cp_token.json");
    let refresh = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(account.access_token.trim());
    let inner = serde_json::json!({
        "accessToken": account.access_token.trim(),
        "refreshToken": refresh,
    });
    let encrypted = xor_encrypt(inner.to_string().as_bytes());
    let payload = serde_json::json!({
        "encrypted": encrypted,
        "ts": unix_millis(),
        "email": account.email.trim(),
    });
    write_file_allowing_auth(&token_path, &payload.to_string())?;
    Ok(())
}

fn patch_installed(source: &str) -> bool {
    source.contains(PATCH_MARKER) || source.contains(FORGE_MARKER)
}

fn patch_workbench_source(source: &str) -> Result<String, String> {
    if patch_installed(source) {
        return Ok(source.to_string());
    }
    if !source.contains(INJECT_ANCHOR) {
        return Err("未找到注入点，请确认 Cursor 版本是否兼容（需要 0.44+）".to_string());
    }
    let mut patched = bypass_integrity_notification(source);
    let injection = format!(
        "{PATCH_MARKER}{STORE_HOOK}{POLLER_SCRIPT}\n{FORGE_MARKER}{INJECT_ANCHOR}"
    );
    patched = patched.replacen(INJECT_ANCHOR, &injection, 1);
    Ok(patched)
}

fn bypass_integrity_notification(source: &str) -> String {
    let mut patched = insert_notification_return(source, NOTIFY_COMPACT);
    patched = insert_notification_return(&patched, NOTIFY_SPACED);
    patched
}

fn insert_notification_return(source: &str, signature: &str) -> String {
    let Some(index) = source.find(signature) else {
        return source.to_string();
    };
    let rest = &source[index + signature.len()..];
    if rest.starts_with("return;") {
        return source.to_string();
    }
    let mut next = String::with_capacity(source.len() + 7);
    next.push_str(&source[..index + signature.len()]);
    next.push_str("return;");
    next.push_str(rest);
    next
}

fn user_id_from_access_token(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64,
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let sub = value.get("sub").and_then(|item| item.as_str())?.trim();
    let user_id = sub.rsplit('|').next().unwrap_or(sub).trim();
    if user_id.is_empty() {
        None
    } else {
        Some(user_id.to_string())
    }
}

fn xor_encrypt(plain: &[u8]) -> String {
    let mixed: Vec<u8> = plain
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ XOR_KEY[index % XOR_KEY.len()])
        .collect();
    base64::engine::general_purpose::STANDARD.encode(mixed)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|item| !item.is_empty())
}

fn seamless_backup_path(js_path: &Path) -> PathBuf {
    let mut backup = js_path.as_os_str().to_os_string();
    backup.push(".seamless.bak");
    PathBuf::from(backup)
}

fn copy_file_allowing_auth(from: &Path, to: &Path) -> Result<(), String> {
    match fs::copy(from, to) {
        Ok(_) => Ok(()),
        Err(err) if is_permission_error(&err) => Err(permission_denied_message()),
        Err(err) => Err(explain_io_error(to, &err)),
    }
}

fn write_file_allowing_auth(path: &Path, contents: &str) -> Result<(), String> {
    match fs::write(path, contents) {
        Ok(()) => Ok(()),
        Err(err) if is_permission_error(&err) => Err(permission_denied_message()),
        Err(err) => Err(explain_io_error(path, &err)),
    }
}

fn is_permission_error(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::PermissionDenied || err.raw_os_error() == Some(1)
}

fn permission_denied_message() -> String {
    #[cfg(target_os = "macos")]
    {
        return open_app_management_settings();
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!(
            "没有写入 Cursor 安装目录的权限。{}",
            write_permission_hint()
        )
    }
}

fn open_app_management_settings() -> String {
    #[cfg(target_os = "macos")]
    {
        let urls = [
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AppBundles",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles",
        ];
        for url in urls {
            if Command::new("open")
                .arg(url)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            {
                logger::log_info("[Cursor Seamless] 已打开系统设置 App 管理");
                return "已打开系统设置的「App 管理」。请打开本程序的开关，然后重新点击无感切号。".to_string();
            }
        }
    }
    "macOS 阻止修改 Cursor。请打开「系统设置 → 隐私与安全性 → App 管理」，打开本程序的开关后重试。".to_string()
}

fn explain_io_error(path: &Path, err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "写入失败: {}（{}）。{}",
            path.display(),
            err,
            write_permission_hint()
        )
    } else {
        format!("写入失败: {}（{}）", path.display(), err)
    }
}

fn write_permission_hint() -> &'static str {
    "请确认 Cursor 未受系统保护、或把 Cursor 移出受保护目录"
}

const WORKBENCH_JS_RELATIVES: &[&str] = &[
    "Contents/Resources/app/out/vs/workbench/workbench.desktop.main.js",
    "resources/app/out/vs/workbench/workbench.desktop.main.js",
    "app/out/vs/workbench/workbench.desktop.main.js",
    "out/vs/workbench/workbench.desktop.main.js",
    "Contents/Resources/app/out/vs/code/electron-sandbox/workbench/workbench.desktop.main.js",
    "resources/app/out/vs/code/electron-sandbox/workbench/workbench.desktop.main.js",
    "app/out/vs/code/electron-sandbox/workbench/workbench.desktop.main.js",
];

const WORKBENCH_HTML_RELATIVES: &[&str] = &[
    "Contents/Resources/app/out/vs/code/electron-sandbox/workbench/workbench.html",
    "resources/app/out/vs/code/electron-sandbox/workbench/workbench.html",
    "app/out/vs/code/electron-sandbox/workbench/workbench.html",
    "out/vs/code/electron-sandbox/workbench/workbench.html",
];

fn workbench_js_path() -> Result<PathBuf, String> {
    let launch_path = cursor_launch_path()?;
    find_under_install(&launch_path, WORKBENCH_JS_RELATIVES).ok_or_else(|| {
        format!(
            "未找到 Cursor 的 workbench.desktop.main.js（已从 {} 向上查找）",
            launch_path.display()
        )
    })
}

fn workbench_html_dir() -> Result<PathBuf, String> {
    let launch_path = cursor_launch_path()?;
    if let Some(html) = find_under_install(&launch_path, WORKBENCH_HTML_RELATIVES) {
        return html
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法定位 workbench.html 目录".to_string());
    }
    workbench_js_path()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "无法定位 workbench 目录".to_string())
}

fn find_under_install(launch_path: &Path, relatives: &[&str]) -> Option<PathBuf> {
    for root in install_search_roots(launch_path) {
        for relative in relatives {
            let candidate = root.join(relative);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn cursor_launch_path() -> Result<PathBuf, String> {
    let configured = config::get_user_config().cursor_app_path;
    let raw = if configured.trim().is_empty() {
        cursor_instance::detect_and_save_cursor_launch_path(false)
            .ok_or("请先在设置中配置 Cursor 路径")?
    } else {
        configured
    };
    Ok(PathBuf::from(raw))
}

/// 从用户配置的 Cursor 路径往上走，覆盖 macOS .app、Windows 安装目录和 Linux 可执行文件所在目录。
fn install_search_roots(launch_path: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let text = launch_path.to_string_lossy();
    if let Some(index) = text.to_ascii_lowercase().find(".app") {
        let end = index + 4;
        if text.is_char_boundary(end) {
            roots.push(PathBuf::from(&text[..end]));
        }
    }

    let mut current = if launch_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("app"))
        .unwrap_or(false)
        || launch_path.is_file()
    {
        launch_path.parent().map(Path::to_path_buf)
    } else {
        Some(launch_path.to_path_buf())
    };

    for _ in 0..8 {
        let Some(dir) = current else {
            break;
        };
        if !roots.iter().any(|existing| existing == &dir) {
            roots.push(dir.clone());
        }
        if dir.file_name().and_then(|name| name.to_str()) == Some("bin") {
            if let Some(parent) = dir.parent() {
                for extra in [
                    parent.join("share").join("cursor"),
                    parent.join("lib").join("cursor"),
                ] {
                    if !roots.iter().any(|existing| existing == &extra) {
                        roots.push(extra);
                    }
                }
            }
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_roots_cover_mac_windows_and_linux() {
        let mac = install_search_roots(Path::new(
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
        ));
        assert!(mac.iter().any(|path| path.ends_with("Cursor.app")));

        let windows = install_search_roots(Path::new(
            "/Users/me/AppData/Local/Programs/Cursor/Cursor.exe",
        ));
        assert!(windows.iter().any(|path| path.ends_with("Cursor")));

        let linux = install_search_roots(Path::new("/usr/bin/cursor"));
        assert!(linux
            .iter()
            .any(|path| path.ends_with("share/cursor") || path.ends_with(r"share\cursor")));
    }

    #[test]
    fn patch_inserts_marker_and_notification_return() {
        let source = format!(
            "function x(){{ {NOTIFY_COMPACT}if(!ok){{}} }} {INJECT_ANCHOR}"
        );
        let patched = patch_workbench_source(&source).expect("patch");
        assert!(patched.contains(PATCH_MARKER));
        assert!(patched.contains(FORGE_MARKER));
        assert!(patched.contains("window.__ctSwitch"));
        assert!(patched.contains(&format!("{NOTIFY_COMPACT}return;")));
        assert_eq!(patched.matches(INJECT_ANCHOR).count(), 1);
        assert!(patch_installed(&patched));
        let again = patch_workbench_source(&patched).expect("idempotent");
        assert_eq!(again, patched);
    }

    #[test]
    fn patch_rejects_missing_anchor() {
        let err = patch_workbench_source("no anchor here").unwrap_err();
        assert!(err.contains("未找到注入点"));
    }

    #[test]
    fn fixed_auth_writes_user_id_even_when_key_was_absent() {
        let conn = Connection::open_in_memory().expect("memory db");
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"alg":"none","typ":"JWT"}"#,
        );
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            r#"{"sub":"auth0|user_01NEW"}"#,
        );
        let account = CursorAccount {
            id: "id".to_string(),
            email: "new@example.com".to_string(),
            auth_id: None,
            name: None,
            tags: None,
            access_token: format!("{header}.{payload}.sig"),
            refresh_token: Some("refresh-1".to_string()),
            membership_type: Some("pro".to_string()),
            subscription_status: Some("active".to_string()),
            sign_up_type: None,
            cursor_auth_raw: None,
            cursor_usage_raw: None,
            status: None,
            status_reason: None,
            quota_query_last_error: None,
            quota_query_last_error_at: None,
            usage_updated_at: None,
            created_at: 0,
            last_used: 0,
        };
        write_fixed_auth_conn(&conn, &account).expect("write");
        let user_id: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'cursorAuth/userId'",
                [],
                |row| row.get(0),
            )
            .expect("user id");
        assert_eq!(user_id, "user_01NEW");
        let email: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'cursorAuth/cachedEmail'",
                [],
                |row| row.get(0),
            )
            .expect("email");
        assert_eq!(email, "new@example.com");
    }
}
