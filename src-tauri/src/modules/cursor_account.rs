use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::models::cursor::{CursorAccount, CursorAccountIndex, CursorImportPayload};
use crate::modules::{account, logger};

const ACCOUNTS_INDEX_FILE: &str = "cursor_accounts.json";
const ACCOUNTS_DIR: &str = "cursor_accounts";
const CURSOR_QUOTA_ALERT_COOLDOWN_SECONDS: i64 = 10 * 60;
const CURSOR_ACCESS_TOKEN_REFRESH_THRESHOLD_SECONDS: i64 = 5 * 60;
const CURSOR_AUTH_VSCDB_RAW_KEY: &str = "_vscdb";

lazy_static::lazy_static! {
    static ref CURSOR_ACCOUNT_INDEX_LOCK: Mutex<()> = Mutex::new(());
    static ref CURSOR_QUOTA_ALERT_LAST_SENT: Mutex<HashMap<String, i64>> = Mutex::new(HashMap::new());
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn normalize_status_value(value: Option<&str>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
        }
    })
}

fn is_banned_status(value: Option<&str>) -> bool {
    matches!(
        normalize_status_value(value).as_deref(),
        Some("banned") | Some("ban") | Some("forbidden")
    )
}

fn is_banned_reason(value: Option<&str>) -> bool {
    let Some(reason) = normalize_status_value(value) else {
        return false;
    };
    reason.contains("banned")
        || reason.contains("forbidden")
        || reason.contains("suspended")
        || reason.contains("disabled")
        || reason.contains("封禁")
        || reason.contains("禁用")
}

pub(crate) fn is_banned_account(account: &CursorAccount) -> bool {
    is_banned_status(account.status.as_deref())
        || is_banned_reason(account.status_reason.as_deref())
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn get_data_dir() -> Result<PathBuf, String> {
    account::get_data_dir()
}

fn get_accounts_dir() -> Result<PathBuf, String> {
    let base = get_data_dir()?;
    let dir = base.join(ACCOUNTS_DIR);
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("创建 Cursor 账号目录失败: {}", e))?;
    }
    Ok(dir)
}

fn get_accounts_index_path() -> Result<PathBuf, String> {
    Ok(get_data_dir()?.join(ACCOUNTS_INDEX_FILE))
}

pub fn accounts_index_path_string() -> Result<String, String> {
    Ok(get_accounts_index_path()?.to_string_lossy().to_string())
}

fn normalize_account_id(account_id: &str) -> Result<String, String> {
    let trimmed = account_id.trim();
    if trimmed.is_empty() {
        return Err("账号 ID 不能为空".to_string());
    }

    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err("账号 ID 非法，包含路径字符".to_string());
    }

    let valid = trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.');
    if !valid {
        return Err("账号 ID 非法，仅允许字母/数字/._-".to_string());
    }

    Ok(trimmed.to_string())
}

fn resolve_account_file_path(account_id: &str) -> Result<PathBuf, String> {
    let normalized = normalize_account_id(account_id)?;
    Ok(get_accounts_dir()?.join(format!("{}.json", normalized)))
}

// ---------------------------------------------------------------------------
// Account file operations
// ---------------------------------------------------------------------------

pub fn load_account(account_id: &str) -> Option<CursorAccount> {
    let account_path = resolve_account_file_path(account_id).ok()?;
    if !account_path.exists() {
        return None;
    }
    let content = fs::read_to_string(&account_path).ok()?;
    match crate::modules::secure_account_storage::deserialize_account_file::<CursorAccount>(
        &account_path,
        &content,
    ) {
        Ok((account, needs_rotation)) => {
            if needs_rotation {
                let account_for_rewrite = account.clone();
                crate::modules::deferred_account_rewrite::schedule_account_rewrite_if_unchanged(
                    "cursor",
                    account_for_rewrite.id.clone(),
                    account_path.clone(),
                    content.as_bytes(),
                    move || {
                        crate::modules::secure_account_storage::serialize_account_file(
                            "cursor",
                            &account_for_rewrite,
                        )
                    },
                );
            }
            Some(account)
        }
        Err(_) => None,
    }
}

fn save_account_file(account: &CursorAccount) -> Result<(), String> {
    let path = resolve_account_file_path(account.id.as_str())?;
    let content =
        crate::modules::secure_account_storage::serialize_account_file("cursor", account)?;
    crate::modules::atomic_write::write_string_atomic(&path, &content)
        .map_err(|e| format!("保存账号失败: {}", e))
}

fn delete_account_file(account_id: &str) -> Result<(), String> {
    let path = resolve_account_file_path(account_id)?;
    if path.exists() {
        crate::modules::atomic_write::remove_file_locked(&path)
            .map_err(|e| format!("删除账号文件失败: {}", e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index operations
// ---------------------------------------------------------------------------

fn load_account_index() -> CursorAccountIndex {
    let path = match get_accounts_index_path() {
        Ok(p) => p,
        Err(_) => return CursorAccountIndex::new(),
    };

    if !path.exists() {
        return CursorAccountIndex::new();
    }

    match fs::read_to_string(path.as_path()) {
        Ok(content) => match crate::modules::atomic_write::parse_json_with_auto_restore::<
            CursorAccountIndex,
        >(&path, &content)
        {
            Ok(index) => index,
            Err(err) => {
                logger::log_warn(&format!(
                    "[Cursor Account] 账号索引解析失败，使用空索引兜底: path={}, error={}",
                    path.display(),
                    err
                ));
                CursorAccountIndex::new()
            }
        },
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 读取账号索引失败，使用空索引兜底: path={}, error={}",
                path.display(),
                err
            ));
            CursorAccountIndex::new()
        }
    }
}

fn load_account_index_checked() -> Result<CursorAccountIndex, String> {
    let path = get_accounts_index_path()?;
    if !path.exists() {
        return Ok(CursorAccountIndex::new());
    }

    let content = match fs::read_to_string(path.as_path()) {
        Ok(content) => content,
        Err(err) => {
            if !collect_account_ids_from_directory().is_empty() {
                logger::log_warn(&format!(
                    "[Cursor Account] 读取账号索引失败，将按账号目录补扫恢复: path={}, error={}",
                    path.display(),
                    err
                ));
                return Ok(CursorAccountIndex::new());
            }
            return Err(format!("读取账号索引失败: {}", err));
        }
    };

    if content.trim().is_empty() {
        return Ok(CursorAccountIndex::new());
    }

    match crate::modules::atomic_write::parse_json_with_auto_restore::<CursorAccountIndex>(
        &path, &content,
    ) {
        Ok(index) => Ok(index),
        Err(err) => {
            if !collect_account_ids_from_directory().is_empty() {
                logger::log_warn(&format!(
                    "[Cursor Account] 账号索引解析失败，将按账号目录补扫恢复: path={}, error={}",
                    path.display(),
                    err
                ));
                return Ok(CursorAccountIndex::new());
            }
            Err(crate::error::file_corrupted_error(
                ACCOUNTS_INDEX_FILE,
                &path.to_string_lossy(),
                &err.to_string(),
            ))
        }
    }
}

fn save_account_index(index: &CursorAccountIndex) -> Result<(), String> {
    let path = get_accounts_index_path()?;
    let content =
        serde_json::to_string_pretty(index).map_err(|e| format!("序列化账号索引失败: {}", e))?;
    crate::modules::atomic_write::write_string_atomic(&path, &content)
        .map_err(|e| format!("写入账号索引失败: {}", e))
}

fn refresh_summary(index: &mut CursorAccountIndex, account: &CursorAccount) {
    if let Some(summary) = index.accounts.iter_mut().find(|item| item.id == account.id) {
        *summary = account.summary();
        return;
    }
    index.accounts.push(account.summary());
}

fn upsert_account_record(account: CursorAccount) -> Result<CursorAccount, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let mut index = load_account_index();
    save_account_file(&account)?;
    refresh_summary(&mut index, &account);
    save_account_index(&index)?;
    Ok(account)
}

fn persist_quota_query_error(account_id: &str, message: &str) {
    let Some(mut account) = load_account(account_id) else {
        return;
    };
    account.quota_query_last_error = Some(message.to_string());
    account.quota_query_last_error_at = Some(chrono::Utc::now().timestamp_millis());
    let _ = upsert_account_record(account);
}

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_email_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value).and_then(|raw| {
        let lowered = raw.to_lowercase();
        if lowered.contains('@') {
            Some(lowered)
        } else {
            None
        }
    })
}

fn normalize_token_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value)
}

fn normalize_auth_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value).and_then(|raw| normalize_workos_user_id(&raw))
}

fn decode_access_token_payload(access_token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{}==", payload_b64),
        3 => format!("{}=", payload_b64),
        _ => payload_b64,
    };

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn extract_auth_id_from_access_token(access_token: &str) -> Option<String> {
    let value = decode_access_token_payload(access_token)?;
    normalize_non_empty(value.get("sub").and_then(|raw| raw.as_str()))
}

fn extract_access_token_exp(access_token: &str) -> Option<i64> {
    let value = decode_access_token_payload(access_token)?;
    value.get("exp").and_then(|raw| raw.as_i64())
}

fn access_token_needs_refresh(access_token: &str) -> bool {
    let Some(exp) = extract_access_token_exp(access_token) else {
        return true;
    };
    exp <= now_ts() + CURSOR_ACCESS_TOKEN_REFRESH_THRESHOLD_SECONDS
}

fn extract_auth_id_from_raw_value(raw: Option<&Value>) -> Option<String> {
    let obj = raw.and_then(|value| value.as_object())?;

    normalize_auth_identity(
        obj.get("authId")
            .and_then(|value| value.as_str())
            .or_else(|| obj.get("auth_id").and_then(|value| value.as_str()))
            .or_else(|| obj.get("workosId").and_then(|value| value.as_str()))
            .or_else(|| obj.get("workos_id").and_then(|value| value.as_str())),
    )
}

fn resolve_payload_auth_id(payload: &CursorImportPayload) -> Option<String> {
    normalize_auth_identity(payload.auth_id.as_deref())
        .or_else(|| extract_auth_id_from_raw_value(payload.cursor_auth_raw.as_ref()))
        .or_else(|| {
            normalize_auth_identity(
                extract_auth_id_from_access_token(payload.access_token.as_str()).as_deref(),
            )
        })
}

fn resolve_account_auth_id(account: &CursorAccount) -> Option<String> {
    normalize_auth_identity(account.auth_id.as_deref())
        .or_else(|| extract_auth_id_from_raw_value(account.cursor_auth_raw.as_ref()))
        .or_else(|| {
            normalize_auth_identity(
                extract_auth_id_from_access_token(account.access_token.as_str()).as_deref(),
            )
        })
}

fn cursor_identities_match(
    left_auth_id: Option<&str>,
    right_auth_id: Option<&str>,
    left_email: Option<&str>,
    right_email: Option<&str>,
    left_token: Option<&str>,
    right_token: Option<&str>,
) -> bool {
    if let (Some(left), Some(right)) = (left_auth_id, right_auth_id) {
        if left == right {
            return true;
        }
    }

    if let (Some(left), Some(right)) = (left_email, right_email) {
        if left == right {
            return true;
        }
        return false;
    }

    matches!(
        (left_token, right_token),
        (Some(left), Some(right)) if left == right
    )
}

fn cursor_auth_raw_object_mut(account: &mut CursorAccount) -> &mut serde_json::Map<String, Value> {
    if !matches!(account.cursor_auth_raw, Some(Value::Object(_))) {
        account.cursor_auth_raw = Some(Value::Object(serde_json::Map::new()));
    }

    match account.cursor_auth_raw.as_mut() {
        Some(Value::Object(obj)) => obj,
        _ => unreachable!("cursor_auth_raw 应始终为对象"),
    }
}

fn upsert_cursor_auth_raw_string(account: &mut CursorAccount, key: &str, value: Option<String>) {
    let Some(text) = normalize_non_empty(value.as_deref()) else {
        return;
    };
    cursor_auth_raw_object_mut(account).insert(key.to_string(), Value::String(text));
}

fn upsert_cursor_auth_raw_bool(account: &mut CursorAccount, key: &str, value: Option<bool>) {
    let Some(flag) = value else {
        return;
    };
    cursor_auth_raw_object_mut(account).insert(key.to_string(), Value::Bool(flag));
}

fn normalize_cursor_sign_up_type(value: Option<&str>) -> Option<String> {
    let raw = normalize_non_empty(value)?;
    match raw.as_str() {
        "SIGN_UP_TYPE_AUTH_0" => Some("Auth_0".to_string()),
        "SIGN_UP_TYPE_GOOGLE" => Some("Google".to_string()),
        "SIGN_UP_TYPE_GITHUB" => Some("Github".to_string()),
        "SIGN_UP_TYPE_WORKOS" => Some("WorkOS".to_string()),
        _ => Some(raw),
    }
}

fn accounts_are_duplicates(left: &CursorAccount, right: &CursorAccount) -> bool {
    let left_auth_id = resolve_account_auth_id(left);
    let right_auth_id = resolve_account_auth_id(right);
    let left_email = normalize_email_identity(Some(left.email.as_str()));
    let right_email = normalize_email_identity(Some(right.email.as_str()));
    let left_token = normalize_token_identity(Some(left.access_token.as_str()));
    let right_token = normalize_token_identity(Some(right.access_token.as_str()));

    cursor_identities_match(
        left_auth_id.as_deref(),
        right_auth_id.as_deref(),
        left_email.as_deref(),
        right_email.as_deref(),
        left_token.as_deref(),
        right_token.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

fn merge_string_list(
    primary: Option<Vec<String>>,
    secondary: Option<Vec<String>>,
) -> Option<Vec<String>> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();

    for source in [primary, secondary] {
        if let Some(values) = source {
            for value in values {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let key = trimmed.to_lowercase();
                if seen.insert(key) {
                    merged.push(trimmed.to_string());
                }
            }
        }
    }

    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

fn fill_if_empty_string(target: &mut String, source: &str) {
    if target.trim().is_empty() {
        let incoming = source.trim();
        if !incoming.is_empty() {
            *target = incoming.to_string();
        }
    }
}

fn fill_if_none<T: Clone>(target: &mut Option<T>, source: &Option<T>) {
    if target.is_none() {
        *target = source.clone();
    }
}

fn merge_duplicate_account(primary: &mut CursorAccount, duplicate: &CursorAccount) {
    fill_if_empty_string(&mut primary.email, duplicate.email.as_str());
    fill_if_empty_string(&mut primary.access_token, duplicate.access_token.as_str());

    fill_if_none(&mut primary.auth_id, &duplicate.auth_id);
    fill_if_none(&mut primary.name, &duplicate.name);
    fill_if_none(&mut primary.refresh_token, &duplicate.refresh_token);
    fill_if_none(&mut primary.membership_type, &duplicate.membership_type);
    fill_if_none(
        &mut primary.subscription_status,
        &duplicate.subscription_status,
    );
    fill_if_none(&mut primary.sign_up_type, &duplicate.sign_up_type);
    fill_if_none(&mut primary.cursor_auth_raw, &duplicate.cursor_auth_raw);
    fill_if_none(&mut primary.cursor_usage_raw, &duplicate.cursor_usage_raw);
    fill_if_none(&mut primary.status, &duplicate.status);
    fill_if_none(&mut primary.status_reason, &duplicate.status_reason);

    primary.tags = merge_string_list(primary.tags.clone(), duplicate.tags.clone());
    primary.created_at = primary.created_at.min(duplicate.created_at);
    primary.last_used = primary.last_used.max(duplicate.last_used);
}

fn choose_primary_account_index(group: &[usize], accounts: &[CursorAccount]) -> usize {
    group
        .iter()
        .copied()
        .max_by(|left, right| {
            let left_account = &accounts[*left];
            let right_account = &accounts[*right];
            left_account
                .last_used
                .cmp(&right_account.last_used)
                .then_with(|| right_account.created_at.cmp(&left_account.created_at))
        })
        .unwrap_or(group[0])
}

fn collect_account_ids_from_directory() -> Vec<String> {
    let accounts_dir = match get_accounts_dir() {
        Ok(dir) => dir,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 获取账号目录失败，跳过目录补扫: {}",
                err
            ));
            return Vec::new();
        }
    };

    let entries = match fs::read_dir(&accounts_dir) {
        Ok(value) => value,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 读取账号目录失败，跳过目录补扫: path={}, error={}",
                accounts_dir.display(),
                err
            ));
            return Vec::new();
        }
    };

    let mut ids = Vec::new();
    for entry in entries {
        let Ok(item) = entry else {
            continue;
        };
        let path = item.path();
        if !path.is_file() {
            continue;
        }

        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if !is_json {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(account_id) = normalize_account_id(stem) else {
            logger::log_warn(&format!(
                "[Cursor Account] 检测到非法账号文件名，已忽略: file={}",
                path.display()
            ));
            continue;
        };
        ids.push(account_id);
    }

    ids.sort();
    ids.dedup();
    ids
}

fn normalize_account_index(index: &mut CursorAccountIndex) -> Vec<CursorAccount> {
    let mut loaded_accounts = Vec::new();
    let mut seen_account_ids = HashSet::new();
    let mut seen_summary_ids = HashSet::new();

    for summary in &index.accounts {
        if !seen_summary_ids.insert(summary.id.clone()) {
            continue;
        }
        if let Some(account) = load_account(&summary.id) {
            if seen_account_ids.insert(account.id.clone()) {
                loaded_accounts.push(account);
            }
        }
    }

    let mut recovered_count = 0usize;
    for account_id in collect_account_ids_from_directory() {
        if seen_account_ids.contains(&account_id) {
            continue;
        }
        if let Some(account) = load_account(&account_id) {
            if seen_account_ids.insert(account.id.clone()) {
                if !seen_summary_ids.contains(&account_id) {
                    recovered_count += 1;
                }
                loaded_accounts.push(account);
            }
        }
    }
    if recovered_count > 0 {
        logger::log_warn(&format!(
            "[Cursor Account] 检测到索引缺失，已从账号目录恢复 {} 个账号",
            recovered_count
        ));
    }

    if loaded_accounts.len() <= 1 {
        index.accounts = loaded_accounts
            .iter()
            .map(|account| account.summary())
            .collect();
        return loaded_accounts;
    }

    let mut parents: Vec<usize> = (0..loaded_accounts.len()).collect();

    fn find(parents: &mut [usize], idx: usize) -> usize {
        let parent = parents[idx];
        if parent == idx {
            return idx;
        }
        let root = find(parents, parent);
        parents[idx] = root;
        root
    }

    fn union(parents: &mut [usize], left: usize, right: usize) {
        let left_root = find(parents, left);
        let right_root = find(parents, right);
        if left_root != right_root {
            parents[right_root] = left_root;
        }
    }

    let total = loaded_accounts.len();
    for left in 0..total {
        for right in (left + 1)..total {
            if accounts_are_duplicates(&loaded_accounts[left], &loaded_accounts[right]) {
                union(&mut parents, left, right);
            }
        }
    }

    let mut grouped: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..total {
        let root = find(&mut parents, idx);
        grouped.entry(root).or_default().push(idx);
    }

    let mut processed_roots = HashSet::new();
    let mut normalized_accounts = Vec::new();
    let mut removed_ids = Vec::new();
    for idx in 0..total {
        let root = find(&mut parents, idx);
        if !processed_roots.insert(root) {
            continue;
        }
        let Some(group) = grouped.get(&root) else {
            continue;
        };

        if group.len() == 1 {
            normalized_accounts.push(loaded_accounts[group[0]].clone());
            continue;
        }

        let primary_idx = choose_primary_account_index(group, &loaded_accounts);
        let mut primary = loaded_accounts[primary_idx].clone();
        for member in group {
            if *member == primary_idx {
                continue;
            }
            merge_duplicate_account(&mut primary, &loaded_accounts[*member]);
            removed_ids.push(loaded_accounts[*member].id.clone());
        }

        normalized_accounts.push(primary);
    }

    if !removed_ids.is_empty() {
        for account in &normalized_accounts {
            if let Err(err) = save_account_file(account) {
                logger::log_warn(&format!(
                    "[Cursor Account] 保存去重账号失败: id={}, error={}",
                    account.id, err
                ));
            }
        }
        for account_id in &removed_ids {
            if let Err(err) = delete_account_file(account_id) {
                logger::log_warn(&format!(
                    "[Cursor Account] 删除重复账号文件失败: id={}, error={}",
                    account_id, err
                ));
            }
        }
        logger::log_warn(&format!(
            "[Cursor Account] 检测到重复账号并已合并: removed_ids={}",
            removed_ids.join(",")
        ));
    }

    index.accounts = normalized_accounts
        .iter()
        .map(|account| account.summary())
        .collect();
    normalized_accounts
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

pub fn list_accounts() -> Vec<CursorAccount> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut index = load_account_index();
    let had_index_accounts = !index.accounts.is_empty();
    let index_before_normalize = serde_json::to_vec(&index).ok();
    let accounts = normalize_account_index(&mut index);
    if had_index_accounts && accounts.is_empty() {
        logger::log_warn(
            "[Cursor Account] 账号索引中存在账号，但详情文件均无法读取，已跳过空索引写回",
        );
        return accounts;
    }
    let index_changed = index_before_normalize
        .as_ref()
        .map(|before| Some(before.as_slice()) != serde_json::to_vec(&index).ok().as_deref())
        .unwrap_or(true);
    if index_changed {
        if let Err(err) = save_account_index(&index) {
            logger::log_warn(&format!("[Cursor Account] 保存账号索引失败: {}", err));
        }
    }
    accounts
}

pub fn list_accounts_checked() -> Result<Vec<CursorAccount>, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let mut index = load_account_index_checked()?;
    let had_index_accounts = !index.accounts.is_empty();
    let index_before_normalize = serde_json::to_vec(&index).ok();
    let accounts = normalize_account_index(&mut index);
    if had_index_accounts && accounts.is_empty() {
        return Err("Cursor 账号索引中存在账号，但详情文件均无法读取；已保留前端缓存，请从账号备份或本地账号文件恢复。".to_string());
    }
    let index_changed = index_before_normalize
        .as_ref()
        .map(|before| Some(before.as_slice()) != serde_json::to_vec(&index).ok().as_deref())
        .unwrap_or(true);
    if index_changed {
        if let Err(err) = save_account_index(&index) {
            logger::log_warn(&format!("[Cursor Account] 保存账号索引失败: {}", err));
        }
    }
    Ok(accounts)
}

fn apply_payload(
    account: &mut CursorAccount,
    payload: CursorImportPayload,
    resolved_auth_id: Option<String>,
) {
    let incoming_email = payload.email.trim().to_string();
    if !incoming_email.is_empty() {
        account.email = incoming_email;
    } else if !account.email.contains('@') {
        account.email.clear();
    }
    account.name = payload.name;
    account.access_token = payload.access_token;
    account.refresh_token = payload.refresh_token;
    account.membership_type = payload.membership_type;
    account.subscription_status = payload.subscription_status;
    account.sign_up_type = payload.sign_up_type;
    account.cursor_auth_raw = payload.cursor_auth_raw;
    account.cursor_usage_raw = payload.cursor_usage_raw;
    if let Some(auth_id) = resolved_auth_id {
        account.auth_id = Some(auth_id.clone());
        upsert_cursor_auth_raw_string(account, "authId", Some(auth_id));
    }
    account.status = payload.status;
    account.status_reason = payload.status_reason;
    account.last_used = now_ts();
}

pub fn upsert_account(payload: CursorImportPayload) -> Result<CursorAccount, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;

    let now = now_ts();
    let mut index = load_account_index();
    let incoming_auth_id = resolve_payload_auth_id(&payload);
    let incoming_email = normalize_email_identity(Some(payload.email.as_str()));
    let incoming_token = normalize_token_identity(Some(payload.access_token.as_str()));

    let identity_seed = incoming_auth_id
        .clone()
        .or_else(|| incoming_email.clone())
        .or_else(|| incoming_token.clone())
        .unwrap_or_else(|| "cursor_user".to_string())
        .to_lowercase();
    let generated_id = format!("cursor_{:x}", md5::compute(identity_seed.as_bytes()));

    let account_id = index
        .accounts
        .iter()
        .filter_map(|item| load_account(&item.id))
        .find(|account| {
            let existing_auth_id = resolve_account_auth_id(account);
            let existing_email = normalize_email_identity(Some(account.email.as_str()));
            let existing_token = normalize_token_identity(Some(account.access_token.as_str()));
            cursor_identities_match(
                existing_auth_id.as_deref(),
                incoming_auth_id.as_deref(),
                existing_email.as_deref(),
                incoming_email.as_deref(),
                existing_token.as_deref(),
                incoming_token.as_deref(),
            )
        })
        .map(|account| account.id)
        .unwrap_or(generated_id);

    let existing = load_account(&account_id);
    let tags = existing.as_ref().and_then(|acc| acc.tags.clone());
    let created_at = existing.as_ref().map(|acc| acc.created_at).unwrap_or(now);

    let mut account = existing.unwrap_or(CursorAccount {
        id: account_id.clone(),
        email: payload.email.clone(),
        auth_id: incoming_auth_id.clone(),
        name: payload.name.clone(),
        tags,
        access_token: payload.access_token.clone(),
        refresh_token: payload.refresh_token.clone(),
        membership_type: payload.membership_type.clone(),
        subscription_status: payload.subscription_status.clone(),
        sign_up_type: payload.sign_up_type.clone(),
        cursor_auth_raw: payload.cursor_auth_raw.clone(),
        cursor_usage_raw: payload.cursor_usage_raw.clone(),
        status: payload.status.clone(),
        status_reason: payload.status_reason.clone(),
        quota_query_last_error: None,
        quota_query_last_error_at: None,
        usage_updated_at: None,
        created_at,
        last_used: now,
    });

    apply_payload(&mut account, payload, incoming_auth_id);
    account.id = account_id;
    account.created_at = created_at;
    account.quota_query_last_error = None;
    account.quota_query_last_error_at = None;
    account.last_used = now;

    save_account_file(&account)?;
    refresh_summary(&mut index, &account);
    save_account_index(&index)?;

    logger::log_info(&format!(
        "Cursor 账号已保存: id={}, email={}",
        account.id, account.email
    ));
    Ok(account)
}

pub fn remove_account(account_id: &str) -> Result<(), String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let mut index = load_account_index();
    index.accounts.retain(|item| item.id != account_id);
    save_account_index(&index)?;
    delete_account_file(account_id)?;
    Ok(())
}

pub fn remove_accounts(account_ids: &[String]) -> Result<(), String> {
    for id in account_ids {
        remove_account(id)?;
    }
    Ok(())
}

pub fn update_account_tags(account_id: &str, tags: Vec<String>) -> Result<CursorAccount, String> {
    let mut account = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    account.tags = Some(tags);
    account.last_used = now_ts();
    let updated = account.clone();
    upsert_account_record(account)?;
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Import / Export
// ---------------------------------------------------------------------------

/// Normalize common Cursor session separators (`%3A%3A` → `::`).
fn normalize_cursor_token_separators(raw: &str) -> String {
    raw.trim()
        .replace("%3A%3A", "::")
        .replace("%3a%3a", "::")
}

fn is_likely_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return false;
    }
    decode_access_token_payload(token).is_some()
}

fn normalize_workos_user_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let user_id = trimmed.rsplit('|').next().unwrap_or(trimmed).trim();
    if user_id.is_empty() {
        None
    } else {
        Some(user_id.to_string())
    }
}

fn payload_from_token_parts(
    email: Option<&str>,
    auth_id_hint: Option<&str>,
    access_token: &str,
) -> Result<CursorImportPayload, String> {
    let access_token = access_token.trim();
    if !is_likely_jwt(access_token) {
        return Err("无效的 Cursor JWT Token".to_string());
    }

    let jwt_auth_id = extract_workos_user_id(access_token)
        .or_else(|| extract_auth_id_from_access_token(access_token).and_then(|id| normalize_workos_user_id(&id)));

    let hint_auth_id = auth_id_hint.and_then(normalize_workos_user_id);
    let auth_id = match (hint_auth_id, jwt_auth_id) {
        (Some(hint), Some(from_jwt)) if hint != from_jwt => {
            logger::log_warn(&format!(
                "[Cursor Import] auth_id 与 JWT sub 不一致，以 JWT 为准: hint={}, jwt={}",
                hint, from_jwt
            ));
            Some(from_jwt)
        }
        (hint, from_jwt) => hint.or(from_jwt),
    };

    let email = normalize_email_identity(email).unwrap_or_else(|| "unknown".to_string());

    Ok(CursorImportPayload {
        email,
        auth_id,
        name: None,
        access_token: access_token.to_string(),
        refresh_token: None,
        membership_type: None,
        subscription_status: None,
        sign_up_type: None,
        cursor_auth_raw: None,
        cursor_usage_raw: None,
        status: None,
        status_reason: None,
    })
}

/// Parse one Cursor token line.
///
/// Supported:
/// - `email----user_id::jwt`
/// - `user_id::jwt` / `WorkosCursorSessionToken=user_id%3A%3Ajwt`
/// - bare JWT (`eyJ...`)
pub fn parse_cursor_token_line(line: &str) -> Result<CursorImportPayload, String> {
    let normalized = normalize_cursor_token_separators(line);
    let mut trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err("空行".to_string());
    }

    if let Some(rest) = trimmed.strip_prefix("WorkosCursorSessionToken=") {
        trimmed = rest.trim();
    }

    if let Some((email_part, rest)) = trimmed.split_once("----") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Err("email---- 格式缺少 token 部分".to_string());
        }
        if let Some((user_part, jwt_part)) = rest.split_once("::") {
            return payload_from_token_parts(
                Some(email_part.trim()),
                Some(user_part.trim()),
                jwt_part.trim(),
            );
        }
        return payload_from_token_parts(Some(email_part.trim()), None, rest);
    }

    if let Some((user_part, jwt_part)) = trimmed.split_once("::") {
        let user_part = user_part.trim();
        let jwt_part = jwt_part.trim();
        if user_part.starts_with("user_") || user_part.contains('|') {
            return payload_from_token_parts(None, Some(user_part), jwt_part);
        }
    }

    if is_likely_jwt(trimmed) {
        return payload_from_token_parts(None, None, trimmed);
    }

    Err("无法识别的 Cursor Token 格式，支持: email----user_id::jwt / user_id::jwt / JWT".to_string())
}

/// Parse multi-line Cursor token text. Empty lines are skipped.
/// Partial failures are logged; returns error only when nothing parses.
pub fn parse_cursor_token_text(text: &str) -> Result<Vec<CursorImportPayload>, String> {
    let mut payloads = Vec::new();
    let mut errors = Vec::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_cursor_token_line(line) {
            Ok(payload) => payloads.push(payload),
            Err(err) => errors.push(format!("第 {} 行: {}", idx + 1, err)),
        }
    }

    if payloads.is_empty() {
        if errors.is_empty() {
            return Err("未找到可导入的 Token".to_string());
        }
        return Err(format!("全部解析失败: {}", errors.join("; ")));
    }

    if !errors.is_empty() {
        logger::log_warn(&format!(
            "[Cursor Import] 部分行解析失败 (成功 {} 条): {}",
            payloads.len(),
            errors.join("; ")
        ));
    }

    Ok(payloads)
}

pub fn import_from_token_text(text: &str) -> Result<Vec<CursorAccount>, String> {
    let payloads = parse_cursor_token_text(text)?;
    let mut result = Vec::with_capacity(payloads.len());
    for payload in payloads {
        result.push(upsert_account(payload)?);
    }
    Ok(result)
}

fn looks_like_cursor_token_text(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') || trimmed.starts_with('[') {
        return false;
    }
    trimmed.contains("----")
        || trimmed.contains("::")
        || trimmed.contains("%3A%3A")
        || trimmed.contains("%3a%3a")
        || trimmed.contains("WorkosCursorSessionToken=")
        || trimmed.lines().any(|line| is_likely_jwt(line.trim()))
}

pub fn format_account_token_line(account: &CursorAccount) -> String {
    let email = {
        let trimmed = account.email.trim();
        if trimmed.is_empty() {
            "unknown"
        } else {
            trimmed
        }
    };

    let auth_id = resolve_account_auth_id(account)
        .and_then(|id| normalize_workos_user_id(&id))
        .or_else(|| extract_workos_user_id(&account.access_token))
        .unwrap_or_else(|| "unknown".to_string());

    format!("{}----{}::{}", email, auth_id, account.access_token.trim())
}

pub fn export_accounts_text(account_ids: &[String]) -> Result<String, String> {
    let mut lines = Vec::new();
    for id in account_ids {
        if let Some(account) = load_account(id) {
            lines.push(format_account_token_line(&account));
        }
    }
    if lines.is_empty() {
        return Err("没有可导出的账号".to_string());
    }
    Ok(lines.join("\n"))
}

fn clone_object_value(value: Option<&Value>) -> Option<Value> {
    value.and_then(|raw| {
        if raw.is_object() {
            Some(raw.clone())
        } else {
            None
        }
    })
}

fn extract_string(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = obj.get(*key) {
            if let Some(text) = value.as_str().map(str::trim).filter(|v| !v.is_empty()) {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn payload_from_import_value(raw: Value) -> Result<CursorImportPayload, String> {
    let obj = raw
        .as_object()
        .ok_or_else(|| "Cursor 导入 JSON 必须是对象".to_string())?;

    let email = extract_string(obj, &["email", "cachedEmail", "cursor_email"])
        .ok_or_else(|| "缺少 email 字段".to_string())?;
    let access_token = extract_string(
        obj,
        &[
            "access_token",
            "accessToken",
            "token",
            "cursor_access_token",
        ],
    )
    .ok_or_else(|| "缺少 access_token 字段".to_string())?;

    let name = extract_string(obj, &["name", "displayName"]);
    let refresh_token = extract_string(
        obj,
        &["refresh_token", "refreshToken", "cursor_refresh_token"],
    );
    let membership_type = extract_string(
        obj,
        &[
            "membership_type",
            "membershipType",
            "stripeMembershipType",
            "plan",
        ],
    );
    let subscription_status = extract_string(
        obj,
        &[
            "subscription_status",
            "subscriptionStatus",
            "stripeSubscriptionStatus",
        ],
    );
    let sign_up_type = extract_string(obj, &["sign_up_type", "signUpType", "cachedSignUpType"]);
    let status = extract_string(obj, &["status"]);
    let status_reason = extract_string(obj, &["status_reason", "statusReason"]);

    let cursor_auth_raw = clone_object_value(obj.get("cursor_auth_raw"))
        .or_else(|| clone_object_value(obj.get("cursorAuthRaw")));
    let cursor_usage_raw = clone_object_value(obj.get("cursor_usage_raw"))
        .or_else(|| clone_object_value(obj.get("cursorUsageRaw")));
    let auth_id = normalize_auth_identity(
        extract_string(obj, &["auth_id", "authId", "workos_id", "workosId"])
            .or_else(|| extract_auth_id_from_raw_value(cursor_auth_raw.as_ref()))
            .or_else(|| extract_auth_id_from_access_token(access_token.as_str()))
            .as_deref(),
    );

    Ok(CursorImportPayload {
        email,
        auth_id,
        name,
        access_token,
        refresh_token,
        membership_type,
        subscription_status,
        sign_up_type,
        cursor_auth_raw,
        cursor_usage_raw,
        status,
        status_reason,
    })
}

fn payloads_from_import_json_value(value: Value) -> Result<Vec<CursorImportPayload>, String> {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Err("导入数组为空".to_string());
            }
            let mut payloads = Vec::with_capacity(items.len());
            for (idx, item) in items.into_iter().enumerate() {
                let payload = payload_from_import_value(item)
                    .map_err(|e| format!("第 {} 条 Cursor 账号解析失败: {}", idx + 1, e))?;
                payloads.push(payload);
            }
            Ok(payloads)
        }
        Value::Object(mut obj) => {
            let object_value = Value::Object(obj.clone());
            if let Ok(payload) = payload_from_import_value(object_value) {
                return Ok(vec![payload]);
            }

            if let Some(accounts) = obj
                .remove("accounts")
                .or_else(|| obj.remove("items"))
                .and_then(|raw| raw.as_array().cloned())
            {
                if accounts.is_empty() {
                    return Err("导入数组为空".to_string());
                }
                let mut payloads = Vec::with_capacity(accounts.len());
                for (idx, item) in accounts.into_iter().enumerate() {
                    let payload = payload_from_import_value(item)
                        .map_err(|e| format!("第 {} 条 Cursor 账号解析失败: {}", idx + 1, e))?;
                    payloads.push(payload);
                }
                return Ok(payloads);
            }

            Err("无法解析 Cursor 导入对象".to_string())
        }
        _ => Err("Cursor 导入 JSON 必须是对象或数组".to_string()),
    }
}

pub fn import_from_json(json_content: &str) -> Result<Vec<CursorAccount>, String> {
    if let Ok(account) = serde_json::from_str::<CursorAccount>(json_content) {
        let saved = upsert_account_record(account)?;
        return Ok(vec![saved]);
    }

    if let Ok(accounts) = serde_json::from_str::<Vec<CursorAccount>>(json_content) {
        let mut result = Vec::new();
        for account in accounts {
            let saved = upsert_account_record(account)?;
            result.push(saved);
        }
        return Ok(result);
    }

    if let Ok(value) = serde_json::from_str::<Value>(json_content) {
        if let Ok(payloads) = payloads_from_import_json_value(value) {
            let mut result = Vec::with_capacity(payloads.len());
            for payload in payloads {
                let saved = upsert_account(payload)?;
                result.push(saved);
            }
            return Ok(result);
        }
    }

    // Fall back to token-line formats when content is not JSON.
    if looks_like_cursor_token_text(json_content) {
        return import_from_token_text(json_content);
    }

    Err("无法解析 JSON 内容".to_string())
}

pub fn export_accounts(account_ids: &[String]) -> Result<String, String> {
    let accounts: Vec<CursorAccount> = account_ids
        .iter()
        .filter_map(|id| load_account(id))
        .collect();
    serde_json::to_string_pretty(&accounts).map_err(|e| format!("序列化失败: {}", e))
}

// ---------------------------------------------------------------------------
// Local import (read from Cursor's state.vscdb)
// ---------------------------------------------------------------------------

pub fn get_default_cursor_data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or("无法获取用户主目录")?;
        return Ok(home.join("Library/Application Support/Cursor"));
    }

    #[cfg(target_os = "windows")]
    {
        let appdata =
            std::env::var("APPDATA").map_err(|_| "无法获取 APPDATA 环境变量".to_string())?;
        return Ok(PathBuf::from(appdata).join("Cursor"));
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or("无法获取用户主目录")?;
        return Ok(home.join(".config/Cursor"));
    }

    #[allow(unreachable_code)]
    Err("Cursor 账号导入仅支持 macOS、Windows 和 Linux".to_string())
}

pub fn get_default_cursor_state_db_path() -> Result<PathBuf, String> {
    Ok(get_default_cursor_data_dir()?
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

fn read_vscdb_item(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .and_then(|v| {
        let trimmed = v.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

const CURSOR_IDENTITY_EXACT_KEYS: &[&str] = &[
    "cursor.accessToken",
    "cursor.email",
    "glass.lastSignedInAuthId",
    "adminSettings.cachedAuthId",
];

const CURSOR_STALE_ACCOUNT_CACHE_KEYS: &[&str] = &[
    "cursorAuth/cachedTeam",
    "cursorAuth/cachedScopedProfile",
];

const CURSOR_KEYCHAIN_ACCOUNT: &str = "cursor-user";
const CURSOR_KEYCHAIN_ACCESS_SERVICE: &str = "cursor-access-token";
const CURSOR_KEYCHAIN_REFRESH_SERVICE: &str = "cursor-refresh-token";

fn is_cursor_identity_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with("grok|") {
        return false;
    }
    trimmed.starts_with("auth0|") || trimmed.starts_with("user_")
}

fn is_identity_snapshot_key(key: &str) -> bool {
    key.starts_with("cursorAuth/") || CURSOR_IDENTITY_EXACT_KEYS.contains(&key)
}

fn should_keep_identity_row(key: &str, value: &str) -> bool {
    if key == "adminSettings.cachedAuthId" {
        return is_cursor_identity_value(value);
    }
    is_identity_snapshot_key(key)
}

fn read_identity_vscdb_rows(
    conn: &Connection,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM ItemTable \
             WHERE key LIKE 'cursorAuth/%' \
                OR key IN ( \
                    'cursor.accessToken', \
                    'cursor.email', \
                    'glass.lastSignedInAuthId', \
                    'adminSettings.cachedAuthId' \
                )",
        )
        .map_err(|e| format!("读取 Cursor 登录快照失败: {}", e))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读取 Cursor 登录快照失败: {}", e))?;
    let mut map = serde_json::Map::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("读取 Cursor 登录快照失败: {}", e))?
    {
        let key: String = row
            .get(0)
            .map_err(|e| format!("读取 Cursor 登录快照失败: {}", e))?;
        let value: String = row
            .get(1)
            .map_err(|e| format!("读取 Cursor 登录快照失败: {}", e))?;
        if should_keep_identity_row(&key, &value) {
            map.insert(key, Value::String(value));
        }
    }
    Ok(map)
}

fn extract_vscdb_auth_rows(raw: Option<&Value>) -> Vec<(String, String)> {
    let Some(obj) = raw
        .and_then(Value::as_object)
        .and_then(|map| map.get(CURSOR_AUTH_VSCDB_RAW_KEY))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    obj.iter()
        .filter_map(|(key, value)| {
            value.as_str().and_then(|text| {
                if should_keep_identity_row(key, text) {
                    Some((key.clone(), text.to_string()))
                } else {
                    None
                }
            })
        })
        .collect()
}

pub fn read_local_cursor_auth() -> Result<Option<CursorImportPayload>, String> {
    let db_path = get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(&db_path)
        .map_err(|e| format!("打开 Cursor 本地数据库失败({}): {}", db_path.display(), e))?;

    let access_token = match read_vscdb_item(&conn, "cursorAuth/accessToken") {
        Some(t) => t,
        None => return Ok(None),
    };

    let email = read_vscdb_item(&conn, "cursorAuth/cachedEmail").unwrap_or_default();
    if email.is_empty() {
        return Ok(None);
    }

    let refresh_token = read_vscdb_item(&conn, "cursorAuth/refreshToken");
    let auth_id = normalize_auth_identity(
        read_vscdb_item(&conn, "cursorAuth/authId")
            .or_else(|| extract_auth_id_from_access_token(access_token.as_str()))
            .as_deref(),
    );
    let membership_type = read_vscdb_item(&conn, "cursorAuth/stripeMembershipType");
    let subscription_status = read_vscdb_item(&conn, "cursorAuth/stripeSubscriptionStatus");
    let sign_up_type = read_vscdb_item(&conn, "cursorAuth/cachedSignUpType");

    let mut auth_raw = serde_json::Map::new();
    auth_raw.insert(
        "accessToken".to_string(),
        Value::String(access_token.clone()),
    );
    if let Some(ref rt) = refresh_token {
        auth_raw.insert("refreshToken".to_string(), Value::String(rt.clone()));
    }
    if let Some(ref auth_id_value) = auth_id {
        auth_raw.insert("authId".to_string(), Value::String(auth_id_value.clone()));
    }
    auth_raw.insert("cachedEmail".to_string(), Value::String(email.clone()));
    if let Some(ref mt) = membership_type {
        auth_raw.insert(
            "stripeMembershipType".to_string(),
            Value::String(mt.clone()),
        );
    }
    if let Some(ref ss) = subscription_status {
        auth_raw.insert(
            "stripeSubscriptionStatus".to_string(),
            Value::String(ss.clone()),
        );
    }
    if let Some(ref st) = sign_up_type {
        auth_raw.insert("cachedSignUpType".to_string(), Value::String(st.clone()));
    }

    let vscdb_rows = read_identity_vscdb_rows(&conn)?;
    if !vscdb_rows.is_empty() {
        auth_raw.insert(CURSOR_AUTH_VSCDB_RAW_KEY.to_string(), Value::Object(vscdb_rows));
    }

    Ok(Some(CursorImportPayload {
        email,
        auth_id,
        name: None,
        access_token,
        refresh_token,
        membership_type,
        subscription_status,
        sign_up_type,
        cursor_auth_raw: Some(Value::Object(auth_raw)),
        cursor_usage_raw: None,
        status: None,
        status_reason: None,
    }))
}

pub fn import_from_local() -> Result<Option<CursorAccount>, String> {
    let payload = match read_local_cursor_auth()? {
        Some(p) => p,
        None => return Ok(None),
    };
    let account = upsert_account(payload)?;
    logger::log_info(&format!(
        "[Cursor Account] 从本地导入成功: id={}, email={}",
        account.id, account.email
    ));
    Ok(Some(account))
}

// ---------------------------------------------------------------------------
// Inject (write auth fields back to Cursor's state.vscdb)
// ---------------------------------------------------------------------------

fn upsert_vscdb_item(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
        (key, value),
    )
    .map_err(|e| format!("写入 {} 失败: {}", key, e))?;
    Ok(())
}

fn delete_vscdb_item(conn: &Connection, key: &str) -> Result<(), String> {
    conn.execute("DELETE FROM ItemTable WHERE key = ?1", [key])
        .map_err(|e| format!("删除 {} 失败: {}", key, e))?;
    Ok(())
}

fn inject_account_into_conn(conn: &Connection, account: &CursorAccount) -> Result<(), String> {
    let live_rows = read_identity_vscdb_rows(conn)?;
    let live_keys: HashSet<String> = live_rows.keys().cloned().collect();

    let mut restored_keys = HashSet::new();
    for (key, value) in extract_vscdb_auth_rows(account.cursor_auth_raw.as_ref()) {
        upsert_vscdb_item(conn, &key, &value)?;
        restored_keys.insert(key);
    }

    upsert_vscdb_item(conn, "cursorAuth/accessToken", &account.access_token)?;

    let refresh_token = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(account.access_token.as_str());
    upsert_vscdb_item(conn, "cursorAuth/refreshToken", refresh_token)?;

    upsert_vscdb_item(conn, "cursorAuth/cachedEmail", &account.email)?;

    let sign_up_type = account
        .sign_up_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Auth_0");
    upsert_vscdb_item(conn, "cursorAuth/cachedSignUpType", sign_up_type)?;

    upsert_vscdb_item(conn, "cursor.accessToken", &account.access_token)?;
    upsert_vscdb_item(conn, "cursor.email", &account.email)?;

    if let Some(jwt_sub) = extract_auth_id_from_access_token(&account.access_token) {
        let key_known = |key: &str| live_keys.contains(key) || restored_keys.contains(key);
        if key_known("cursorAuth/stripeMembershipAuthId") {
            upsert_vscdb_item(conn, "cursorAuth/stripeMembershipAuthId", &jwt_sub)?;
        }
        if key_known("cursorAuth/userId") {
            if let Some(user_id) = normalize_workos_user_id(&jwt_sub) {
                upsert_vscdb_item(conn, "cursorAuth/userId", &user_id)?;
            }
        }
        if key_known("glass.lastSignedInAuthId") {
            upsert_vscdb_item(conn, "glass.lastSignedInAuthId", &jwt_sub)?;
        }
        if key_known("adminSettings.cachedAuthId") {
            upsert_vscdb_item(conn, "adminSettings.cachedAuthId", &jwt_sub)?;
        }
    }

    apply_stale_account_cache(conn, &live_keys, &restored_keys, account)?;

    if let Err(err) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
        logger::log_warn(&format!(
            "[Cursor Account] WAL checkpoint 失败: id={}, email={}, error={}",
            account.id, account.email, err
        ));
    }
    Ok(())
}

fn apply_stale_account_cache(
    conn: &Connection,
    live_keys: &HashSet<String>,
    restored_keys: &HashSet<String>,
    account: &CursorAccount,
) -> Result<(), String> {
    apply_optional_account_cache(
        conn,
        live_keys,
        restored_keys,
        "cursorAuth/stripeMembershipType",
        account.membership_type.as_deref(),
    )?;
    apply_optional_account_cache(
        conn,
        live_keys,
        restored_keys,
        "cursorAuth/stripeSubscriptionStatus",
        account.subscription_status.as_deref(),
    )?;

    for key in CURSOR_STALE_ACCOUNT_CACHE_KEYS {
        if !restored_keys.contains(*key) && live_keys.contains(*key) {
            delete_vscdb_item(conn, key)?;
        }
    }
    Ok(())
}

fn apply_optional_account_cache(
    conn: &Connection,
    live_keys: &HashSet<String>,
    restored_keys: &HashSet<String>,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    if let Some(item) = value.map(str::trim).filter(|item| !item.is_empty()) {
        upsert_vscdb_item(conn, key, item)?;
        return Ok(());
    }
    if !restored_keys.contains(key) && live_keys.contains(key) {
        delete_vscdb_item(conn, key)?;
    }
    Ok(())
}

fn persist_live_identity_snapshot(account_id: &str, conn: &Connection) {
    let rows = match read_identity_vscdb_rows(conn) {
        Ok(rows) => rows,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 回写登录快照失败: id={}, error={}",
                account_id, err
            ));
            return;
        }
    };
    if rows.is_empty() {
        return;
    }

    let Some(mut account) = load_account(account_id) else {
        return;
    };
    cursor_auth_raw_object_mut(&mut account)
        .insert(CURSOR_AUTH_VSCDB_RAW_KEY.to_string(), Value::Object(rows));
    if let Err(err) = upsert_account_record(account) {
        logger::log_warn(&format!(
            "[Cursor Account] 保存登录快照失败: id={}, error={}",
            account_id, err
        ));
    }
}

fn inject_account_and_refresh_snapshot(
    conn: &Connection,
    account: &CursorAccount,
) -> Result<(), String> {
    inject_account_into_conn(conn, account)?;
    persist_live_identity_snapshot(&account.id, conn);
    Ok(())
}

pub fn inject_to_cursor(account_id: &str) -> Result<(), String> {
    let account =
        load_account(account_id).ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    let db_path = get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Err(format!("Cursor state.vscdb 不存在: {}", db_path.display()));
    }

    let conn =
        Connection::open(&db_path).map_err(|e| format!("打开 Cursor 本地数据库失败: {}", e))?;
    inject_account_and_refresh_snapshot(&conn, &account)?;
    sync_cursor_extra_auth_stores(&account);

    logger::log_info(&format!(
        "[Cursor Account] 注入成功: id={}, email={}",
        account.id, account.email
    ));
    Ok(())
}

pub fn inject_to_cursor_at_path(db_path: &std::path::Path, account_id: &str) -> Result<(), String> {
    let account =
        load_account(account_id).ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    if !db_path.exists() {
        return Err(format!("Cursor state.vscdb 不存在: {}", db_path.display()));
    }

    let conn =
        Connection::open(db_path).map_err(|e| format!("打开 Cursor 本地数据库失败: {}", e))?;
    inject_account_and_refresh_snapshot(&conn, &account)?;
    if get_default_cursor_state_db_path()
        .ok()
        .is_some_and(|default_path| default_path == db_path)
    {
        sync_cursor_extra_auth_stores(&account);
    }

    logger::log_info(&format!(
        "[Cursor Account] 注入成功(自定义路径): id={}, email={}, path={}",
        account.id,
        account.email,
        db_path.display()
    ));
    Ok(())
}

fn sync_cursor_extra_auth_stores(account: &CursorAccount) {
    if let Err(err) = sync_cursor_secret_store_tokens(account) {
        logger::log_warn(&format!(
            "[Cursor Account] 探测/更新系统凭据失败: id={}, error={}",
            account.id, err
        ));
    }
    if let Err(err) = sync_cursor_cli_token_files(account) {
        logger::log_warn(&format!(
            "[Cursor Account] 探测/更新 Cursor CLI 凭据文件失败: id={}, error={}",
            account.id, err
        ));
    }
}

fn cursor_cli_home_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cursor"))
}

fn refresh_token_for_extra_stores(account: &CursorAccount) -> &str {
    account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(account.access_token.as_str())
}

fn sync_cursor_secret_store_tokens(account: &CursorAccount) -> Result<(), String> {
    let refresh_token = refresh_token_for_extra_stores(account);
    update_existing_cursor_secret(CURSOR_KEYCHAIN_ACCESS_SERVICE, &account.access_token)?;
    update_existing_cursor_secret(CURSOR_KEYCHAIN_REFRESH_SERVICE, refresh_token)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn cursor_secret_item_exists(service: &str) -> Result<bool, String> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            service,
            "-a",
            CURSOR_KEYCHAIN_ACCOUNT,
        ])
        .output()
        .map_err(|e| format!("调用 Keychain 失败: {}", e))?;
    Ok(output.status.success())
}

#[cfg(target_os = "macos")]
fn update_existing_cursor_secret(service: &str, secret: &str) -> Result<(), String> {
    if !cursor_secret_item_exists(service)? {
        return Ok(());
    }
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            CURSOR_KEYCHAIN_ACCOUNT,
            "-s",
            service,
            "-w",
            secret,
        ])
        .output()
        .map_err(|e| format!("更新 Keychain {} 失败: {}", service, e))?;
    if !output.status.success() {
        return Err(format!(
            "更新 Keychain {} 失败: {}",
            service,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    logger::log_info(&format!(
        "[Cursor Account] 已更新已有 Keychain 条目: service={}",
        service
    ));
    Ok(())
}

#[cfg(target_os = "windows")]
fn cursor_secret_item_exists(service: &str) -> Result<bool, String> {
    let output = Command::new("cmdkey")
        .args(["/list"])
        .output()
        .map_err(|e| format!("调用 Credential Manager 失败: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "列出 Credential Manager 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listing = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(listing.contains(service))
}

#[cfg(target_os = "windows")]
fn update_existing_cursor_secret(service: &str, secret: &str) -> Result<(), String> {
    if !cursor_secret_item_exists(service)? {
        return Ok(());
    }
    let generic = format!("/generic:{}", service);
    let user = format!("/user:{}", CURSOR_KEYCHAIN_ACCOUNT);
    let pass = format!("/pass:{}", secret);
    let output = Command::new("cmdkey")
        .args([generic.as_str(), user.as_str(), pass.as_str()])
        .output()
        .map_err(|e| format!("更新 Credential Manager {} 失败: {}", service, e))?;
    if !output.status.success() {
        return Err(format!(
            "更新 Credential Manager {} 失败: {}",
            service,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    logger::log_info(&format!(
        "[Cursor Account] 已更新已有 Credential Manager 条目: service={}",
        service
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
fn cursor_secret_item_exists(service: &str) -> Result<bool, String> {
    let output = Command::new("secret-tool")
        .args([
            "lookup",
            "service",
            service,
            "account",
            CURSOR_KEYCHAIN_ACCOUNT,
        ])
        .output()
        .map_err(|e| format!("调用 secret-tool 失败: {}", e))?;
    Ok(output.status.success())
}

#[cfg(target_os = "linux")]
fn update_existing_cursor_secret(service: &str, secret: &str) -> Result<(), String> {
    if !cursor_secret_item_exists(service)? {
        return Ok(());
    }
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            "--label",
            service,
            "service",
            service,
            "account",
            CURSOR_KEYCHAIN_ACCOUNT,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 secret-tool 失败: {}", e))?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin
            .write_all(secret.as_bytes())
            .map_err(|e| format!("写入 secret-tool 失败: {}", e))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("等待 secret-tool 失败: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "更新 secret-tool {} 失败: {}",
            service,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    logger::log_info(&format!(
        "[Cursor Account] 已更新已有 secret-tool 条目: service={}",
        service
    ));
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn update_existing_cursor_secret(_service: &str, _secret: &str) -> Result<(), String> {
    Ok(())
}

fn sync_cursor_cli_token_files(account: &CursorAccount) -> Result<(), String> {
    let Some(home) = cursor_cli_home_dir() else {
        return Ok(());
    };
    let refresh_token = refresh_token_for_extra_stores(account);
    update_existing_token_fields_in_file(
        &home.join("cli-config.json"),
        &account.access_token,
        refresh_token,
    )?;
    update_existing_token_fields_in_file(
        &home.join("auth.json"),
        &account.access_token,
        refresh_token,
    )?;
    Ok(())
}

fn looks_like_token_field(key: &str) -> Option<&'static str> {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    if normalized.contains("refresh") && normalized.contains("token") {
        return Some("refresh");
    }
    if normalized.contains("access") && normalized.contains("token") {
        return Some("access");
    }
    None
}

fn update_existing_token_fields(value: &mut Value, access_token: &str, refresh_token: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let Some(kind) = looks_like_token_field(&key) else {
                    if let Some(child) = map.get_mut(&key) {
                        changed |= update_existing_token_fields(child, access_token, refresh_token);
                    }
                    continue;
                };
                if !matches!(map.get(&key), Some(Value::String(_))) {
                    continue;
                }
                let next = if kind == "refresh" {
                    refresh_token
                } else {
                    access_token
                };
                map.insert(key, Value::String(next.to_string()));
                changed = true;
            }
            changed
        }
        Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            changed | update_existing_token_fields(item, access_token, refresh_token)
        }),
        _ => false,
    }
}

fn update_existing_token_fields_in_file(
    path: &Path,
    access_token: &str,
    refresh_token: &str,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
    let mut value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if !update_existing_token_fields(&mut value, access_token, refresh_token) {
        return Ok(());
    }
    let serialized = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("序列化 {} 失败: {}", path.display(), e))?;
    crate::modules::atomic_write::write_string_atomic(path, &serialized)
        .map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    logger::log_info(&format!(
        "[Cursor Account] 已更新已有 CLI 凭据字段: path={}",
        path.display()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor usage API
// ---------------------------------------------------------------------------

const CURSOR_USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const CURSOR_SAND_USAGE_RPC_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus";
const CURSOR_SAND_USAGE_DASHBOARD_URL: &str =
    "https://cursor.com/api/dashboard/get-sand-usage-status";
const CURSOR_GET_USER_META_URL: &str = "https://api2.cursor.sh/aiserver.v1.AuthService/GetUserMeta";
const CURSOR_FULL_STRIPE_PROFILE_URL: &str = "https://api2.cursor.sh/auth/full_stripe_profile";
const CURSOR_STRIPE_PROFILE_URL: &str = "https://api2.cursor.sh/auth/stripe_profile";
// 与官方 Cursor 客户端保持一致：使用 api2.cursor.sh/oauth/token 和内置 client_id 交换新 token。
const CURSOR_OAUTH_TOKEN_URL: &str = "https://api2.cursor.sh/oauth/token";
const CURSOR_AUTH_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUserMetaResponse {
    email: Option<String>,
    sign_up_type: Option<String>,
    workos_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorStripeProfileResponse {
    membership_type: Option<String>,
    individual_membership_type: Option<String>,
    subscription_status: Option<String>,
    team_membership_type: Option<String>,
    is_team_member: Option<bool>,
    is_enterprise: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct CursorRefreshTokenResponse {
    #[serde(alias = "accessToken")]
    access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, alias = "shouldLogout")]
    should_logout: bool,
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        let detail = err.to_string();
        if !detail.trim().is_empty() && parts.last().map(|item| item != &detail).unwrap_or(true) {
            parts.push(detail);
        }
        source = err.source();
    }
    parts.join(" | caused by: ")
}

fn format_reqwest_error(error: &reqwest::Error) -> String {
    let mut tags = Vec::new();
    if error.is_timeout() {
        tags.push("timeout");
    }
    if error.is_connect() {
        tags.push("connect");
    }
    if error.is_request() {
        tags.push("request");
    }

    let detail = format_error_chain(error);
    if tags.is_empty() {
        detail
    } else {
        format!("{} [{}]", detail, tags.join(","))
    }
}

fn build_cursor_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", format_reqwest_error(&e)))
}

fn extract_workos_user_id(jwt: &str) -> Option<String> {
    let value = decode_access_token_payload(jwt)?;
    let sub = value.get("sub")?.as_str()?;
    let user_id = sub.rsplit('|').next().unwrap_or(sub);
    if user_id.starts_with("user_") {
        Some(user_id.to_string())
    } else {
        None
    }
}

fn build_session_cookie(access_token: &str) -> Option<String> {
    let user_id = extract_workos_user_id(access_token)?;
    Some(format!(
        "WorkosCursorSessionToken={}%3A%3A{}",
        user_id, access_token
    ))
}

fn session_cookie_value(access_token: &str) -> Option<String> {
    let cookie = build_session_cookie(access_token)?;
    cookie
        .strip_prefix("WorkosCursorSessionToken=")
        .map(str::to_string)
}

const CURSOR_DASHBOARD_URL: &str = "https://cursor.com/dashboard";
const CHROME_CDP_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const CHROME_CDP_POLL_INTERVAL: Duration = Duration::from_millis(200);

fn reserve_chrome_cdp_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("分配 Chrome CDP 端口失败: {}", e))?
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("读取 Chrome CDP 端口失败: {}", e))
}

fn find_google_chrome_executable() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let path =
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        if path.exists() {
            return Ok(path);
        }
        return Err("未找到 Google Chrome，请先安装 Google Chrome".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        if let Ok(program_files) = std::env::var("PROGRAMFILES") {
            candidates.push(
                PathBuf::from(program_files)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
        }
        if let Ok(program_files_x86) = std::env::var("PROGRAMFILES(X86)") {
            candidates.push(
                PathBuf::from(program_files_x86)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe"),
            );
        }
        for path in candidates {
            if path.exists() {
                return Ok(path);
            }
        }
        return Err("未找到 Google Chrome，请先安装 Google Chrome".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        for name in ["google-chrome", "google-chrome-stable"] {
            if let Ok(output) = Command::new("which").arg(name).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() {
                        let candidate = PathBuf::from(&path);
                        if candidate.exists() {
                            return Ok(candidate);
                        }
                    }
                }
            }
        }
        return Err("未找到 Google Chrome，请先安装 Google Chrome".to_string());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err("当前平台不支持打开 Google Chrome".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ChromeCdpTarget {
    #[serde(rename = "type")]
    target_type: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: Option<String>,
}

async fn wait_for_chrome_page_target(port: u16) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("创建 CDP HTTP 客户端失败: {}", e))?;
    let deadline = tokio::time::Instant::now() + CHROME_CDP_CONNECT_TIMEOUT;

    while tokio::time::Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{}/json/list", port))
            .send()
            .await
        {
            if response.status().is_success() {
                if let Ok(targets) = response.json::<Vec<ChromeCdpTarget>>().await {
                    if let Some(url) = targets.into_iter().find_map(|target| {
                        if target.target_type == "page" {
                            target.websocket_url
                        } else {
                            None
                        }
                    }) {
                        return Ok(url);
                    }
                }
            }
        }
        tokio::time::sleep(CHROME_CDP_POLL_INTERVAL).await;
    }

    Err("连接 Chrome CDP 超时，请确认 Chrome 已启动".to_string())
}

async fn cdp_send_and_wait(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let payload = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("发送 CDP 命令失败 ({}): {}", method, e))?;

    let wait = timeout(CHROME_CDP_CONNECT_TIMEOUT, async {
        while let Some(message) = socket.next().await {
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("CDP {} 失败: {}", method, error));
                }
                return Ok(value);
            }
        }
        Err(format!("CDP {} 无响应", method))
    })
    .await
    .map_err(|_| format!("等待 CDP {} 响应超时", method))?;

    wait
}

async fn chrome_set_cookie_and_open_dashboard(
    websocket_url: &str,
    cookie_value: &str,
) -> Result<(), String> {
    let (mut socket, _) = timeout(CHROME_CDP_CONNECT_TIMEOUT, connect_async(websocket_url))
        .await
        .map_err(|_| "连接 Chrome WebSocket 超时".to_string())?
        .map_err(|e| format!("连接 Chrome WebSocket 失败: {}", e))?;

    let set_cookie_result = cdp_send_and_wait(
        &mut socket,
        1,
        "Network.setCookie",
        json!({
            "name": "WorkosCursorSessionToken",
            "value": cookie_value,
            "url": "https://cursor.com",
            "domain": ".cursor.com",
            "path": "/",
            "secure": true,
            "httpOnly": true,
            "sameSite": "Lax",
        }),
    )
    .await?;

    let success = set_cookie_result
        .pointer("/result/success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !success {
        return Err("写入 WorkosCursorSessionToken Cookie 失败".to_string());
    }

    cdp_send_and_wait(
        &mut socket,
        2,
        "Page.navigate",
        json!({ "url": CURSOR_DASHBOARD_URL }),
    )
    .await?;

    Ok(())
}

fn launch_chrome_incognito_with_cdp(
    chrome: &Path,
    user_data_dir: &Path,
    port: u16,
) -> Result<std::process::Child, String> {
    let mut command = Command::new(chrome);
    command
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--incognito")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-default-apps")
        .arg("--remote-debugging-address=127.0.0.1")
        .arg(format!("--remote-debugging-port={}", port))
        .arg("about:blank");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    command
        .spawn()
        .map_err(|e| format!("启动 Google Chrome 失败: {}", e))
}

/// 用账号 Session Token 打开 Chrome 无痕窗口并登录 Cursor Dashboard。
pub async fn open_account_in_chrome(account_id: &str) -> Result<(), String> {
    let account =
        load_account(account_id).ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    if account.access_token.trim().is_empty() {
        return Err("账号缺少 access token，无法打开 Chrome 登录".to_string());
    }
    let cookie_value = session_cookie_value(&account.access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;

    let chrome = find_google_chrome_executable()?;
    let port = reserve_chrome_cdp_port()?;
    let user_data_dir = std::env::temp_dir().join(format!(
        "ai-tools-cursor-chrome-{}-{}",
        account.id,
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&user_data_dir)
        .map_err(|e| format!("创建 Chrome 临时配置目录失败: {}", e))?;

    logger::log_info(&format!(
        "[Cursor Chrome] 启动无痕窗口: account_id={}, email={}, port={}, chrome={}",
        account.id,
        account.email,
        port,
        chrome.display()
    ));

    let mut child = launch_chrome_incognito_with_cdp(&chrome, &user_data_dir, port)?;
    // Give Chrome a moment to fail fast if launch args are rejected.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Ok(Some(status)) = child.try_wait() {
        let _ = fs::remove_dir_all(&user_data_dir);
        return Err(format!(
            "Google Chrome 启动后立即退出: {}",
            status
        ));
    }

    let websocket_url = match wait_for_chrome_page_target(port).await {
        Ok(url) => url,
        Err(err) => {
            let _ = child.kill();
            let _ = fs::remove_dir_all(&user_data_dir);
            return Err(err);
        }
    };

    if let Err(err) = chrome_set_cookie_and_open_dashboard(&websocket_url, &cookie_value).await {
        let _ = child.kill();
        let _ = fs::remove_dir_all(&user_data_dir);
        return Err(err);
    }

    // Detach: leave Chrome running; temp profile is cleaned on next OS temp cleanup.
    // Do not remove user_data_dir while Chrome still uses it.
    std::mem::forget(child);

    logger::log_info(&format!(
        "[Cursor Chrome] 无痕登录已打开: account_id={}, email={}",
        account.id, account.email
    ));
    Ok(())
}

fn resolve_membership_from_stripe_profile(profile: &CursorStripeProfileResponse) -> Option<String> {
    let membership = normalize_non_empty(profile.membership_type.as_deref());
    let individual = normalize_non_empty(profile.individual_membership_type.as_deref());

    if let Some(individual_value) = individual.as_ref() {
        if !individual_value.eq_ignore_ascii_case("free")
            && !matches!(
                membership.as_deref(),
                Some(value) if value.eq_ignore_ascii_case("enterprise")
            )
        {
            return Some(individual_value.clone());
        }
    }

    membership.or(individual)
}

async fn exchange_refresh_token_with_client(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CursorRefreshTokenResponse, String> {
    let response = client
        .post(CURSOR_OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CURSOR_AUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor token 刷新接口失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| {
        format!(
            "读取 Cursor token 刷新响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    if status == 401 || status == 403 {
        return Err("Cursor refresh token 已过期或无效，请重新导入账号".to_string());
    }
    if status != 200 {
        let detail = body.trim();
        return Err(if detail.is_empty() {
            format!("Cursor token 刷新接口返回异常状态码: {}", status)
        } else {
            format!(
                "Cursor token 刷新接口返回异常状态码: {}, body_len={}",
                status,
                body.len()
            )
        });
    }

    serde_json::from_str::<CursorRefreshTokenResponse>(&body)
        .map_err(|e| format!("解析 Cursor token 刷新响应失败: {}", e))
}

async fn refresh_account_access_token_with_client(
    client: &reqwest::Client,
    account: &mut CursorAccount,
) -> Result<bool, String> {
    let Some(refresh_token) = normalize_non_empty(account.refresh_token.as_deref()) else {
        return Ok(false);
    };

    let response = exchange_refresh_token_with_client(client, refresh_token.as_str()).await?;
    if response.should_logout {
        return Err("Cursor refresh token 已失效，请重新导入账号".to_string());
    }

    let new_access_token = normalize_non_empty(response.access_token.as_deref())
        .ok_or_else(|| "Cursor token 刷新响应缺少 access_token".to_string())?;
    let new_refresh_token =
        normalize_non_empty(response.refresh_token.as_deref()).or(Some(refresh_token));

    account.access_token = new_access_token.clone();
    account.refresh_token = new_refresh_token.clone();
    upsert_cursor_auth_raw_string(account, "accessToken", Some(new_access_token));
    upsert_cursor_auth_raw_string(account, "refreshToken", new_refresh_token);
    Ok(true)
}

async fn fetch_user_meta_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<CursorUserMetaResponse, String> {
    let response = client
        .post(CURSOR_GET_USER_META_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 Cursor user meta 失败: {}", format_reqwest_error(&e)))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }
    if status != 200 {
        return Err(format!("Cursor user meta API 返回异常状态码: {}", status));
    }

    let body = response.text().await.map_err(|e| {
        format!(
            "读取 Cursor user meta 响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    serde_json::from_str::<CursorUserMetaResponse>(&body)
        .map_err(|e| format!("解析 Cursor user meta JSON 失败: {}", e))
}

async fn fetch_stripe_profile_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Option<CursorStripeProfileResponse>, String> {
    let full_response = client
        .get(CURSOR_FULL_STRIPE_PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor full stripe profile 失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let full_status = full_response.status().as_u16();
    if full_status == 401 || full_status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }
    if full_status == 200 {
        let body = full_response.text().await.map_err(|e| {
            format!(
                "读取 Cursor full stripe profile 响应失败: {}",
                format_reqwest_error(&e)
            )
        })?;
        let profile = serde_json::from_str::<CursorStripeProfileResponse>(&body)
            .map_err(|e| format!("解析 Cursor full stripe profile JSON 失败: {}", e))?;
        return Ok(Some(profile));
    }

    let fallback_response = client
        .get(CURSOR_STRIPE_PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor stripe profile 失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let fallback_status = fallback_response.status().as_u16();
    if fallback_status == 401 || fallback_status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }
    if fallback_status != 200 {
        return Ok(None);
    }

    let body = fallback_response.text().await.map_err(|e| {
        format!(
            "读取 Cursor stripe profile 响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    let parsed = serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("解析 Cursor stripe profile JSON 失败: {}", e))?;

    match parsed {
        Value::Object(_) => serde_json::from_value::<CursorStripeProfileResponse>(parsed)
            .map(Some)
            .map_err(|e| format!("解析 Cursor stripe profile 对象失败: {}", e)),
        Value::String(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(CursorStripeProfileResponse {
                    membership_type: Some("pro".to_string()),
                    individual_membership_type: None,
                    subscription_status: None,
                    team_membership_type: None,
                    is_team_member: None,
                    is_enterprise: None,
                }))
            }
        }
        _ => Ok(None),
    }
}

async fn fetch_usage_summary_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<serde_json::Value, String> {
    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;

    let response = client
        .get(CURSOR_USAGE_SUMMARY_URL)
        .header("Accept", "application/json")
        .header("Cookie", &cookie)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        )
        .send()
        .await
        .map_err(|e| format!("请求 Cursor usage API 失败: {}", format_reqwest_error(&e)))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }
    if status != 200 {
        return Err(format!("Cursor usage API 返回异常状态码: {}", status));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor usage 响应失败: {}", format_reqwest_error(&e)))?;

    serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("解析 Cursor usage JSON 失败: {}", e))
}

fn sand_usage_looks_valid(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    [
        "usagePercent",
        "usage_percent",
        "usedPercent",
        "used_percent",
        "hasNonZeroIncludedLimit",
        "has_non_zero_included_limit",
        "nextResetTimestampUtc",
        "next_reset_timestamp_utc",
    ]
    .iter()
    .any(|key| obj.contains_key(*key))
}

fn parse_json_object_body(body: &str, context: &str) -> Result<Value, String> {
    let value = serde_json::from_str::<Value>(body)
        .map_err(|e| format!("解析 {} JSON 失败: {}", context, e))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(format!("{} 响应不是对象", context))
    }
}

async fn fetch_sand_usage_status_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    let rpc_response = client
        .post(CURSOR_SAND_USAGE_RPC_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor Grok Bot 用量接口失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let rpc_status = rpc_response.status().as_u16();
    if rpc_status == 200 {
        let body = rpc_response.text().await.map_err(|e| {
            format!(
                "读取 Cursor Grok Bot 用量响应失败: {}",
                format_reqwest_error(&e)
            )
        })?;
        if let Ok(value) = parse_json_object_body(&body, "Cursor Grok Bot 用量") {
            if sand_usage_looks_valid(&value) {
                return Ok(value);
            }
        }
    } else if rpc_status == 401 || rpc_status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }

    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;
    let dashboard_response = client
        .post(CURSOR_SAND_USAGE_DASHBOARD_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Cookie", &cookie)
        .header("Origin", "https://cursor.com")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor Grok Bot dashboard 用量失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let dashboard_status = dashboard_response.status().as_u16();
    if dashboard_status == 401 || dashboard_status == 403 {
        return Err("Cursor 会话已过期或未认证，请重新导入账号".to_string());
    }
    if dashboard_status != 200 {
        return Err(format!(
            "Cursor Grok Bot 用量接口返回异常状态码: rpc={}, dashboard={}",
            rpc_status, dashboard_status
        ));
    }

    let body = dashboard_response.text().await.map_err(|e| {
        format!(
            "读取 Cursor Grok Bot dashboard 用量响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;
    parse_json_object_body(&body, "Cursor Grok Bot dashboard 用量")
}

// ---------------------------------------------------------------------------
// Refresh (updates our own account storage + fetches usage from official APIs)
// ---------------------------------------------------------------------------

async fn refresh_account_async_once(account_id: &str) -> Result<CursorAccount, String> {
    let existing = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    logger::log_info(&format!(
        "[Cursor Refresh] 开始刷新账号: id={}, email={}",
        existing.id, existing.email
    ));

    let client = build_cursor_http_client()?;
    let mut account = existing.clone();

    if access_token_needs_refresh(&account.access_token) {
        match refresh_account_access_token_with_client(&client, &mut account).await {
            Ok(true) => {
                logger::log_info(&format!(
                    "[Cursor Refresh] access token 刷新成功: id={}",
                    account.id
                ));
            }
            Ok(false) => {}
            Err(err) => {
                logger::log_warn(&format!(
                    "[Cursor Refresh] access token 刷新失败，继续使用现有 token: id={}, error={}",
                    account.id, err
                ));
            }
        }
    }

    match fetch_user_meta_with_client(&client, &account.access_token).await {
        Ok(meta) => {
            if let Some(email) = normalize_email_identity(meta.email.as_deref()) {
                account.email = email.clone();
                upsert_cursor_auth_raw_string(&mut account, "cachedEmail", Some(email));
            }

            if let Some(sign_up_type) = normalize_cursor_sign_up_type(meta.sign_up_type.as_deref())
            {
                account.sign_up_type = Some(sign_up_type.clone());
                upsert_cursor_auth_raw_string(&mut account, "cachedSignUpType", Some(sign_up_type));
            }

            upsert_cursor_auth_raw_string(&mut account, "workosId", meta.workos_id.clone());
            if account.auth_id.is_none() {
                account.auth_id = normalize_auth_identity(meta.workos_id.as_deref());
            } else if let Some(canonical) = normalize_auth_identity(account.auth_id.as_deref()) {
                account.auth_id = Some(canonical);
            }

            logger::log_info(&format!(
                "[Cursor Refresh] 用户信息拉取成功: id={}, email={}",
                account.id, account.email
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] 用户信息拉取失败: id={}, error={}",
                account.id, err
            ));
        }
    }

    match fetch_stripe_profile_with_client(&client, &account.access_token).await {
        Ok(Some(profile)) => {
            if let Some(membership_type) = resolve_membership_from_stripe_profile(&profile) {
                account.membership_type = Some(membership_type.clone());
                upsert_cursor_auth_raw_string(
                    &mut account,
                    "stripeMembershipType",
                    Some(membership_type),
                );
            }

            let subscription_status = normalize_non_empty(profile.subscription_status.as_deref());
            if let Some(status) = subscription_status.clone() {
                account.subscription_status = Some(status);
            }
            upsert_cursor_auth_raw_string(
                &mut account,
                "stripeSubscriptionStatus",
                subscription_status,
            );
            upsert_cursor_auth_raw_string(
                &mut account,
                "teamMembershipType",
                normalize_non_empty(profile.team_membership_type.as_deref()),
            );
            upsert_cursor_auth_raw_bool(&mut account, "isTeamMember", profile.is_team_member);
            upsert_cursor_auth_raw_bool(&mut account, "isEnterprise", profile.is_enterprise);

            logger::log_info(&format!(
                "[Cursor Refresh] 订阅信息拉取成功: id={}",
                account.id
            ));
        }
        Ok(None) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] 未获取到订阅信息: id={}",
                account.id
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] 订阅信息拉取失败: id={}, error={}",
                account.id, err
            ));
        }
    }

    let mut usage_refreshed = false;
    match fetch_usage_summary_with_client(&client, &account.access_token).await {
        Ok(mut usage) => {
            if let Some(mt) = usage.get("membershipType").and_then(|v| v.as_str()) {
                if !mt.is_empty() {
                    account.membership_type = Some(mt.to_string());
                }
            }
            match fetch_sand_usage_status_with_client(&client, &account.access_token).await {
                Ok(sand) => {
                    if let Some(obj) = usage.as_object_mut() {
                        obj.insert("grokBot".to_string(), sand);
                    }
                    logger::log_info(&format!(
                        "[Cursor Refresh] Grok Bot 用量拉取成功: id={}",
                        account.id
                    ));
                }
                Err(err) => {
                    logger::log_warn(&format!(
                        "[Cursor Refresh] Grok Bot 用量拉取失败: id={}, error={}",
                        account.id, err
                    ));
                }
            }
            account.cursor_usage_raw = Some(usage);
            account.quota_query_last_error = None;
            account.quota_query_last_error_at = None;
            usage_refreshed = true;
            logger::log_info(&format!(
                "[Cursor Refresh] API 配额拉取成功: id={}",
                account.id
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] API 配额拉取失败: id={}, error={}",
                account.id, err
            ));
            account.quota_query_last_error = Some(err);
            account.quota_query_last_error_at = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    let refreshed_at = now_ts();
    if usage_refreshed {
        account.usage_updated_at = Some(refreshed_at);
    }
    account.last_used = refreshed_at;
    let updated = account.clone();
    upsert_account_record(account)?;
    logger::log_info(&format!(
        "[Cursor Refresh] 刷新完成: id={}, email={}",
        updated.id, updated.email
    ));
    Ok(updated)
}

pub async fn refresh_account_async(account_id: &str) -> Result<CursorAccount, String> {
    let result = refresh_account_async_once(account_id).await;
    if let Err(err) = &result {
        persist_quota_query_error(account_id, err);
    }
    result
}

pub async fn refresh_all_tokens() -> Result<Vec<(String, Result<CursorAccount, String>)>, String> {
    use futures::future::join_all;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    const MAX_CONCURRENT: usize = 5;
    let accounts = list_accounts();
    let total = accounts.len();
    let active_accounts: Vec<CursorAccount> = accounts
        .into_iter()
        .filter(|account| !is_banned_account(account))
        .collect();
    let skipped_banned = total.saturating_sub(active_accounts.len());
    if skipped_banned > 0 {
        logger::log_info(&format!(
            "[Cursor Refresh] 跳过封禁账号: skipped={}, total={}",
            skipped_banned, total
        ));
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let tasks: Vec<_> = active_accounts
        .into_iter()
        .map(|account| {
            let id = account.id;
            let semaphore = semaphore.clone();
            async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|e| format!("获取 Cursor 刷新并发许可失败: {}", e))?;
                let result = refresh_account_async(&id).await;
                Ok::<(String, Result<CursorAccount, String>), String>((id, result))
            }
        })
        .collect();

    let mut results = Vec::with_capacity(tasks.len());
    for task in join_all(tasks).await {
        match task {
            Ok(item) => results.push(item),
            Err(err) => return Err(err),
        }
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Quota alert
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct CursorUsagePercent {
    total_used: Option<i32>,
    auto_used: Option<i32>,
    api_used: Option<i32>,
    bot_used: Option<i32>,
}

fn clamp_percent(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    if value <= 0.0 {
        return 0;
    }
    if value >= 100.0 {
        return 100;
    }
    value.round() as i32
}

fn grok_bot_value(raw_obj: &serde_json::Map<String, Value>) -> Option<&Value> {
    raw_obj
        .get("grokBot")
        .or_else(|| raw_obj.get("grok_bot"))
        .or_else(|| raw_obj.get("sandUsage"))
        .or_else(|| raw_obj.get("sand_usage"))
        .or_else(|| {
            raw_obj
                .get("individualUsage")
                .and_then(|value| value.as_object())
                .and_then(|value| value.get("bot"))
        })
        .or_else(|| {
            raw_obj
                .get("individual_usage")
                .and_then(|value| value.as_object())
                .and_then(|value| value.get("bot"))
        })
}

fn grok_bot_has_personal_allowance(bot: Option<&Value>) -> bool {
    let Some(obj) = bot.and_then(|value| value.as_object()) else {
        return true;
    };
    let flag = |keys: &[&str]| -> Option<bool> {
        for key in keys {
            match obj.get(*key) {
                Some(Value::Bool(value)) => return Some(*value),
                Some(Value::String(text)) => {
                    let normalized = text.trim().to_ascii_lowercase();
                    if normalized == "true" {
                        return Some(true);
                    }
                    if normalized == "false" {
                        return Some(false);
                    }
                }
                _ => {}
            }
        }
        None
    };
    if flag(&["usesPooledEnterpriseAllowance", "uses_pooled_enterprise_allowance"]) == Some(true) {
        return false;
    }
    if flag(&["includedLimitZero", "included_limit_zero"]) == Some(true) {
        return false;
    }
    if flag(&["hasNonZeroIncludedLimit", "has_non_zero_included_limit"]) == Some(false) {
        return false;
    }
    true
}

fn pick_grok_bot_percent(raw_obj: &serde_json::Map<String, Value>) -> Option<f64> {
    let bot = grok_bot_value(raw_obj);
    if !grok_bot_has_personal_allowance(bot) {
        return None;
    }
    pick_number(
        bot,
        &[
            "usagePercent",
            "usage_percent",
            "usedPercent",
            "used_percent",
            "botPercentUsed",
            "bot_percent_used",
        ],
    )
}

fn pick_number(value: Option<&Value>, keys: &[&str]) -> Option<f64> {
    let obj = value?.as_object()?;
    for key in keys {
        let Some(raw) = obj.get(*key) else {
            continue;
        };
        if let Some(n) = raw.as_f64() {
            if n.is_finite() {
                return Some(n);
            }
            continue;
        }
        if let Some(text) = raw.as_str() {
            if let Ok(parsed) = text.trim().parse::<f64>() {
                if parsed.is_finite() {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn read_usage_percent(account: &CursorAccount) -> CursorUsagePercent {
    let Some(raw) = account.cursor_usage_raw.as_ref() else {
        return CursorUsagePercent::default();
    };

    let raw_obj = match raw.as_object() {
        Some(value) => value,
        None => return CursorUsagePercent::default(),
    };

    let plan_value = raw_obj
        .get("individualUsage")
        .and_then(|value| value.as_object())
        .and_then(|value| value.get("plan"))
        .or_else(|| {
            raw_obj
                .get("individual_usage")
                .and_then(|value| value.as_object())
                .and_then(|value| value.get("plan"))
        })
        .or_else(|| raw_obj.get("planUsage"))
        .or_else(|| raw_obj.get("plan_usage"));

    let total_direct = pick_number(plan_value, &["totalPercentUsed", "total_percent_used"]);
    let auto_direct = pick_number(plan_value, &["autoPercentUsed", "auto_percent_used"]);
    let api_direct = pick_number(plan_value, &["apiPercentUsed", "api_percent_used"]);
    let bot_direct = pick_grok_bot_percent(raw_obj)
        .or_else(|| pick_number(plan_value, &["botPercentUsed", "bot_percent_used"]));

    let used = pick_number(plan_value, &["used", "totalSpend", "total_spend"]);
    let limit = pick_number(plan_value, &["limit"]);
    let total_ratio = match (used, limit) {
        (Some(used_val), Some(limit_val)) if limit_val > 0.0 => {
            Some((used_val / limit_val) * 100.0)
        }
        _ => None,
    };

    CursorUsagePercent {
        total_used: total_direct.or(total_ratio).map(clamp_percent),
        auto_used: auto_direct.map(clamp_percent),
        api_used: api_direct.map(clamp_percent),
        bot_used: bot_direct.map(clamp_percent),
    }
}

pub(crate) fn extract_quota_metrics(account: &CursorAccount) -> Vec<(String, i32)> {
    let usage = read_usage_percent(account);
    let mut metrics = Vec::new();

    if let Some(used) = usage.total_used {
        metrics.push(("Total Usage".to_string(), 100 - used.clamp(0, 100)));
    }
    if let Some(used) = usage.auto_used {
        metrics.push(("Auto + Composer".to_string(), 100 - used.clamp(0, 100)));
    }
    if let Some(used) = usage.api_used {
        metrics.push(("API Usage".to_string(), 100 - used.clamp(0, 100)));
    }
    if let Some(used) = usage.bot_used {
        metrics.push(("Bot".to_string(), 100 - used.clamp(0, 100)));
    }

    metrics
}

fn average_quota_percentage(metrics: &[(String, i32)]) -> f64 {
    if metrics.is_empty() {
        return 0.0;
    }
    let sum: i32 = metrics.iter().map(|(_, pct)| *pct).sum();
    sum as f64 / metrics.len() as f64
}

fn normalize_quota_alert_threshold(value: i32) -> i32 {
    value.clamp(0, 100)
}

pub(crate) fn resolve_current_account_id(accounts: &[CursorAccount]) -> Option<String> {
    if let Ok(Some(local_payload)) = read_local_cursor_auth() {
        let incoming_auth_id = resolve_payload_auth_id(&local_payload);
        let incoming_email = normalize_email_identity(Some(local_payload.email.as_str()));
        let incoming_token = normalize_token_identity(Some(local_payload.access_token.as_str()));

        if let Some(account_id) = accounts
            .iter()
            .find(|account| {
                let existing_auth_id = resolve_account_auth_id(account);
                let existing_email = normalize_email_identity(Some(account.email.as_str()));
                let existing_token = normalize_token_identity(Some(account.access_token.as_str()));
                cursor_identities_match(
                    existing_auth_id.as_deref(),
                    incoming_auth_id.as_deref(),
                    existing_email.as_deref(),
                    incoming_email.as_deref(),
                    existing_token.as_deref(),
                    incoming_token.as_deref(),
                )
            })
            .map(|account| account.id.clone())
        {
            return Some(account_id);
        }
    }

    if let Ok(settings) = crate::modules::cursor_instance::load_default_settings() {
        if let Some(bind_id) = settings.bind_account_id {
            let trimmed = bind_id.trim();
            if !trimmed.is_empty() && accounts.iter().any(|account| account.id == trimmed) {
                return Some(trimmed.to_string());
            }
        }
    }

    crate::modules::provider_current_state::resolve_existing_current_account_id(
        "cursor",
        accounts.iter().map(|account| account.id.as_str()),
    )
}

fn pick_quota_alert_recommendation(
    accounts: &[CursorAccount],
    current_id: &str,
) -> Option<CursorAccount> {
    let mut candidates: Vec<CursorAccount> = accounts
        .iter()
        .filter(|account| account.id != current_id)
        .filter(|account| !is_banned_account(account))
        .filter(|account| !extract_quota_metrics(account).is_empty())
        .cloned()
        .collect();

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|a, b| {
        let avg_a = average_quota_percentage(&extract_quota_metrics(a));
        let avg_b = average_quota_percentage(&extract_quota_metrics(b));
        avg_b
            .partial_cmp(&avg_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.last_used.cmp(&b.last_used))
    });

    candidates.into_iter().next()
}

fn display_email(account: &CursorAccount) -> String {
    let trimmed = account.email.trim();
    if trimmed.is_empty() {
        account.id.clone()
    } else {
        trimmed.to_string()
    }
}

fn build_quota_alert_cooldown_key(account_id: &str, threshold: i32) -> String {
    format!("cursor:{}:{}", account_id, threshold)
}

fn should_emit_quota_alert(cooldown_key: &str, now: i64) -> bool {
    let Ok(mut state) = CURSOR_QUOTA_ALERT_LAST_SENT.lock() else {
        return true;
    };

    if let Some(last_sent) = state.get(cooldown_key) {
        if now - *last_sent < CURSOR_QUOTA_ALERT_COOLDOWN_SECONDS {
            return false;
        }
    }

    state.insert(cooldown_key.to_string(), now);
    true
}

fn clear_quota_alert_cooldown(account_id: &str, threshold: i32) {
    if let Ok(mut state) = CURSOR_QUOTA_ALERT_LAST_SENT.lock() {
        state.remove(&build_quota_alert_cooldown_key(account_id, threshold));
    }
}

pub fn run_quota_alert_if_needed(
) -> Result<Option<crate::modules::account::QuotaAlertPayload>, String> {
    let cfg = crate::modules::config::get_user_config();
    if !cfg.cursor_quota_alert_enabled {
        return Ok(None);
    }

    let threshold = normalize_quota_alert_threshold(cfg.cursor_quota_alert_threshold);
    let accounts = list_accounts();
    let current_id = match resolve_current_account_id(&accounts) {
        Some(id) => id,
        None => return Ok(None),
    };

    let current = match accounts.iter().find(|account| account.id == current_id) {
        Some(account) => account,
        None => return Ok(None),
    };
    if is_banned_account(current) {
        return Ok(None);
    }

    let metrics = extract_quota_metrics(current);
    if metrics.is_empty() {
        clear_quota_alert_cooldown(&current_id, threshold);
        return Ok(None);
    }

    let low_models: Vec<(String, i32)> = metrics
        .into_iter()
        .filter(|(_, pct)| *pct <= threshold)
        .collect();
    if low_models.is_empty() {
        clear_quota_alert_cooldown(&current_id, threshold);
        return Ok(None);
    }

    let now = chrono::Utc::now().timestamp();
    let cooldown_key = build_quota_alert_cooldown_key(&current_id, threshold);
    if !should_emit_quota_alert(&cooldown_key, now) {
        return Ok(None);
    }

    let recommendation = pick_quota_alert_recommendation(&accounts, &current_id);
    let lowest_percentage = low_models.iter().map(|(_, pct)| *pct).min().unwrap_or(0);
    let payload = crate::modules::account::QuotaAlertPayload {
        platform: "cursor".to_string(),
        current_account_id: current_id,
        current_email: display_email(current),
        threshold,
        threshold_display: None,
        lowest_percentage,
        low_models: low_models.into_iter().map(|(name, _)| name).collect(),
        recommended_account_id: recommendation.as_ref().map(|account| account.id.clone()),
        recommended_email: recommendation.as_ref().map(display_email),
        triggered_at: now,
    };

    crate::modules::account::dispatch_quota_alert(&payload);
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_jwt(sub: &str) -> String {
        // header.payload.signature — only payload is decoded by helpers
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            format!(r#"{{"sub":"{}","type":"session","aud":"https://cursor.com"}}"#, sub).as_bytes(),
        );
        format!("{}.{}.sig", header, payload)
    }

    #[test]
    fn parse_bare_jwt() {
        let jwt = sample_jwt("auth0|user_01ABCDEFGHIJKLMNOPQRSTUV");
        let payload = parse_cursor_token_line(&jwt).expect("parse jwt");
        assert_eq!(payload.email, "unknown");
        assert_eq!(
            payload.auth_id.as_deref(),
            Some("user_01ABCDEFGHIJKLMNOPQRSTUV")
        );
        assert_eq!(payload.access_token, jwt);
    }

    #[test]
    fn parse_email_dash_user_jwt() {
        let jwt = sample_jwt("auth0|user_01KN1CWBNADVQN2V0QMMHS2CX6");
        let line = format!(
            "demo@example.com----user_01KN1CWBNADVQN2V0QMMHS2CX6::{}",
            jwt
        );
        let payload = parse_cursor_token_line(&line).expect("parse line");
        assert_eq!(payload.email, "demo@example.com");
        assert_eq!(
            payload.auth_id.as_deref(),
            Some("user_01KN1CWBNADVQN2V0QMMHS2CX6")
        );
        assert_eq!(payload.access_token, jwt);
    }

    #[test]
    fn parse_user_jwt_and_url_encoded_cookie() {
        let jwt = sample_jwt("auth0|user_01TESTUSERID000000000001");
        let line = format!("user_01TESTUSERID000000000001::{}", jwt);
        let payload = parse_cursor_token_line(&line).expect("parse user::jwt");
        assert_eq!(payload.email, "unknown");
        assert_eq!(
            payload.auth_id.as_deref(),
            Some("user_01TESTUSERID000000000001")
        );

        let cookie = format!(
            "WorkosCursorSessionToken=user_01TESTUSERID000000000001%3A%3A{}",
            jwt
        );
        let payload2 = parse_cursor_token_line(&cookie).expect("parse cookie");
        assert_eq!(
            payload2.auth_id.as_deref(),
            Some("user_01TESTUSERID000000000001")
        );
        assert_eq!(payload2.access_token, jwt);
    }

    #[test]
    fn parse_batch_text_skips_bad_lines() {
        let jwt1 = sample_jwt("auth0|user_01AAA");
        let jwt2 = sample_jwt("auth0|user_01BBB");
        let text = format!(
            "a@example.com----user_01AAA::{}\nbad-line-without-token\nb@example.com----user_01BBB::{}\n",
            jwt1, jwt2
        );
        let payloads = parse_cursor_token_text(&text).expect("batch parse");
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0].email, "a@example.com");
        assert_eq!(payloads[1].email, "b@example.com");
    }

    #[test]
    fn format_token_line_round_trip_fields() {
        let jwt = sample_jwt("auth0|user_01ROUNDTRIP00000000000");
        let account = CursorAccount {
            id: "acc1".to_string(),
            email: "round@example.com".to_string(),
            auth_id: Some("user_01ROUNDTRIP00000000000".to_string()),
            name: None,
            tags: None,
            access_token: jwt.clone(),
            refresh_token: None,
            membership_type: None,
            subscription_status: None,
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
        let line = format_account_token_line(&account);
        assert_eq!(
            line,
            format!("round@example.com----user_01ROUNDTRIP00000000000::{}", jwt)
        );
        let parsed = parse_cursor_token_line(&line).expect("round-trip parse");
        assert_eq!(parsed.email, "round@example.com");
        assert_eq!(parsed.access_token, jwt);
        assert_eq!(
            parsed.auth_id.as_deref(),
            Some("user_01ROUNDTRIP00000000000")
        );
    }

    #[test]
    fn prefer_jwt_sub_when_auth_id_mismatches() {
        let jwt = sample_jwt("auth0|user_01REALUSERID00000000000");
        let line = format!("demo@example.com----user_01WRONGID0000000000000::{}", jwt);
        let payload = parse_cursor_token_line(&line).expect("parse mismatch");
        assert_eq!(
            payload.auth_id.as_deref(),
            Some("user_01REALUSERID00000000000")
        );
    }

    fn identity_account(email: &str, auth_id: Option<&str>, jwt: &str) -> CursorAccount {
        CursorAccount {
            id: "acc-temp".to_string(),
            email: email.to_string(),
            auth_id: auth_id.map(|value| value.to_string()),
            name: None,
            tags: None,
            access_token: jwt.to_string(),
            refresh_token: None,
            membership_type: None,
            subscription_status: None,
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
        }
    }

    fn sample_import_payload(email: &str, auth_id: Option<&str>, jwt: &str) -> CursorImportPayload {
        CursorImportPayload {
            email: email.to_string(),
            auth_id: auth_id.map(|value| value.to_string()),
            name: None,
            access_token: jwt.to_string(),
            refresh_token: None,
            membership_type: None,
            subscription_status: None,
            sign_up_type: None,
            cursor_auth_raw: None,
            cursor_usage_raw: None,
            status: None,
            status_reason: None,
        }
    }

    #[test]
    fn normalize_auth_identity_strips_provider_prefix() {
        assert_eq!(
            normalize_auth_identity(Some("grok|user_01M0G4D2NZEV8DE63HSZ8JK9TM")).as_deref(),
            Some("user_01M0G4D2NZEV8DE63HSZ8JK9TM")
        );
        assert_eq!(
            normalize_auth_identity(Some("auth0|user_01ABCDEFGHIJKLMNOPQRSTUV")).as_deref(),
            Some("user_01ABCDEFGHIJKLMNOPQRSTUV")
        );
        assert_eq!(
            normalize_auth_identity(Some("user_01PLAINUSERID000000000000")).as_deref(),
            Some("user_01PLAINUSERID000000000000")
        );
    }

    #[test]
    fn accounts_are_duplicates_treats_grok_prefix_as_same_user() {
        let jwt = sample_jwt("grok|user_01DUPTEST00000000000000");
        let prefixed = identity_account(
            "irving@example.com",
            Some("grok|user_01DUPTEST00000000000000"),
            &jwt,
        );
        let canonical = identity_account(
            "irving@example.com",
            Some("user_01DUPTEST00000000000000"),
            &jwt,
        );
        assert!(accounts_are_duplicates(&prefixed, &canonical));
    }

    #[test]
    fn accounts_are_duplicates_same_email_when_auth_ids_differ() {
        let left = identity_account(
            "same@example.com",
            Some("user_01LEFTACCOUNT000000000000"),
            &sample_jwt("auth0|user_01LEFTACCOUNT000000000000"),
        );
        let right = identity_account(
            "same@example.com",
            Some("user_01RIGHTACCOUNT00000000000"),
            &sample_jwt("auth0|user_01RIGHTACCOUNT00000000000"),
        );
        assert!(accounts_are_duplicates(&left, &right));
    }

    #[test]
    fn accounts_are_duplicates_rejects_different_email_and_auth() {
        let left = identity_account(
            "left@example.com",
            Some("user_01LEFTACCOUNT000000000000"),
            &sample_jwt("auth0|user_01LEFTACCOUNT000000000000"),
        );
        let right = identity_account(
            "right@example.com",
            Some("user_01RIGHTACCOUNT00000000000"),
            &sample_jwt("auth0|user_01RIGHTACCOUNT00000000000"),
        );
        assert!(!accounts_are_duplicates(&left, &right));
    }

    #[test]
    fn resolve_payload_auth_id_canonicalizes_grok_prefix() {
        let jwt = sample_jwt("grok|user_01LOCALPAYLOAD0000000000");
        let payload = sample_import_payload(
            "local@example.com",
            Some("grok|user_01LOCALPAYLOAD0000000000"),
            &jwt,
        );
        assert_eq!(
            resolve_payload_auth_id(&payload).as_deref(),
            Some("user_01LOCALPAYLOAD0000000000")
        );
    }

    #[test]
    fn payload_from_import_json_writes_canonical_auth_id() {
        let jwt = sample_jwt("grok|user_01JSONIMPORT00000000000");
        let raw = serde_json::json!({
            "email": "json@example.com",
            "access_token": jwt,
            "auth_id": "grok|user_01JSONIMPORT00000000000"
        });
        let payload = payload_from_import_value(raw).expect("parse json payload");
        assert_eq!(
            payload.auth_id.as_deref(),
            Some("user_01JSONIMPORT00000000000")
        );
    }

    #[test]
    fn upsert_reuses_id_when_auth_id_prefix_differs() {
        let _lock = crate::modules::test_support::env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        struct DataDirGuard {
            dir: PathBuf,
            previous: Option<String>,
        }
        impl Drop for DataDirGuard {
            fn drop(&mut self) {
                match self.previous.as_ref() {
                    Some(value) => std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", value),
                    None => std::env::remove_var("COCKPIT_TOOLS_TEST_DATA_DIR"),
                }
                let _ = fs::remove_dir_all(&self.dir);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "cursor-account-dedup-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        let _guard = DataDirGuard {
            previous: std::env::var("COCKPIT_TOOLS_TEST_DATA_DIR").ok(),
            dir: dir.clone(),
        };
        std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", &dir);

        let jwt = sample_jwt("grok|user_01UPSERTDEDUP0000000000");
        let first = upsert_account(sample_import_payload(
            "dup@example.com",
            Some("grok|user_01UPSERTDEDUP0000000000"),
            &jwt,
        ))
        .expect("first upsert");
        let second = upsert_account(sample_import_payload(
            "dup@example.com",
            Some("user_01UPSERTDEDUP0000000000"),
            &jwt,
        ))
        .expect("second upsert");

        assert_eq!(first.id, second.id);
        assert_eq!(
            first.auth_id.as_deref(),
            Some("user_01UPSERTDEDUP0000000000")
        );
        assert_eq!(
            second.auth_id.as_deref(),
            Some("user_01UPSERTDEDUP0000000000")
        );
        assert_eq!(list_accounts().len(), 1);
    }

    fn sample_account(jwt: &str) -> CursorAccount {
        CursorAccount {
            id: "acc1".to_string(),
            email: "next@example.com".to_string(),
            auth_id: Some("user_01NEXTACCOUNT00000000000".to_string()),
            name: None,
            tags: None,
            access_token: jwt.to_string(),
            refresh_token: None,
            membership_type: None,
            subscription_status: None,
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
        }
    }

    fn seed_item_table(conn: &Connection, rows: &[(&str, &str)]) {
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .expect("create table");
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                [*key, *value],
            )
            .expect("seed row");
        }
    }

    fn read_item(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    }

    #[test]
    fn cursor_identity_value_accepts_auth0_and_user_but_not_grok() {
        assert!(is_cursor_identity_value("auth0|user_01ABC"));
        assert!(is_cursor_identity_value("user_01ABC"));
        assert!(!is_cursor_identity_value("grok|user_01ABC"));
        assert!(!is_cursor_identity_value(""));
    }

    #[test]
    fn inject_keeps_unknown_keys_and_skips_missing_identity_keys() {
        let jwt = sample_jwt("auth0|user_01NEXTACCOUNT00000000000");
        let conn = Connection::open_in_memory().expect("memory db");
        seed_item_table(
            &conn,
            &[
                ("cursorAuth/accessToken", "old-token"),
                ("cursorAuth/cachedEmail", "old@example.com"),
                ("cursorAuth/onboardingDate", "2026-01-01T00:00:00.000Z"),
                ("cursorAuth/futureKey", "keep-me"),
                ("cursorAuth/cachedTeam", r#"{"name":"Old Team"}"#),
            ],
        );

        inject_account_into_conn(&conn, &sample_account(&jwt)).expect("inject");

        assert_eq!(read_item(&conn, "cursorAuth/accessToken").as_deref(), Some(jwt.as_str()));
        assert_eq!(
            read_item(&conn, "cursorAuth/cachedEmail").as_deref(),
            Some("next@example.com")
        );
        assert_eq!(
            read_item(&conn, "cursorAuth/onboardingDate").as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(
            read_item(&conn, "cursorAuth/futureKey").as_deref(),
            Some("keep-me")
        );
        assert!(read_item(&conn, "cursorAuth/cachedTeam").is_none());
        assert!(read_item(&conn, "cursorAuth/stripeMembershipAuthId").is_none());
        assert!(read_item(&conn, "cursorAuth/userId").is_none());
        assert!(read_item(&conn, "glass.lastSignedInAuthId").is_none());
    }

    #[test]
    fn inject_patches_existing_identity_keys_from_jwt() {
        let jwt = sample_jwt("auth0|user_01NEXTACCOUNT00000000000");
        let conn = Connection::open_in_memory().expect("memory db");
        seed_item_table(
            &conn,
            &[
                ("cursorAuth/accessToken", "old-token"),
                ("cursorAuth/cachedEmail", "old@example.com"),
                ("cursorAuth/stripeMembershipAuthId", "auth0|user_01OLD"),
                ("cursorAuth/userId", "user_01OLD"),
                ("glass.lastSignedInAuthId", "auth0|user_01OLD"),
            ],
        );

        inject_account_into_conn(&conn, &sample_account(&jwt)).expect("inject");

        assert_eq!(
            read_item(&conn, "cursorAuth/stripeMembershipAuthId").as_deref(),
            Some("auth0|user_01NEXTACCOUNT00000000000")
        );
        assert_eq!(
            read_item(&conn, "cursorAuth/userId").as_deref(),
            Some("user_01NEXTACCOUNT00000000000")
        );
        assert_eq!(
            read_item(&conn, "glass.lastSignedInAuthId").as_deref(),
            Some("auth0|user_01NEXTACCOUNT00000000000")
        );
    }

    #[test]
    fn update_existing_cli_token_fields_only() {
        let mut value = serde_json::json!({
            "version": 1,
            "accessToken": "old-access",
            "nested": { "refresh_token": "old-refresh", "note": "keep" }
        });
        assert!(update_existing_token_fields(
            &mut value,
            "new-access",
            "new-refresh"
        ));
        assert_eq!(value["accessToken"], "new-access");
        assert_eq!(value["nested"]["refresh_token"], "new-refresh");
        assert_eq!(value["nested"]["note"], "keep");
        assert_eq!(value["version"], 1);
        assert!(value.get("refreshToken").is_none());
    }
}
