use crate::account_pool::{
    AccountAuthKind, AccountPool, AccountSource, ImportResult, PreparedAccountImport,
    PreparedAccountRestore,
};
use crate::models::ToolKind;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::{
    aead, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SESSION_TTL_MS: i64 = 10 * 60 * 1_000;
const MAX_SESSIONS: usize = 32;
const BACKUP_VERSION: u32 = 1;
const BACKUP_KDF_ITERATIONS: u32 = 210_000;
const BACKUP_SALT_BYTES: usize = 16;
const BACKUP_NONCE_BYTES: usize = 12;
const BACKUP_KEY_BYTES: usize = 32;
const MAX_BACKUP_BYTES: usize = 24 * 1024 * 1024;
const MAX_PASSPHRASE_BYTES: usize = 4 * 1024;
const BACKUP_AAD: &[u8] = b"trusted-carpool-account-backup:v1";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountPreviewAction {
    New,
    Update,
    Conflict,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountPreviewItem {
    pub item_id: String,
    pub tool: ToolKind,
    pub auth_kind: AccountAuthKind,
    pub name: String,
    pub source: AccountSource,
    pub action: AccountPreviewAction,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountImportPreview {
    pub session_id: String,
    pub expires_at_ms: i64,
    pub items: Vec<AccountPreviewItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RestoreMode {
    #[default]
    Merge,
    Replace,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountRestorePreview {
    pub session_id: String,
    pub expires_at_ms: i64,
    pub mode: RestoreMode,
    pub items: Vec<AccountPreviewItem>,
    pub remove_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountRestoreResult {
    pub imported: usize,
    pub updated: usize,
    pub removed: usize,
    pub accounts: Vec<crate::account_pool::AccountSummary>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupEnvelope {
    version: u32,
    kdf: String,
    iterations: u32,
    salt: String,
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

enum SessionPayload {
    Import(PreparedAccountImport),
    Restore {
        prepared: PreparedAccountRestore,
        mode: RestoreMode,
    },
}

struct TransferSession {
    account_pool_path: PathBuf,
    expires_at_ms: i64,
    payload: SessionPayload,
}

#[derive(Default)]
struct TransferSessions {
    sessions: HashMap<String, TransferSession>,
}

static SESSIONS: OnceLock<Mutex<TransferSessions>> = OnceLock::new();

fn sessions() -> &'static Mutex<TransferSessions> {
    SESSIONS.get_or_init(|| Mutex::new(TransferSessions::default()))
}

pub fn preview_import(
    pool: &AccountPool,
    prepared: PreparedAccountImport,
) -> Result<AccountImportPreview, String> {
    let items = pool.preview_prepared_import(&prepared)?;
    let (session_id, expires_at_ms) = insert_session(pool, SessionPayload::Import(prepared))?;
    Ok(AccountImportPreview {
        session_id,
        expires_at_ms,
        items,
    })
}

pub fn commit_import(pool: &AccountPool, session_id: &str) -> Result<ImportResult, String> {
    let session = take_session(pool, session_id)?;
    let SessionPayload::Import(prepared) = session.payload else {
        return Err("导入预览会话类型无效".to_string());
    };
    pool.commit_prepared_import(prepared)
}

pub fn cancel_session(pool: &AccountPool, session_id: &str) -> Result<bool, String> {
    let mut sessions = sessions()
        .lock()
        .map_err(|_| "账号预览会话暂时不可用".to_string())?;
    prune_expired(&mut sessions, now_ms());
    let Some(session) = sessions.sessions.get(session_id) else {
        return Ok(false);
    };
    if session.account_pool_path != pool.path() {
        return Err("账号预览会话不属于当前账号池".to_string());
    }
    sessions.sessions.remove(session_id);
    Ok(true)
}

pub fn export_backup_bytes(pool: &AccountPool, passphrase: &str) -> Result<Vec<u8>, String> {
    validate_passphrase(passphrase)?;
    let mut plaintext = pool.backup_plaintext()?;
    let mut salt = [0u8; BACKUP_SALT_BYTES];
    let mut nonce = [0u8; BACKUP_NONCE_BYTES];
    let random = SystemRandom::new();
    random
        .fill(&mut salt)
        .map_err(|_| "无法生成备份派生盐".to_string())?;
    random
        .fill(&mut nonce)
        .map_err(|_| "无法生成备份加密随机数".to_string())?;
    let mut key_bytes = derive_backup_key(passphrase, &salt)?;
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "无法初始化备份加密".to_string())?;
    let seal_result = key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(BACKUP_AAD),
        &mut plaintext,
    );
    key_bytes.fill(0);
    seal_result.map_err(|_| "无法加密账号备份".to_string())?;
    let envelope = BackupEnvelope {
        version: BACKUP_VERSION,
        kdf: "PBKDF2-HMAC-SHA256".to_string(),
        iterations: BACKUP_KDF_ITERATIONS,
        salt: URL_SAFE_NO_PAD.encode(salt),
        algorithm: "AES-256-GCM".to_string(),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(plaintext),
    };
    let mut bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|error| format!("无法编码账号备份: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_BACKUP_BYTES {
        return Err("账号备份内容过大".to_string());
    }
    Ok(bytes)
}

pub fn write_backup_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_BACKUP_BYTES {
        return Err("账号备份内容过大".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "账号备份路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建账号备份目录: {error}"))?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("无法创建账号备份: {error}"))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!("无法写入账号备份: {error}"));
    }
    Ok(())
}

pub fn preview_restore(
    pool: &AccountPool,
    bytes: &[u8],
    passphrase: &str,
    mode: RestoreMode,
) -> Result<AccountRestorePreview, String> {
    let mut plaintext = decrypt_backup(bytes, passphrase)?;
    let prepared = pool.parse_backup_plaintext(&plaintext);
    plaintext.fill(0);
    let prepared = prepared?;
    let (items, remove_count) = pool.preview_prepared_restore(&prepared, mode)?;
    let (session_id, expires_at_ms) =
        insert_session(pool, SessionPayload::Restore { prepared, mode })?;
    Ok(AccountRestorePreview {
        session_id,
        expires_at_ms,
        mode,
        items,
        remove_count,
    })
}

pub fn commit_restore(
    pool: &AccountPool,
    session_id: &str,
    mode: RestoreMode,
    confirm_replace: bool,
) -> Result<AccountRestoreResult, String> {
    if mode == RestoreMode::Replace && !confirm_replace {
        return Err("替换恢复需要显式二次确认".to_string());
    }
    let session = take_session(pool, session_id)?;
    let SessionPayload::Restore {
        prepared,
        mode: previewed_mode,
    } = session.payload
    else {
        return Err("恢复预览会话类型无效".to_string());
    };
    if previewed_mode != mode {
        return Err("恢复方式已变化，请重新预览".to_string());
    }
    pool.commit_prepared_restore(prepared, mode)
}

fn decrypt_backup(bytes: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    validate_passphrase(passphrase)?;
    if bytes.len() > MAX_BACKUP_BYTES {
        return Err("账号备份文件过大".to_string());
    }
    let envelope: BackupEnvelope =
        serde_json::from_slice(bytes).map_err(|_| "账号备份格式已损坏".to_string())?;
    if envelope.version != BACKUP_VERSION {
        return Err(format!("不支持的账号备份版本: {}", envelope.version));
    }
    if envelope.kdf != "PBKDF2-HMAC-SHA256"
        || envelope.iterations != BACKUP_KDF_ITERATIONS
        || envelope.algorithm != "AES-256-GCM"
    {
        return Err("不支持的账号备份加密格式".to_string());
    }
    let salt = URL_SAFE_NO_PAD
        .decode(envelope.salt)
        .ok()
        .and_then(|bytes| <[u8; BACKUP_SALT_BYTES]>::try_from(bytes).ok())
        .ok_or_else(|| "账号备份派生盐无效".to_string())?;
    let nonce = URL_SAFE_NO_PAD
        .decode(envelope.nonce)
        .ok()
        .and_then(|bytes| <[u8; BACKUP_NONCE_BYTES]>::try_from(bytes).ok())
        .ok_or_else(|| "账号备份加密随机数无效".to_string())?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(envelope.ciphertext)
        .map_err(|_| "账号备份密文格式无效".to_string())?;
    let mut key_bytes = derive_backup_key(passphrase, &salt)?;
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "无法初始化备份解密".to_string())?;
    let open_result = key.open_in_place(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(BACKUP_AAD),
        &mut ciphertext,
    );
    let plaintext_len = open_result.as_ref().map(|plaintext| plaintext.len());
    key_bytes.fill(0);
    let plaintext_len = plaintext_len.map_err(|_| "备份口令错误或文件已被篡改".to_string())?;
    ciphertext.truncate(plaintext_len);
    Ok(ciphertext)
}

fn derive_backup_key(passphrase: &str, salt: &[u8]) -> Result<[u8; BACKUP_KEY_BYTES], String> {
    let iterations =
        NonZeroU32::new(BACKUP_KDF_ITERATIONS).ok_or_else(|| "备份密钥派生参数无效".to_string())?;
    let mut key = [0u8; BACKUP_KEY_BYTES];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        passphrase.as_bytes(),
        &mut key,
    );
    Ok(key)
}

fn validate_passphrase(passphrase: &str) -> Result<(), String> {
    let length = passphrase.len();
    if length < 8 {
        return Err("备份口令至少需要 8 个字符".to_string());
    }
    if length > MAX_PASSPHRASE_BYTES {
        return Err("备份口令过长".to_string());
    }
    Ok(())
}

fn insert_session(pool: &AccountPool, payload: SessionPayload) -> Result<(String, i64), String> {
    let current = now_ms();
    let expires_at_ms = current.saturating_add(SESSION_TTL_MS);
    let mut sessions = sessions()
        .lock()
        .map_err(|_| "账号预览会话暂时不可用".to_string())?;
    prune_expired(&mut sessions, current);
    while sessions.sessions.len() >= MAX_SESSIONS {
        let Some(oldest) = sessions
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.expires_at_ms)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        sessions.sessions.remove(&oldest);
    }
    let session_id = Uuid::new_v4().to_string();
    sessions.sessions.insert(
        session_id.clone(),
        TransferSession {
            account_pool_path: pool.path().to_path_buf(),
            expires_at_ms,
            payload,
        },
    );
    Ok((session_id, expires_at_ms))
}

fn take_session(pool: &AccountPool, session_id: &str) -> Result<TransferSession, String> {
    let current = now_ms();
    let mut sessions = sessions()
        .lock()
        .map_err(|_| "账号预览会话暂时不可用".to_string())?;
    prune_expired(&mut sessions, current);
    let Some(session) = sessions.sessions.get(session_id) else {
        return Err("账号预览已过期或不存在，请重新预览".to_string());
    };
    if session.account_pool_path != pool.path() {
        return Err("账号预览会话不属于当前账号池".to_string());
    }
    let session = sessions
        .sessions
        .remove(session_id)
        .ok_or_else(|| "账号预览已过期或不存在，请重新预览".to_string())?;
    if session.expires_at_ms <= current {
        return Err("账号预览已过期，请重新预览".to_string());
    }
    Ok(session)
}

fn prune_expired(sessions: &mut TransferSessions, current: i64) {
    sessions
        .sessions
        .retain(|_, session| session.expires_at_ms > current);
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_pool::{ImportOptions, DEFAULT_ACCOUNT_PRIORITY};
    use serde_json::Value;
    use tempfile::TempDir;

    fn pool() -> (TempDir, AccountPool) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = AccountPool::new(temp.path().join("private/accounts.json"));
        (temp, pool)
    }

    fn seeded_pool() -> (TempDir, AccountPool) {
        let (temp, pool) = pool();
        pool.import_content(
            "sk-ant-api03-backup-secret",
            ImportOptions {
                tool: Some(ToolKind::Claude),
                name: Some("主账号".to_string()),
                priority: Some(7),
                enabled: Some(false),
                ..ImportOptions::default()
            },
        )
        .expect("seed account");
        (temp, pool)
    }

    #[test]
    fn import_preview_contains_no_credentials_and_is_one_time() {
        let (_temp, pool) = pool();
        let secret = "sk-ant-api03-preview-secret";
        let prepared = pool
            .prepare_import_content(
                secret,
                ImportOptions {
                    tool: Some(ToolKind::Claude),
                    ..ImportOptions::default()
                },
                AccountSource::Json,
            )
            .expect("prepare");
        let preview = preview_import(&pool, prepared).expect("preview");
        let serialized = serde_json::to_string(&preview).expect("serialize preview");
        assert!(!serialized.contains(secret));
        assert_eq!(preview.items[0].action, AccountPreviewAction::New);

        let result = commit_import(&pool, &preview.session_id).expect("commit");
        assert_eq!(result.imported, 1);
        assert!(commit_import(&pool, &preview.session_id).is_err());
    }

    #[test]
    fn encrypted_backup_round_trip_preserves_metadata_without_plaintext_leaks() {
        let (_source_temp, source) = seeded_pool();
        let bytes = export_backup_bytes(&source, "correct horse battery staple").expect("backup");
        let encoded = String::from_utf8(bytes.clone()).expect("json envelope");
        assert!(!encoded.contains("sk-ant-api03-backup-secret"));
        assert!(!encoded.contains("主账号"));

        let (_target_temp, target) = pool();
        let preview = preview_restore(
            &target,
            &bytes,
            "correct horse battery staple",
            RestoreMode::Merge,
        )
        .expect("preview restore");
        assert_eq!(preview.items[0].action, AccountPreviewAction::New);
        let result = commit_restore(&target, &preview.session_id, RestoreMode::Merge, false)
            .expect("restore");
        assert_eq!(result.imported, 1);
        let restored_accounts = target.list().expect("restored list");
        let [restored] = restored_accounts.as_slice() else {
            panic!("one restored account expected");
        };
        assert_eq!(restored.name, "主账号");
        assert_eq!(restored.priority, 7);
        assert!(!restored.enabled);
    }

    #[test]
    fn wrong_password_tampering_and_unknown_versions_never_modify_the_pool() {
        let (_source_temp, source) = seeded_pool();
        let bytes = export_backup_bytes(&source, "correct horse battery staple").expect("backup");
        let (_target_temp, target) = pool();
        target
            .import_content(
                "sk-proj-existing-target",
                ImportOptions {
                    tool: Some(ToolKind::Codex),
                    ..ImportOptions::default()
                },
            )
            .expect("target seed");
        let before = target.list().expect("before");

        assert!(
            preview_restore(&target, &bytes, "incorrect password", RestoreMode::Merge).is_err()
        );
        let mut tampered: Value = serde_json::from_slice(&bytes).expect("envelope");
        tampered["ciphertext"] = Value::String("AAAA".to_string());
        assert!(preview_restore(
            &target,
            &serde_json::to_vec(&tampered).unwrap(),
            "correct horse battery staple",
            RestoreMode::Merge
        )
        .is_err());
        let mut unknown: Value = serde_json::from_slice(&bytes).expect("envelope");
        unknown["version"] = Value::from(999);
        assert!(preview_restore(
            &target,
            &serde_json::to_vec(&unknown).unwrap(),
            "correct horse battery staple",
            RestoreMode::Merge
        )
        .is_err());
        assert!(preview_restore(
            &target,
            &bytes[..bytes.len() / 2],
            "correct horse battery staple",
            RestoreMode::Merge
        )
        .is_err());
        assert_eq!(target.list().expect("unchanged"), before);
    }

    #[test]
    fn merge_keeps_local_choices_and_replace_requires_explicit_confirmation() {
        let (_source_temp, source) = seeded_pool();
        let bytes = export_backup_bytes(&source, "correct horse battery staple").expect("backup");
        let (_target_temp, target) = pool();
        target
            .import_content(
                "sk-ant-api03-backup-secret",
                ImportOptions {
                    tool: Some(ToolKind::Claude),
                    name: Some("本机名称".to_string()),
                    priority: Some(DEFAULT_ACCOUNT_PRIORITY + 12),
                    enabled: Some(true),
                    ..ImportOptions::default()
                },
            )
            .expect("target seed");

        let merge = preview_restore(
            &target,
            &bytes,
            "correct horse battery staple",
            RestoreMode::Merge,
        )
        .expect("merge preview");
        let result =
            commit_restore(&target, &merge.session_id, RestoreMode::Merge, false).expect("merge");
        assert_eq!(result.updated, 1);
        let account = &target.list().unwrap()[0];
        assert_eq!(account.name, "本机名称");
        assert_eq!(account.priority, DEFAULT_ACCOUNT_PRIORITY + 12);
        assert!(account.enabled);

        let replace = preview_restore(
            &target,
            &bytes,
            "correct horse battery staple",
            RestoreMode::Replace,
        )
        .expect("replace preview");
        assert!(commit_restore(&target, &replace.session_id, RestoreMode::Replace, false).is_err());
        assert_eq!(target.list().unwrap()[0].name, "本机名称");

        let replaced = commit_restore(&target, &replace.session_id, RestoreMode::Replace, true)
            .expect("confirmed replace");
        assert_eq!(replaced.updated, 1);
        let replaced_accounts = target.list().unwrap();
        let replaced_account = &replaced_accounts[0];
        assert_eq!(replaced_account.name, "主账号");
        assert_eq!(replaced_account.priority, 7);
        assert!(!replaced_account.enabled);
        assert!(target
            .path()
            .with_file_name("accounts.json.pre-restore.bak")
            .is_file());
    }

    #[test]
    fn expired_and_cancelled_previews_cannot_commit() {
        let (_temp, pool) = pool();
        let prepared = pool
            .prepare_import_content(
                "sk-ant-api03-expiring-preview",
                ImportOptions {
                    tool: Some(ToolKind::Claude),
                    ..ImportOptions::default()
                },
                AccountSource::Json,
            )
            .expect("prepare");
        let preview = preview_import(&pool, prepared).expect("preview");
        sessions()
            .lock()
            .expect("sessions")
            .sessions
            .get_mut(&preview.session_id)
            .expect("session")
            .expires_at_ms = now_ms().saturating_sub(1);
        assert!(commit_import(&pool, &preview.session_id).is_err());
        assert!(pool.list().unwrap().is_empty());

        let prepared = pool
            .prepare_import_content(
                "sk-ant-api03-cancelled-preview",
                ImportOptions {
                    tool: Some(ToolKind::Claude),
                    ..ImportOptions::default()
                },
                AccountSource::Json,
            )
            .expect("prepare cancel");
        let preview = preview_import(&pool, prepared).expect("preview cancel");
        assert!(cancel_session(&pool, &preview.session_id).expect("cancel"));
        assert!(commit_import(&pool, &preview.session_id).is_err());
    }

    #[test]
    fn backup_writer_handles_nested_paths_and_refuses_overwrite() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("nested").join("accounts.tcarpool-backup");
        write_backup_file(&path, b"encrypted-backup").expect("write backup");
        assert_eq!(fs::read(&path).unwrap(), b"encrypted-backup");
        assert!(write_backup_file(&path, b"replacement").is_err());
    }
}
