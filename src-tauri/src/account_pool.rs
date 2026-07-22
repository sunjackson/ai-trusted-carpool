use crate::account_router::RouteHealthSummary;
use crate::account_transfer::{
    AccountPreviewAction, AccountPreviewItem, AccountRestoreResult, RestoreMode,
};
use crate::models::ToolKind;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const STORE_VERSION: u32 = 2;
const LEGACY_STORE_VERSION: u32 = 1;
const ENVELOPE_VERSION: u32 = 1;
const ACCOUNT_KEY_BYTES: usize = 32;
const ACCOUNT_NONCE_BYTES: usize = 12;
const ACCOUNT_STORE_AAD: &[u8] = b"trusted-carpool-account-pool:v1";
const MAX_STORE_BYTES: u64 = 24 * 1024 * 1024;
const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACCOUNTS: usize = 1_000;
const MAX_SECRET_BYTES: usize = 32 * 1024;
const MAX_NAME_CHARS: usize = 80;
const MAX_SOURCE_ID_CHARS: usize = 160;
const MAX_PRIORITY: i32 = 1_000_000;
const BACKUP_PAYLOAD_VERSION: u32 = 1;

// AccountPool instances are short-lived in Tauri commands. A process-wide lock
// prevents two independently constructed instances from racing key creation or
// atomic replacement of the same store.
static ACCOUNT_STORE_LOCK: Mutex<()> = Mutex::new(());

pub const DEFAULT_ACCOUNT_PRIORITY: i32 = 100;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AccountAuthKind {
    #[serde(rename = "apiKey")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountSource {
    Local,
    Json,
    File,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CredentialState {
    Normal,
    Expired,
    ReimportRequired,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub id: String,
    pub tool: ToolKind,
    pub name: String,
    pub auth_kind: AccountAuthKind,
    pub enabled: bool,
    pub priority: i32,
    pub source: AccountSource,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub credential_state: CredentialState,
    pub route_health: RouteHealthSummary,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportOptions {
    #[serde(default)]
    pub tool: Option<ToolKind>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub source: Option<AccountSource>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAccountInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub imported: usize,
    pub updated: usize,
    pub accounts: Vec<AccountSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalRefreshResult {
    pub updated: usize,
    pub discovered: usize,
    pub accounts: Vec<AccountSummary>,
}

/// A credential is deliberately neither serializable nor debuggable. It may only
/// leave this module through explicit accessors used by the local relay.
#[derive(Clone)]
pub struct AccountCredential {
    auth_kind: AccountAuthKind,
    secret: String,
    // Preserved encrypted for import fidelity. The app deliberately leaves
    // refresh-token rotation to the official client that owns the login.
    #[allow(dead_code)]
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at_ms: Option<i64>,
}

impl AccountCredential {
    pub fn auth_kind(&self) -> AccountAuthKind {
        self.auth_kind
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    #[allow(dead_code)]
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_deref()
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    #[allow(dead_code)]
    pub fn expires_at_ms(&self) -> Option<i64> {
        self.expires_at_ms
    }

    pub fn is_expired_at(&self, timestamp_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms <= timestamp_ms)
    }
}

/// An enabled routing candidate. This type intentionally has no Debug or
/// Serialize implementation because it contains an AccountCredential.
#[derive(Clone)]
pub struct AccountCandidate {
    pub id: String,
    pub tool: ToolKind,
    pub name: String,
    pub priority: u32,
    pub credential: AccountCredential,
}

#[derive(Clone)]
pub struct AccountPool {
    path: PathBuf,
}

pub(crate) struct PreparedAccountImport {
    parsed: Vec<ParsedAccount>,
    options: ImportOptions,
    default_source: AccountSource,
}

pub(crate) struct PreparedAccountRestore {
    store: AccountStore,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountBackupPayload {
    version: u32,
    exported_at_ms: i64,
    accounts: Vec<StoredAccount>,
}

impl AccountPool {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<AccountSummary>, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut summaries = load_store(&self.path)?
            .accounts
            .iter()
            .map(StoredAccount::summary)
            .collect::<Vec<_>>();
        sort_summaries(&mut summaries);
        Ok(summaries)
    }

    pub fn has_enabled(&self, tool: ToolKind) -> Result<bool, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        // Relay routing drops OAuth credentials during the one-minute
        // refresh safety window, so authentication detection must agree.
        let current_ms = now_ms().saturating_add(60_000);
        Ok(load_store(&self.path)?.accounts.iter().any(|account| {
            account.tool == tool && account.enabled && !account.credential.is_expired_at(current_ms)
        }))
    }

    /// Returns enabled credentials ordered by ascending priority. Equal
    /// priorities use stable creation order; runtime health/LRU state may
    /// further reorder that equal-priority group.
    pub fn candidates(&self, tool: ToolKind) -> Result<Vec<AccountCandidate>, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut accounts = load_store(&self.path)?
            .accounts
            .into_iter()
            .filter(|account| account.tool == tool && account.enabled)
            .collect::<Vec<_>>();
        accounts.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(accounts.into_iter().map(StoredAccount::candidate).collect())
    }

    pub(crate) fn prepare_import_content(
        &self,
        content: &str,
        options: ImportOptions,
        default_source: AccountSource,
    ) -> Result<PreparedAccountImport, String> {
        let parsed = parse_import_content(content, options.tool)?;
        prepare_account_import(parsed, options, default_source)
    }

    pub(crate) fn prepare_import_contents(
        &self,
        contents: &[String],
        mut options: ImportOptions,
        default_source: AccountSource,
    ) -> Result<PreparedAccountImport, String> {
        if contents.is_empty() {
            return Err("请选择至少一个账号文件".to_string());
        }
        let total_bytes = contents
            .iter()
            .try_fold(0usize, |total, content| total.checked_add(content.len()))
            .ok_or_else(|| "导入内容过大".to_string())?;
        if total_bytes > MAX_IMPORT_BYTES {
            return Err("导入内容过大".to_string());
        }
        let mut parsed = Vec::new();
        for (index, content) in contents.iter().enumerate() {
            parsed.extend(
                parse_import_content(content, options.tool)
                    .map_err(|error| format!("第 {} 个账号文件无法导入: {error}", index + 1))?,
            );
        }
        options.source = Some(default_source);
        prepare_account_import(parsed, options, default_source)
    }

    pub(crate) fn prepare_local_import(&self) -> Result<PreparedAccountImport, String> {
        let home = dirs::home_dir().ok_or_else(|| "无法获取用户主目录".to_string())?;
        #[cfg(target_os = "macos")]
        let keychain_json = read_claude_keychain_credentials();
        #[cfg(not(target_os = "macos"))]
        let keychain_json: Option<String> = None;
        let parsed = collect_local_accounts(&home, keychain_json.as_deref())?;
        prepare_account_import(
            parsed,
            ImportOptions {
                source: Some(AccountSource::Local),
                ..ImportOptions::default()
            },
            AccountSource::Local,
        )
    }

    pub(crate) fn preview_prepared_import(
        &self,
        prepared: &PreparedAccountImport,
    ) -> Result<Vec<AccountPreviewItem>, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let store = load_store(&self.path)?;
        preview_import_items(&store, prepared)
    }

    pub(crate) fn commit_prepared_import(
        &self,
        prepared: PreparedAccountImport,
    ) -> Result<ImportResult, String> {
        self.import_parsed(prepared.parsed, prepared.options, prepared.default_source)
    }

    pub(crate) fn backup_plaintext(&self) -> Result<Vec<u8>, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let store = load_store(&self.path)?;
        serde_json::to_vec(&AccountBackupPayload {
            version: BACKUP_PAYLOAD_VERSION,
            exported_at_ms: now_ms(),
            accounts: store.accounts,
        })
        .map_err(|error| format!("无法编码账号备份内容: {error}"))
    }

    pub(crate) fn parse_backup_plaintext(
        &self,
        plaintext: &[u8],
    ) -> Result<PreparedAccountRestore, String> {
        let payload: AccountBackupPayload =
            serde_json::from_slice(plaintext).map_err(|_| "账号备份解密内容已损坏".to_string())?;
        if payload.version != BACKUP_PAYLOAD_VERSION {
            return Err(format!("不支持的账号备份内容版本: {}", payload.version));
        }
        let store = AccountStore {
            version: STORE_VERSION,
            accounts: payload.accounts,
        };
        validate_store(&store)?;
        Ok(PreparedAccountRestore { store })
    }

    pub(crate) fn preview_prepared_restore(
        &self,
        prepared: &PreparedAccountRestore,
        mode: RestoreMode,
    ) -> Result<(Vec<AccountPreviewItem>, usize), String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let current = load_store(&self.path)?;
        preview_restore_items(&current, &prepared.store, mode)
    }

    pub(crate) fn commit_prepared_restore(
        &self,
        prepared: PreparedAccountRestore,
        mode: RestoreMode,
    ) -> Result<AccountRestoreResult, String> {
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let current = load_store(&self.path)?;
        let (mut proposed, imported, updated, removed, affected_ids) =
            build_restored_store(&current, prepared.store, mode)?;
        proposed.version = STORE_VERSION;
        validate_store(&proposed)?;

        if mode == RestoreMode::Replace {
            write_restore_snapshot(&self.path, &current)?;
        }
        if let Err(error) = save_store(&self.path, &proposed) {
            if mode == RestoreMode::Replace {
                let _ = rollback_restore_snapshot(&self.path);
            }
            return Err(error);
        }
        if let Err(error) = read_store_file(&self.path, &self.path) {
            if mode == RestoreMode::Replace {
                rollback_restore_snapshot(&self.path)?;
            }
            return Err(format!("恢复后的账号存储校验失败，已自动回滚: {error}"));
        }

        let affected = affected_ids.into_iter().collect::<HashSet<_>>();
        let mut accounts = proposed
            .accounts
            .iter()
            .filter(|account| mode == RestoreMode::Replace || affected.contains(&account.id))
            .map(StoredAccount::summary)
            .collect::<Vec<_>>();
        sort_summaries(&mut accounts);
        Ok(AccountRestoreResult {
            imported,
            updated,
            removed,
            accounts,
        })
    }

    #[allow(dead_code)] // Legacy native API; Tauri now routes through preview + commit.
    pub fn import_content(
        &self,
        content: &str,
        options: ImportOptions,
    ) -> Result<ImportResult, String> {
        let parsed = parse_import_content(content, options.tool)?;
        self.import_parsed(parsed, options, AccountSource::Json)
    }

    #[allow(dead_code)] // The webview imports contents; retained for native callers.
    pub fn import_files(
        &self,
        paths: &[PathBuf],
        mut options: ImportOptions,
    ) -> Result<ImportResult, String> {
        if paths.is_empty() {
            return Err("请选择至少一个账号文件".to_string());
        }
        let mut parsed = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let file_number = index + 1;
            let metadata = fs::metadata(path)
                .map_err(|error| format!("无法读取第 {file_number} 个账号文件: {error}"))?;
            if metadata.len() > MAX_IMPORT_BYTES as u64 {
                return Err(format!("第 {file_number} 个账号文件过大"));
            }
            let content = fs::read_to_string(path)
                .map_err(|error| format!("无法读取第 {file_number} 个账号文件内容: {error}"))?;
            parsed.extend(
                parse_import_content(&content, options.tool)
                    .map_err(|error| format!("第 {file_number} 个账号文件无法导入: {error}"))?,
            );
        }
        options.source = Some(AccountSource::File);
        self.import_parsed(parsed, options, AccountSource::File)
    }

    #[allow(dead_code)] // Legacy native API; Tauri now routes through preview + commit.
    pub fn import_local(&self) -> Result<ImportResult, String> {
        let home = dirs::home_dir().ok_or_else(|| "无法获取用户主目录".to_string())?;
        #[cfg(target_os = "macos")]
        let keychain_json = read_claude_keychain_credentials();
        #[cfg(not(target_os = "macos"))]
        let keychain_json: Option<String> = None;
        self.import_local_with_keychain(&home, keychain_json.as_deref())
    }

    #[allow(dead_code)] // Retained for deterministic native callers and tests.
    #[allow(dead_code)] // Deterministic file-only source used by native callers/tests.
    pub fn import_local_from(&self, home: &Path) -> Result<ImportResult, String> {
        self.import_local_with_keychain(home, None)
    }

    fn import_local_with_keychain(
        &self,
        home: &Path,
        keychain_json: Option<&str>,
    ) -> Result<ImportResult, String> {
        let parsed = collect_local_accounts(home, keychain_json)?;
        self.import_parsed(
            parsed,
            ImportOptions {
                source: Some(AccountSource::Local),
                ..ImportOptions::default()
            },
            AccountSource::Local,
        )
    }

    pub fn refresh_known_local(&self) -> Result<LocalRefreshResult, String> {
        let home = dirs::home_dir().ok_or_else(|| "无法获取用户主目录".to_string())?;
        #[cfg(target_os = "macos")]
        let keychain_json = read_claude_keychain_credentials();
        #[cfg(not(target_os = "macos"))]
        let keychain_json: Option<String> = None;
        self.refresh_known_local_with_keychain(&home, keychain_json.as_deref())
    }

    fn refresh_known_local_with_keychain(
        &self,
        home: &Path,
        keychain_json: Option<&str>,
    ) -> Result<LocalRefreshResult, String> {
        let parsed = collect_local_accounts(home, keychain_json)?;
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut store = load_store(&self.path)?;
        let now = now_ms();
        let mut updated_ids = Vec::new();
        let mut unmatched: Vec<ParsedAccount> = Vec::new();

        for candidate in parsed {
            if let Ok(Some(index)) = matching_account_index(&store.accounts, &candidate) {
                let account = &mut store.accounts[index];
                if account.credential != candidate.credential {
                    account.credential = candidate.credential;
                    if candidate.source_id.is_some() {
                        account.source_id = candidate.source_id;
                    }
                    account.updated_at_ms = now;
                    if !updated_ids.contains(&account.id) {
                        updated_ids.push(account.id.clone());
                    }
                }
            } else if !unmatched
                .iter()
                .any(|other| parsed_accounts_match(other, &candidate))
            {
                unmatched.push(candidate);
            }
        }

        if !updated_ids.is_empty() {
            validate_store(&store)?;
            save_store(&self.path, &store)?;
        }
        let affected = updated_ids.iter().cloned().collect::<HashSet<_>>();
        let mut accounts = store
            .accounts
            .iter()
            .filter(|account| affected.contains(&account.id))
            .map(StoredAccount::summary)
            .collect::<Vec<_>>();
        sort_summaries(&mut accounts);
        Ok(LocalRefreshResult {
            updated: accounts.len(),
            discovered: unmatched.len(),
            accounts,
        })
    }

    pub fn update(&self, id: &str, input: UpdateAccountInput) -> Result<AccountSummary, String> {
        if id.trim().is_empty() {
            return Err("账号编号不能为空".to_string());
        }
        let name = input.name.as_deref().map(normalize_name).transpose()?;
        let priority = input.priority.map(validate_priority).transpose()?;
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut store = load_store(&self.path)?;
        let account = store
            .accounts
            .iter_mut()
            .find(|account| account.id == id)
            .ok_or_else(|| "账号不存在".to_string())?;
        if let Some(name) = name {
            account.name = name;
        }
        if let Some(enabled) = input.enabled {
            account.enabled = enabled;
        }
        if let Some(priority) = priority {
            account.priority = priority;
        }
        account.updated_at_ms = now_ms();
        let summary = account.summary();
        save_store(&self.path, &store)?;
        Ok(summary)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        if id.trim().is_empty() {
            return Err("账号编号不能为空".to_string());
        }
        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut store = load_store(&self.path)?;
        let original_len = store.accounts.len();
        store.accounts.retain(|account| account.id != id);
        if store.accounts.len() == original_len {
            return Ok(false);
        }
        save_store(&self.path, &store)?;
        Ok(true)
    }

    fn import_parsed(
        &self,
        mut parsed: Vec<ParsedAccount>,
        options: ImportOptions,
        default_source: AccountSource,
    ) -> Result<ImportResult, String> {
        if parsed.is_empty() {
            return Err("未找到可导入的账号凭证".to_string());
        }
        if parsed.len() > MAX_ACCOUNTS {
            return Err(format!("单次最多导入 {MAX_ACCOUNTS} 个账号"));
        }
        let priority = options.priority.map(validate_priority).transpose()?;
        let explicit_name = options.name.as_deref().map(normalize_name).transpose()?;
        let source = options.source.unwrap_or(default_source);
        let apply_explicit_name = parsed.len() == 1;

        let _guard = ACCOUNT_STORE_LOCK
            .lock()
            .map_err(|_| "账号存储暂时不可用".to_string())?;
        let mut store = load_store(&self.path)?;
        let now = now_ms();
        let mut imported = 0usize;
        let mut updated = 0usize;
        let mut affected_ids = Vec::new();

        for candidate in parsed.drain(..) {
            validate_parsed_account(&candidate)?;
            let parsed_priority = candidate.priority.map(validate_priority).transpose()?;
            let candidate_name = candidate
                .name
                .as_deref()
                .map(normalize_name)
                .transpose()?
                .filter(|name| !candidate.credential.contains_secret(name));
            let existing = matching_account_index(&store.accounts, &candidate)?;

            let id = if let Some(index) = existing {
                let account = &mut store.accounts[index];
                account.credential = candidate.credential;
                if candidate.source_id.is_some() {
                    account.source_id = candidate.source_id;
                }
                // Credential rotation must not silently undo local account
                // management choices. Name, priority, and enabled state are
                // changed only through the explicit update command.
                account.updated_at_ms = now;
                updated += 1;
                account.id.clone()
            } else {
                if store.accounts.len() >= MAX_ACCOUNTS {
                    return Err(format!("账号数量不能超过 {MAX_ACCOUNTS}"));
                }
                let name = if apply_explicit_name {
                    explicit_name.clone().or(candidate_name)
                } else {
                    candidate_name
                }
                .unwrap_or_else(|| {
                    default_account_name(candidate.tool, candidate.credential.kind())
                });
                let id = Uuid::new_v4().to_string();
                store.accounts.push(StoredAccount {
                    id: id.clone(),
                    tool: candidate.tool,
                    name,
                    enabled: options.enabled.or(candidate.enabled).unwrap_or(true),
                    priority: priority
                        .or(parsed_priority)
                        .unwrap_or(DEFAULT_ACCOUNT_PRIORITY),
                    source,
                    created_at_ms: now,
                    updated_at_ms: now,
                    source_id: candidate.source_id,
                    credential: candidate.credential,
                });
                imported += 1;
                id
            };
            if !affected_ids.contains(&id) {
                affected_ids.push(id);
            }
        }

        validate_store(&store)?;
        save_store(&self.path, &store)?;
        let affected = affected_ids.into_iter().collect::<HashSet<_>>();
        let mut accounts = store
            .accounts
            .iter()
            .filter(|account| affected.contains(&account.id))
            .map(StoredAccount::summary)
            .collect::<Vec<_>>();
        sort_summaries(&mut accounts);
        Ok(ImportResult {
            imported,
            updated,
            accounts,
        })
    }
}

fn collect_local_accounts(
    home: &Path,
    keychain_json: Option<&str>,
) -> Result<Vec<ParsedAccount>, String> {
    let allow_codex_api_keys = codex_local_config_allows_official_api(home);
    let sources = [
        (
            home.join(".claude/.credentials.json"),
            ToolKind::Claude,
            "credentials",
        ),
        (
            home.join(".claude/settings.json"),
            ToolKind::Claude,
            "settings",
        ),
        (
            home.join(".claude/settings.local.json"),
            ToolKind::Claude,
            "settings-local",
        ),
        (home.join(".claude.json"), ToolKind::Claude, "profile"),
        (home.join(".codex/auth.json"), ToolKind::Codex, "auth"),
    ];
    let mut parsed = Vec::new();
    let mut found_files = 0usize;
    let mut first_error = None;

    for (path, tool, slot) in sources {
        if !path.exists() {
            continue;
        }
        found_files += 1;
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    format!("无法读取本机 {} 账号配置: {error}", tool.command())
                });
                continue;
            }
        };
        match parse_import_content(&content, Some(tool)) {
            Ok(mut accounts) => {
                if tool == ToolKind::Codex && !allow_codex_api_keys {
                    accounts.retain(|account| account.credential.kind() == AccountAuthKind::OAuth);
                }
                for (index, account) in accounts.iter_mut().enumerate() {
                    account.source_id = Some(local_source_id(tool, slot, index));
                }
                parsed.append(&mut accounts);
            }
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    format!("本机 {} 账号配置无法导入: {error}", tool.command())
                });
            }
        }
    }

    if let Some(content) = keychain_json {
        found_files += 1;
        match parse_import_content(content, Some(ToolKind::Claude)) {
            Ok(mut accounts) => {
                for (index, account) in accounts.iter_mut().enumerate() {
                    account.source_id = Some(local_source_id(ToolKind::Claude, "keychain", index));
                }
                parsed.append(&mut accounts)
            }
            Err(error) => {
                first_error
                    .get_or_insert_with(|| format!("本机 Claude Keychain 配置无法导入: {error}"));
            }
        }
    }

    if parsed.is_empty() {
        if let Some(error) = first_error {
            return Err(error);
        }
        return if found_files == 0 {
            Err("未找到本机 Claude 或 Codex 账号配置".to_string())
        } else {
            Err("本机配置中未找到可导入的 Claude 或 Codex 凭证".to_string())
        };
    }
    Ok(parsed)
}

fn local_source_id(tool: ToolKind, slot: &str, index: usize) -> String {
    format!("local:{}:{slot}:{index}", tool.command())
}

#[cfg(target_os = "macos")]
fn read_claude_keychain_credentials() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let content = String::from_utf8(output.stdout).ok()?;
    let content = content.trim();
    (!content.is_empty()).then(|| content.to_string())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountStore {
    version: u32,
    #[serde(default)]
    accounts: Vec<StoredAccount>,
}

impl Default for AccountStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            accounts: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EncryptedStoreEnvelope {
    version: u32,
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccount {
    id: String,
    tool: ToolKind,
    name: String,
    enabled: bool,
    priority: i32,
    source: AccountSource,
    created_at_ms: i64,
    updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    credential: StoredCredential,
}

impl StoredAccount {
    fn summary(&self) -> AccountSummary {
        AccountSummary {
            id: self.id.clone(),
            tool: self.tool,
            name: self.name.clone(),
            auth_kind: self.credential.kind(),
            enabled: self.enabled,
            priority: self.priority,
            source: self.source,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            credential_state: if self
                .credential
                .is_expired_at(now_ms().saturating_add(60_000))
            {
                CredentialState::Expired
            } else {
                CredentialState::Normal
            },
            route_health: RouteHealthSummary::default(),
        }
    }

    fn candidate(self) -> AccountCandidate {
        AccountCandidate {
            id: self.id,
            tool: self.tool,
            name: self.name,
            priority: self.priority as u32,
            credential: self.credential.into_public(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredCredential {
    ApiKey {
        secret: String,
    },
    OAuth {
        #[serde(rename = "accessToken")]
        access_token: String,
        #[serde(
            default,
            rename = "refreshToken",
            skip_serializing_if = "Option::is_none"
        )]
        refresh_token: Option<String>,
        #[serde(default, rename = "accountId", skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(
            default,
            rename = "expiresAtMs",
            skip_serializing_if = "Option::is_none"
        )]
        expires_at_ms: Option<i64>,
    },
}

impl StoredCredential {
    fn kind(&self) -> AccountAuthKind {
        match self {
            Self::ApiKey { .. } => AccountAuthKind::ApiKey,
            Self::OAuth { .. } => AccountAuthKind::OAuth,
        }
    }

    fn secret(&self) -> &str {
        match self {
            Self::ApiKey { secret } => secret,
            Self::OAuth { access_token, .. } => access_token,
        }
    }

    fn account_id(&self) -> Option<&str> {
        match self {
            Self::OAuth { account_id, .. } => account_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            Self::ApiKey { .. } => None,
        }
    }

    fn is_expired_at(&self, timestamp_ms: i64) -> bool {
        matches!(
            self,
            Self::OAuth {
                expires_at_ms: Some(expires_at_ms),
                ..
            } if *expires_at_ms <= timestamp_ms
        )
    }

    fn same_identity(&self, other: &Self) -> bool {
        if self.kind() != other.kind() {
            return false;
        }
        match (self, other) {
            (
                Self::OAuth {
                    account_id: left_account_id,
                    refresh_token: left_refresh_token,
                    ..
                },
                Self::OAuth {
                    account_id: right_account_id,
                    refresh_token: right_refresh_token,
                    ..
                },
            ) => {
                let left_account_id = left_account_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let right_account_id = right_account_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let (Some(left), Some(right)) = (left_account_id, right_account_id) {
                    return left == right;
                }
                let left_refresh_token = left_refresh_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let right_refresh_token = right_refresh_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let (Some(left), Some(right)) = (left_refresh_token, right_refresh_token) {
                    return left == right;
                }
                self.secret() == other.secret()
            }
            _ => self.secret() == other.secret(),
        }
    }

    fn contains_secret(&self, value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        match self {
            Self::ApiKey { secret } => value.contains(secret) || secret.contains(value),
            Self::OAuth {
                access_token,
                refresh_token,
                ..
            } => {
                value.contains(access_token)
                    || access_token.contains(value)
                    || refresh_token
                        .as_ref()
                        .is_some_and(|token| value.contains(token) || token.contains(value))
            }
        }
    }

    fn into_public(self) -> AccountCredential {
        match self {
            Self::ApiKey { secret } => AccountCredential {
                auth_kind: AccountAuthKind::ApiKey,
                secret,
                refresh_token: None,
                account_id: None,
                expires_at_ms: None,
            },
            Self::OAuth {
                access_token,
                refresh_token,
                account_id,
                expires_at_ms,
            } => AccountCredential {
                auth_kind: AccountAuthKind::OAuth,
                secret: access_token,
                refresh_token,
                account_id,
                expires_at_ms,
            },
        }
    }
}

struct ParsedAccount {
    tool: ToolKind,
    name: Option<String>,
    priority: Option<i32>,
    enabled: Option<bool>,
    source_id: Option<String>,
    credential: StoredCredential,
}

fn parsed_accounts_match(left: &ParsedAccount, right: &ParsedAccount) -> bool {
    if left.tool != right.tool {
        return false;
    }
    if let (Some(left), Some(right)) = (left.credential.account_id(), right.credential.account_id())
    {
        return left == right;
    }
    if let (Some(left), Some(right)) = (left.source_id.as_deref(), right.source_id.as_deref()) {
        return left == right;
    }
    left.credential.same_identity(&right.credential)
}

fn matching_account_index(
    accounts: &[StoredAccount],
    candidate: &ParsedAccount,
) -> Result<Option<usize>, String> {
    let candidate_account_id = candidate.credential.account_id();
    let account_id_matches = candidate_account_id
        .map(|account_id| {
            accounts
                .iter()
                .enumerate()
                .filter(|(_, account)| {
                    account.tool == candidate.tool
                        && account.credential.account_id() == Some(account_id)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if account_id_matches.len() > 1 {
        return Err("多个本机账号具有相同的官方账号标识，请先处理冲突".to_string());
    }
    if let Some(index) = account_id_matches.first() {
        return Ok(Some(*index));
    }

    if let Some(source_id) = candidate.source_id.as_deref() {
        let source_matches = accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| {
                account.tool == candidate.tool && account.source_id.as_deref() == Some(source_id)
            })
            .collect::<Vec<_>>();
        if source_matches.len() > 1 {
            return Err("多个本机账号具有相同的来源标识，请先处理冲突".to_string());
        }
        if let Some((index, account)) = source_matches.first() {
            if let (Some(candidate_id), Some(existing_id)) =
                (candidate_account_id, account.credential.account_id())
            {
                if candidate_id != existing_id {
                    return Err("官方客户端来源槽位已切换为另一个账号，请先确认冲突".to_string());
                }
            }
            return Ok(Some(*index));
        }
    }

    let identity_matches = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| {
            account.tool == candidate.tool
                && account.credential.same_identity(&candidate.credential)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match identity_matches.as_slice() {
        [] => Ok(None),
        [index] => Ok(Some(*index)),
        _ => Err("导入凭据与多个现有账号匹配，请先处理冲突".to_string()),
    }
}

fn prepare_account_import(
    parsed: Vec<ParsedAccount>,
    options: ImportOptions,
    default_source: AccountSource,
) -> Result<PreparedAccountImport, String> {
    if parsed.is_empty() {
        return Err("未找到可导入的账号凭证".to_string());
    }
    if parsed.len() > MAX_ACCOUNTS {
        return Err(format!("单次最多导入 {MAX_ACCOUNTS} 个账号"));
    }
    options.priority.map(validate_priority).transpose()?;
    options.name.as_deref().map(normalize_name).transpose()?;
    for candidate in &parsed {
        validate_parsed_account(candidate)?;
        candidate.priority.map(validate_priority).transpose()?;
        if let Some(name) = candidate.name.as_deref() {
            normalize_name(name)?;
        }
        if let Some(source_id) = candidate.source_id.as_deref() {
            validate_source_id(source_id)?;
        }
    }
    Ok(PreparedAccountImport {
        parsed,
        options,
        default_source,
    })
}

fn preview_import_items(
    store: &AccountStore,
    prepared: &PreparedAccountImport,
) -> Result<Vec<AccountPreviewItem>, String> {
    let mut working = store.clone();
    let source = prepared.options.source.unwrap_or(prepared.default_source);
    let explicit_name = prepared
        .options
        .name
        .as_deref()
        .map(normalize_name)
        .transpose()?;
    let apply_explicit_name = prepared.parsed.len() == 1;
    let mut items = Vec::with_capacity(prepared.parsed.len());
    for (index, candidate) in prepared.parsed.iter().enumerate() {
        let proposed_name = candidate_preview_name(
            candidate,
            apply_explicit_name
                .then_some(explicit_name.as_deref())
                .flatten(),
        )?;
        let (action, name) = match matching_account_index(&working.accounts, candidate) {
            Ok(Some(account_index)) => {
                let account = &mut working.accounts[account_index];
                account.credential = candidate.credential.clone();
                if candidate.source_id.is_some() {
                    account.source_id = candidate.source_id.clone();
                }
                (AccountPreviewAction::Update, account.name.clone())
            }
            Ok(None) if working.accounts.len() < MAX_ACCOUNTS => {
                working.accounts.push(StoredAccount {
                    id: Uuid::new_v4().to_string(),
                    tool: candidate.tool,
                    name: proposed_name.clone(),
                    enabled: prepared
                        .options
                        .enabled
                        .or(candidate.enabled)
                        .unwrap_or(true),
                    priority: prepared
                        .options
                        .priority
                        .or(candidate.priority)
                        .unwrap_or(DEFAULT_ACCOUNT_PRIORITY),
                    source,
                    created_at_ms: now_ms(),
                    updated_at_ms: now_ms(),
                    source_id: candidate.source_id.clone(),
                    credential: candidate.credential.clone(),
                });
                (AccountPreviewAction::New, proposed_name)
            }
            Ok(None) | Err(_) => (AccountPreviewAction::Conflict, proposed_name),
        };
        items.push(AccountPreviewItem {
            item_id: format!("item-{}", index + 1),
            tool: candidate.tool,
            auth_kind: candidate.credential.kind(),
            name,
            source,
            action,
        });
    }
    Ok(items)
}

fn candidate_preview_name(
    candidate: &ParsedAccount,
    explicit_name: Option<&str>,
) -> Result<String, String> {
    let explicit = explicit_name
        .map(normalize_name)
        .transpose()?
        .filter(|name| !candidate.credential.contains_secret(name));
    let candidate_name = candidate
        .name
        .as_deref()
        .map(normalize_name)
        .transpose()?
        .filter(|name| !candidate.credential.contains_secret(name));
    Ok(explicit
        .or(candidate_name)
        .unwrap_or_else(|| default_account_name(candidate.tool, candidate.credential.kind())))
}

fn parsed_from_stored(account: &StoredAccount) -> ParsedAccount {
    ParsedAccount {
        tool: account.tool,
        name: Some(account.name.clone()),
        priority: Some(account.priority),
        enabled: Some(account.enabled),
        source_id: account.source_id.clone(),
        credential: account.credential.clone(),
    }
}

fn preview_restore_items(
    current: &AccountStore,
    backup: &AccountStore,
    mode: RestoreMode,
) -> Result<(Vec<AccountPreviewItem>, usize), String> {
    let mut matched_current_ids = HashSet::new();
    let mut items = Vec::with_capacity(backup.accounts.len());
    for (index, account) in backup.accounts.iter().enumerate() {
        let candidate = parsed_from_stored(account);
        let action = match matching_account_index(&current.accounts, &candidate) {
            Ok(Some(current_index)) => {
                matched_current_ids.insert(current.accounts[current_index].id.clone());
                AccountPreviewAction::Update
            }
            Ok(None) => AccountPreviewAction::New,
            Err(_) => AccountPreviewAction::Conflict,
        };
        items.push(AccountPreviewItem {
            item_id: format!("item-{}", index + 1),
            tool: account.tool,
            auth_kind: account.credential.kind(),
            name: account.name.clone(),
            source: account.source,
            action,
        });
    }
    let remove_count = if mode == RestoreMode::Replace {
        current
            .accounts
            .iter()
            .filter(|account| !matched_current_ids.contains(&account.id))
            .count()
    } else {
        0
    };
    Ok((items, remove_count))
}

fn build_restored_store(
    current: &AccountStore,
    backup: AccountStore,
    mode: RestoreMode,
) -> Result<(AccountStore, usize, usize, usize, Vec<String>), String> {
    if mode == RestoreMode::Replace {
        let (_, remove_count) = preview_restore_items(current, &backup, mode)?;
        let mut imported = 0usize;
        let mut updated = 0usize;
        for account in &backup.accounts {
            let candidate = parsed_from_stored(account);
            match matching_account_index(&current.accounts, &candidate)? {
                Some(_) => updated += 1,
                None => imported += 1,
            }
        }
        let affected_ids = backup
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        return Ok((backup, imported, updated, remove_count, affected_ids));
    }

    let mut proposed = current.clone();
    let mut imported = 0usize;
    let mut updated = 0usize;
    let mut affected_ids = Vec::new();
    let current_ms = now_ms();
    for backup_account in backup.accounts {
        let candidate = parsed_from_stored(&backup_account);
        let id = if let Some(index) = matching_account_index(&proposed.accounts, &candidate)? {
            let local = &mut proposed.accounts[index];
            local.credential = backup_account.credential;
            if backup_account.source_id.is_some() {
                local.source_id = backup_account.source_id;
            }
            local.updated_at_ms = current_ms;
            updated += 1;
            local.id.clone()
        } else {
            if proposed.accounts.len() >= MAX_ACCOUNTS {
                return Err(format!("账号数量不能超过 {MAX_ACCOUNTS}"));
            }
            let mut added = backup_account;
            added.id = Uuid::new_v4().to_string();
            added.created_at_ms = current_ms;
            added.updated_at_ms = current_ms;
            let id = added.id.clone();
            proposed.accounts.push(added);
            imported += 1;
            id
        };
        if !affected_ids.contains(&id) {
            affected_ids.push(id);
        }
    }
    Ok((proposed, imported, updated, 0, affected_ids))
}

fn parse_import_content(
    content: &str,
    tool_hint: Option<ToolKind>,
) -> Result<Vec<ParsedAccount>, String> {
    if content.len() > MAX_IMPORT_BYTES {
        return Err("导入内容过大".to_string());
    }
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("导入内容不能为空".to_string());
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let mut accounts = Vec::new();
        collect_json_accounts(&value, tool_hint, 0, &mut accounts)?;
        if accounts.is_empty() {
            return Err("JSON 中未找到支持的 Claude 或 Codex 凭证".to_string());
        }
        return Ok(accounts);
    }

    let lines = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut accounts = Vec::with_capacity(lines.len());
    for (index, line) in lines.into_iter().enumerate() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            collect_json_accounts(&value, tool_hint, 0, &mut accounts)
                .map_err(|error| format!("第 {} 行无法导入: {error}", index + 1))?;
        } else {
            accounts.push(
                parse_raw_credential(line, tool_hint)
                    .map_err(|error| format!("第 {} 行无法导入: {error}", index + 1))?,
            );
        }
    }
    if accounts.is_empty() {
        return Err("导入内容不能为空".to_string());
    }
    Ok(accounts)
}

fn collect_json_accounts(
    value: &Value,
    tool_hint: Option<ToolKind>,
    depth: usize,
    accounts: &mut Vec<ParsedAccount>,
) -> Result<(), String> {
    if depth > 6 {
        return Err("JSON 账号包装层级过深".to_string());
    }
    if let Some(platforms) = cockpit_platforms(value) {
        let mut dispatched = false;
        for (platform, payload) in platforms {
            let Some(platform_tool) = cockpit_platform_tool(platform) else {
                continue;
            };
            dispatched = true;
            collect_json_accounts(payload, Some(platform_tool), depth + 1, accounts)?;
        }
        if dispatched {
            return Ok(());
        }
    }
    reject_non_official_provider_base_urls(value, tool_hint)?;
    match value {
        Value::Array(items) => {
            for item in items {
                collect_json_accounts(item, tool_hint, depth + 1, accounts)?;
            }
        }
        Value::Object(object) => {
            if let Some(account) = parse_json_account(value, tool_hint)? {
                accounts.push(account);
                return Ok(());
            }
            let mut visited_wrapper = false;
            for key in [
                "accounts",
                "items",
                "profiles",
                "data",
                "exported_data",
                "exportedData",
                "account",
                "credential",
                "credentials",
                "auth",
                "session",
                "session_json",
            ] {
                let Some(nested) = object.get(key) else {
                    continue;
                };
                if matches!(nested, Value::Array(_) | Value::Object(_)) {
                    visited_wrapper = true;
                    collect_json_accounts(nested, tool_hint, depth + 1, accounts)?;
                } else if let Some(raw) = nested.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                        visited_wrapper = true;
                        collect_json_accounts(&parsed, tool_hint, depth + 1, accounts)?;
                    }
                }
            }

            if !visited_wrapper && depth == 0 {
                return Err("JSON 中未找到支持的账号结构".to_string());
            }
        }
        Value::String(raw) => accounts.push(parse_raw_credential(raw, tool_hint)?),
        _ => {}
    }
    Ok(())
}

fn cockpit_platforms(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value
        .get("platforms")
        .and_then(Value::as_object)
        .or_else(|| {
            value
                .get("accounts")
                .and_then(|accounts| accounts.get("platforms"))
                .and_then(Value::as_object)
        })
}

fn cockpit_platform_tool(platform: &str) -> Option<ToolKind> {
    let normalized = platform
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    match normalized.as_str() {
        "claudemanager" => Some(ToolKind::Claude),
        "codex" => Some(ToolKind::Codex),
        _ => None,
    }
}

fn parse_json_account(
    value: &Value,
    tool_hint: Option<ToolKind>,
) -> Result<Option<ParsedAccount>, String> {
    reject_non_official_provider_base_urls(value, tool_hint)?;
    let object = match value.as_object() {
        Some(object) => object,
        None => return Ok(None),
    };
    let explicit_tool = infer_explicit_tool(value);
    let auth_mode = first_string(
        value,
        &[
            &["auth_mode"],
            &["authMode"],
            &["openai_auth_mode"],
            &["openaiAuthMode"],
            &["credential", "auth_mode"],
            &["credential", "authMode"],
            &["credential", "kind"],
            &["credential", "authKind"],
        ],
    )
    .map(|mode| mode.to_ascii_lowercase());
    let name = extract_name(value);
    let priority = first_i64(value, &[&["priority"]]).and_then(|value| i32::try_from(value).ok());
    let enabled = first_bool(value, &[&["enabled"]]);
    let prefer_api_key = auth_mode
        .as_deref()
        .is_some_and(|mode| matches!(mode, "api_key" | "apikey" | "api-key"));
    let explicit_oauth = auth_mode.as_deref().is_some_and(is_oauth_auth_mode);
    let credential_secret = first_string(value, &[&["credential", "secret"]]);
    let credential_secret_is_oauth = !prefer_api_key
        && credential_secret.as_deref().is_some_and(|secret| {
            explicit_oauth
                || credential_has_oauth_fields(value)
                || secret.starts_with("sk-ant-oat")
                || secret.starts_with("sk-ant-sid")
                || secret.starts_with("at-")
                || (explicit_tool.or(tool_hint) == Some(ToolKind::Codex)
                    && decode_jwt_payload(secret).is_some())
                || infer_oauth_tool(value, secret).is_some()
        });
    let generic_api_key = first_string(
        value,
        &[
            &["api_key"],
            &["apiKey"],
            &["credential", "api_key"],
            &["credential", "apiKey"],
        ],
    )
    .or_else(|| {
        (!credential_secret_is_oauth)
            .then(|| credential_secret.clone())
            .flatten()
    });

    let claude_api_key = first_string(
        value,
        &[
            &["ANTHROPIC_API_KEY"],
            &["anthropicApiKey"],
            &["env", "ANTHROPIC_API_KEY"],
            &["claude_credentials_raw", "apiKey"],
            &["claudeCredentialsRaw", "apiKey"],
        ],
    )
    .or_else(|| {
        (explicit_tool == Some(ToolKind::Claude)
            || tool_hint == Some(ToolKind::Claude)
            || object.contains_key("claude_credentials_raw")
            || object.contains_key("claudeCredentialsRaw"))
        .then(|| generic_api_key.clone())
        .flatten()
    });
    let codex_api_key = first_string(
        value,
        &[
            &["OPENAI_API_KEY"],
            &["openai_api_key"],
            &["openaiApiKey"],
            &["env", "OPENAI_API_KEY"],
        ],
    )
    .or_else(|| {
        (explicit_tool == Some(ToolKind::Codex) || tool_hint == Some(ToolKind::Codex))
            .then(|| generic_api_key.clone())
            .flatten()
    });

    // Cockpit exports may omit a provider marker. Generic credential secrets
    // are API keys unless an explicit OAuth mode or OAuth-shaped token says
    // otherwise, then key prefixes select the provider when possible.
    let (claude_api_key, codex_api_key) = if claude_api_key.is_none() && codex_api_key.is_none() {
        match infer_api_key_tool(explicit_tool.or(tool_hint), generic_api_key.as_deref()) {
            Some(ToolKind::Claude) => (generic_api_key, None),
            Some(ToolKind::Codex) => (None, generic_api_key),
            None => (None, None),
        }
    } else {
        (claude_api_key, codex_api_key)
    };

    if prefer_api_key {
        if let Some((tool, secret)) = select_api_key(
            explicit_tool.or(tool_hint),
            claude_api_key.clone(),
            codex_api_key.clone(),
        )? {
            return Ok(Some(parsed_api_key(tool, name, priority, enabled, secret)?));
        }
    }

    let claude_access_token = first_string(
        value,
        &[
            &["claudeAiOauth", "accessToken"],
            &["claudeAiOauth", "access_token"],
            &["claude_credentials_raw", "claudeAiOauth", "accessToken"],
            &["claudeCredentialsRaw", "claudeAiOauth", "accessToken"],
            &["credentials", "claudeAiOauth", "accessToken"],
            &["env", "CLAUDE_CODE_OAUTH_TOKEN"],
            &["env", "ANTHROPIC_AUTH_TOKEN"],
        ],
    );
    if let Some(access_token) = claude_access_token {
        let refresh_token = first_string(
            value,
            &[
                &["claudeAiOauth", "refreshToken"],
                &["claudeAiOauth", "refresh_token"],
                &["claude_credentials_raw", "claudeAiOauth", "refreshToken"],
                &["claudeCredentialsRaw", "claudeAiOauth", "refreshToken"],
                &["credentials", "claudeAiOauth", "refreshToken"],
            ],
        );
        let expires_at_ms = first_i64(
            value,
            &[
                &["claudeAiOauth", "expiresAt"],
                &["claudeAiOauth", "expires_at"],
                &["claude_credentials_raw", "claudeAiOauth", "expiresAt"],
                &["claudeCredentialsRaw", "claudeAiOauth", "expiresAt"],
                &["credentials", "claudeAiOauth", "expiresAt"],
            ],
        )
        .map(normalize_timestamp_ms);
        return Ok(Some(parsed_oauth(
            ToolKind::Claude,
            name,
            priority,
            enabled,
            access_token,
            refresh_token,
            claude_oauth_account_id(value),
            expires_at_ms,
        )?));
    }

    let codex_access_token = first_string(
        value,
        &[
            &["tokens", "access_token"],
            &["tokens", "accessToken"],
            &["tokens", "personal_access_token"],
            &["tokens", "personalAccessToken"],
            &["tokens", "at_token"],
            &["tokens", "atToken"],
            &["credentials", "access_token"],
            &["credentials", "accessToken"],
            &["credentials", "personal_access_token"],
            &["credentials", "personalAccessToken"],
            &["credentials", "at_token"],
            &["credentials", "atToken"],
            &["personal_access_token"],
            &["personalAccessToken"],
            &["at_token"],
            &["atToken"],
        ],
    );
    if let Some(access_token) = codex_access_token {
        return Ok(Some(parsed_codex_oauth(
            value,
            name,
            priority,
            enabled,
            access_token,
        )?));
    }

    if let Some((tool, secret)) =
        select_api_key(explicit_tool.or(tool_hint), claude_api_key, codex_api_key)?
    {
        return Ok(Some(parsed_api_key(tool, name, priority, enabled, secret)?));
    }

    let generic_access_token = first_string(
        value,
        &[
            &["access_token"],
            &["accessToken"],
            &["credential", "accessToken"],
        ],
    )
    .or_else(|| {
        credential_secret_is_oauth
            .then(|| credential_secret.clone())
            .flatten()
    });
    if let Some(access_token) = generic_access_token {
        let inferred_tool = explicit_tool
            .or_else(|| infer_oauth_tool(value, &access_token))
            .or(tool_hint)
            .ok_or_else(|| {
                "无法判断 OAuth Token 属于 Claude 还是 Codex，请选择账号类型".to_string()
            })?;
        if inferred_tool == ToolKind::Codex {
            return Ok(Some(parsed_codex_oauth(
                value,
                name,
                priority,
                enabled,
                access_token,
            )?));
        }
        let refresh_token = first_string(
            value,
            &[
                &["refresh_token"],
                &["refreshToken"],
                &["credential", "refreshToken"],
            ],
        );
        let expires_at_ms = first_i64(
            value,
            &[
                &["expires_at"],
                &["expiresAt"],
                &["expires_at_ms"],
                &["expiresAtMs"],
            ],
        )
        .map(normalize_timestamp_ms);
        return Ok(Some(parsed_oauth(
            ToolKind::Claude,
            name,
            priority,
            enabled,
            access_token,
            refresh_token,
            claude_oauth_account_id(value),
            expires_at_ms,
        )?));
    }

    Ok(None)
}

pub(crate) fn codex_local_config_allows_official_api(home: &Path) -> bool {
    let path = home.join(".codex/config.toml");
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    codex_config_allows_official_api(&content)
}

fn codex_config_allows_official_api(content: &str) -> bool {
    let mut top_level = true;
    for raw_line in content.lines() {
        let Some(line) = toml_without_comment(raw_line) else {
            return false;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            top_level = false;
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key
            .trim()
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches(['"', '\'']);
        let normalized_key = key
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>();
        let inline_value_key = raw_value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>();
        let is_base_url_key = matches!(
            normalized_key.as_str(),
            "baseurl" | "openaibaseurl" | "openaiapibase"
        );
        if !is_base_url_key && inline_value_key.contains("baseurl") {
            return false;
        }
        if is_base_url_key {
            let Some(base_url) = parse_toml_string(raw_value) else {
                return false;
            };
            if !is_official_provider_base_url(&base_url, Some(ToolKind::Codex)) {
                return false;
            }
        } else if top_level && normalized_key == "modelprovider" {
            let Some(provider) = parse_toml_string(raw_value) else {
                return false;
            };
            if !provider.trim().eq_ignore_ascii_case("openai") {
                return false;
            }
        }
    }
    true
}

fn toml_without_comment(line: &str) -> Option<&str> {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(active) if character == active => quote = None,
            Some(_) => {}
            None if matches!(character, '"' | '\'') => quote = Some(character),
            None if character == '#' => return Some(&line[..index]),
            None => {}
        }
    }
    quote.is_none().then_some(line)
}

fn parse_toml_string(value: &str) -> Option<String> {
    let value = toml_without_comment(value)?.trim();
    if value.starts_with('"') && value.ends_with('"') {
        serde_json::from_str(value).ok()
    } else if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        Some(value[1..value.len() - 1].to_string())
    } else {
        None
    }
}

/// Imported credentials are always routed to the hard-coded official API
/// endpoints. Reject exports that carry a provider override so a proxy key
/// cannot be mistaken for an official credential. The scan is recursive to
/// cover common `env`, `credentials`, and provider wrapper shapes.
pub(crate) fn reject_non_official_provider_base_urls(
    value: &Value,
    tool_hint: Option<ToolKind>,
) -> Result<(), String> {
    const MAX_SCAN_DEPTH: usize = 32;
    let mut pending = vec![(value, tool_hint, 0usize)];
    while let Some((current, inherited_tool, depth)) = pending.pop() {
        if depth > MAX_SCAN_DEPTH {
            continue;
        }
        let current_tool = infer_explicit_tool(current).or(inherited_tool);
        match current {
            Value::Object(object) => {
                for (key, nested) in object {
                    if let Some(field_tool) = base_url_field_tool(key) {
                        if let Some(raw_url) = nested.as_str() {
                            let raw_url = raw_url.trim();
                            let inferred_tool = field_tool
                                .or(current_tool)
                                .or_else(|| infer_credential_tool(current));
                            if !raw_url.is_empty()
                                && !is_official_provider_base_url(raw_url, inferred_tool)
                            {
                                return Err("账号包含非官方 API 地址，已拒绝导入".to_string());
                            }
                        }
                    }
                    pending.push((nested, current_tool, depth + 1));
                }
            }
            Value::Array(items) => {
                for nested in items {
                    pending.push((nested, current_tool, depth + 1));
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    Ok(())
}

fn base_url_field_tool(key: &str) -> Option<Option<ToolKind>> {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    match normalized.as_str() {
        "anthropicbaseurl" => Some(Some(ToolKind::Claude)),
        "openaibaseurl" | "openaiapibase" => Some(Some(ToolKind::Codex)),
        // `api_base_url`, `apiBaseUrl`, and provider `baseUrl` are common
        // exports from account managers and need the surrounding tool hint.
        "apibaseurl" | "baseurl" | "inferencegatewaybaseurl" => Some(None),
        _ => None,
    }
}

fn infer_credential_tool(value: &Value) -> Option<ToolKind> {
    let (claude, codex) = credential_tool_signals(value, 0);
    match (claude, codex) {
        (true, false) => Some(ToolKind::Claude),
        (false, true) => Some(ToolKind::Codex),
        _ => None,
    }
}

fn credential_tool_signals(value: &Value, depth: usize) -> (bool, bool) {
    if depth > 16 {
        return (false, false);
    }
    let mut claude = false;
    let mut codex = false;
    match value {
        Value::Object(object) => {
            for (key, nested) in object {
                let normalized = key
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .map(|character| character.to_ascii_lowercase())
                    .collect::<String>();
                match normalized.as_str() {
                    "anthropicapikey"
                    | "anthropicauthtoken"
                    | "claudecodeoauthtoken"
                    | "claudeaioauth"
                    | "claudecredentialsraw"
                    | "claudeconfigraw" => claude = true,
                    "openaapikey" | "tokens" | "accountid" | "personalaccesstoken" | "attoken" => {
                        codex = true
                    }
                    "apikey" | "secret" => {
                        if let Some(raw) = nested.as_str() {
                            match infer_api_key_tool(None, Some(raw.trim())) {
                                Some(ToolKind::Claude) => claude = true,
                                Some(ToolKind::Codex) => codex = true,
                                None => {}
                            }
                        }
                    }
                    "accesstoken" => {
                        if let Some(raw) = nested.as_str() {
                            if raw.starts_with("sk-ant-oat") || raw.starts_with("sk-ant-sid") {
                                claude = true;
                            } else if raw.starts_with("at-")
                                || decode_jwt_payload(raw).as_ref().is_some_and(|payload| {
                                    payload.get("https://api.openai.com/auth").is_some()
                                })
                            {
                                codex = true;
                            }
                        }
                    }
                    _ => {}
                }
                let (nested_claude, nested_codex) = credential_tool_signals(nested, depth + 1);
                claude |= nested_claude;
                codex |= nested_codex;
            }
        }
        Value::Array(items) => {
            for nested in items {
                let (nested_claude, nested_codex) = credential_tool_signals(nested, depth + 1);
                claude |= nested_claude;
                codex |= nested_codex;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    (claude, codex)
}

pub(crate) fn is_official_provider_base_url(raw_url: &str, tool: Option<ToolKind>) -> bool {
    let Ok(url) = url::Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    match tool {
        Some(ToolKind::Claude) => host == "api.anthropic.com",
        Some(ToolKind::Codex) => host == "api.openai.com",
        None => matches!(host.as_str(), "api.anthropic.com" | "api.openai.com"),
    }
}

fn claude_oauth_account_id(value: &Value) -> Option<String> {
    const WRAPPERS: [&str; 6] = [
        "claude_config_raw",
        "claudeConfigRaw",
        "claude_credentials_raw",
        "claudeCredentialsRaw",
        "credentials",
        "config",
    ];
    const ID_FIELDS: [&str; 5] = [
        "accountUuid",
        "accountUUID",
        "account_uuid",
        "account_id",
        "accountId",
    ];

    for field in ID_FIELDS {
        if let Some(account_id) = first_string(value, &[&["oauthAccount", field]]) {
            return Some(account_id);
        }
        for wrapper in WRAPPERS {
            if let Some(account_id) = first_string(value, &[&[wrapper, "oauthAccount", field]]) {
                return Some(account_id);
            }
        }
        if let Some(account_id) = first_string(value, &[&[field]]) {
            return Some(account_id);
        }
        for wrapper in WRAPPERS {
            if let Some(account_id) = first_string(value, &[&[wrapper, field]]) {
                return Some(account_id);
            }
        }
    }
    None
}

pub(crate) fn codex_oauth_expires_at_ms(value: &Value, access_token: &str) -> Option<i64> {
    first_i64(
        value,
        &[
            &["expires_at"],
            &["expiresAt"],
            &["expires_at_ms"],
            &["expiresAtMs"],
            &["tokens", "expires_at"],
            &["tokens", "expiresAt"],
            &["tokens", "expires_at_ms"],
            &["tokens", "expiresAtMs"],
            &["credentials", "expires_at"],
            &["credentials", "expiresAt"],
            &["credentials", "expires_at_ms"],
            &["credentials", "expiresAtMs"],
        ],
    )
    .map(normalize_timestamp_ms)
    .or_else(|| {
        decode_jwt_payload(access_token)
            .and_then(|payload| payload.get("exp").and_then(value_as_i64))
            .map(normalize_timestamp_ms)
    })
}

pub(crate) fn codex_oauth_account_id(value: &Value, access_token: &str) -> Option<String> {
    first_string(
        value,
        &[
            &["tokens", "account_id"],
            &["tokens", "accountId"],
            &["credentials", "account_id"],
            &["credentials", "accountId"],
            &["account_id"],
            &["accountId"],
            &["account", "id"],
            &["credential", "accountId"],
        ],
    )
    .or_else(|| {
        decode_jwt_payload(access_token)
            .as_ref()
            .and_then(codex_account_id_from_jwt)
    })
}

fn parsed_codex_oauth(
    value: &Value,
    name: Option<String>,
    priority: Option<i32>,
    enabled: Option<bool>,
    access_token: String,
) -> Result<ParsedAccount, String> {
    let jwt = decode_jwt_payload(&access_token);
    let refresh_token = first_string(
        value,
        &[
            &["tokens", "refresh_token"],
            &["tokens", "refreshToken"],
            &["credentials", "refresh_token"],
            &["credentials", "refreshToken"],
            &["refresh_token"],
            &["refreshToken"],
            &["credential", "refreshToken"],
        ],
    );
    let account_id = codex_oauth_account_id(value, &access_token);
    let expires_at_ms = codex_oauth_expires_at_ms(value, &access_token);
    let name = name.or_else(|| jwt.as_ref().and_then(email_from_jwt));
    parsed_oauth(
        ToolKind::Codex,
        name,
        priority,
        enabled,
        access_token,
        refresh_token,
        account_id,
        expires_at_ms,
    )
}

fn parsed_api_key(
    tool: ToolKind,
    name: Option<String>,
    priority: Option<i32>,
    enabled: Option<bool>,
    secret: String,
) -> Result<ParsedAccount, String> {
    validate_secret(&secret)?;
    Ok(ParsedAccount {
        tool,
        name,
        priority,
        enabled,
        source_id: None,
        credential: StoredCredential::ApiKey { secret },
    })
}

#[allow(clippy::too_many_arguments)]
fn parsed_oauth(
    tool: ToolKind,
    name: Option<String>,
    priority: Option<i32>,
    enabled: Option<bool>,
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at_ms: Option<i64>,
) -> Result<ParsedAccount, String> {
    validate_secret(&access_token)?;
    if let Some(refresh_token) = refresh_token.as_deref() {
        validate_secret(refresh_token)?;
    }
    Ok(ParsedAccount {
        tool,
        name,
        priority,
        enabled,
        source_id: None,
        credential: StoredCredential::OAuth {
            access_token,
            refresh_token,
            account_id: normalize_optional(account_id),
            expires_at_ms,
        },
    })
}

fn parse_raw_credential(raw: &str, tool_hint: Option<ToolKind>) -> Result<ParsedAccount, String> {
    let secret = raw.trim();
    validate_secret(secret)?;
    let jwt_payload = decode_jwt_payload(secret);
    let is_jwt = jwt_payload.is_some();
    let is_openai_jwt = jwt_payload
        .as_ref()
        .is_some_and(|payload| payload.get("https://api.openai.com/auth").is_some());
    let (tool, oauth) = match tool_hint {
        Some(ToolKind::Claude) => (
            ToolKind::Claude,
            secret.starts_with("sk-ant-oat") || secret.starts_with("sk-ant-sid"),
        ),
        Some(ToolKind::Codex) => (ToolKind::Codex, secret.starts_with("at-") || is_jwt),
        None if secret.starts_with("sk-ant-oat") || secret.starts_with("sk-ant-sid") => {
            (ToolKind::Claude, true)
        }
        None if is_openai_jwt => (ToolKind::Codex, true),
        None if secret.starts_with("sk-ant-") => (ToolKind::Claude, false),
        None if secret.starts_with("sk-") => (ToolKind::Codex, false),
        None if secret.starts_with("at-") => (ToolKind::Codex, true),
        None => return Err("无法判断原始凭证属于 Claude 还是 Codex，请选择账号类型".to_string()),
    };

    if oauth {
        parsed_oauth(
            tool,
            None,
            None,
            None,
            secret.to_string(),
            None,
            None,
            jwt_payload
                .and_then(|payload| payload.get("exp").and_then(value_as_i64))
                .map(normalize_timestamp_ms),
        )
    } else {
        parsed_api_key(tool, None, None, None, secret.to_string())
    }
}

fn select_api_key(
    preferred_tool: Option<ToolKind>,
    claude: Option<String>,
    codex: Option<String>,
) -> Result<Option<(ToolKind, String)>, String> {
    match (preferred_tool, claude, codex) {
        (Some(ToolKind::Claude), Some(secret), _) => Ok(Some((ToolKind::Claude, secret))),
        (Some(ToolKind::Codex), _, Some(secret)) => Ok(Some((ToolKind::Codex, secret))),
        (Some(ToolKind::Claude), None, Some(_)) => {
            Err("所选 Claude 类型与 OPENAI_API_KEY 不匹配".to_string())
        }
        (Some(ToolKind::Codex), Some(_), None) => {
            Err("所选 Codex 类型与 ANTHROPIC_API_KEY 不匹配".to_string())
        }
        (None, Some(secret), None) => Ok(Some((ToolKind::Claude, secret))),
        (None, None, Some(secret)) => Ok(Some((ToolKind::Codex, secret))),
        (None, Some(_), Some(_)) => {
            Err("同一账号对象同时包含 Claude 和 Codex API Key，请分开导入".to_string())
        }
        (_, None, None) => Ok(None),
    }
}

fn infer_api_key_tool(preferred_tool: Option<ToolKind>, secret: Option<&str>) -> Option<ToolKind> {
    preferred_tool.or_else(|| {
        let secret = secret?;
        if secret.starts_with("sk-ant-") {
            Some(ToolKind::Claude)
        } else if secret.starts_with("sk-") {
            Some(ToolKind::Codex)
        } else {
            None
        }
    })
}

fn is_oauth_auth_mode(mode: &str) -> bool {
    let normalized = mode
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "oauth"
            | "oauth2"
            | "oauthtoken"
            | "desktopoauth"
            | "oidc"
            | "accesstoken"
            | "personalaccesstoken"
            | "setuptoken"
            | "bearer"
            | "bearertoken"
    )
}

fn credential_has_oauth_fields(value: &Value) -> bool {
    first_string(
        value,
        &[
            &["credential", "access_token"],
            &["credential", "accessToken"],
            &["credential", "refresh_token"],
            &["credential", "refreshToken"],
            &["credential", "account_id"],
            &["credential", "accountId"],
        ],
    )
    .is_some()
        || first_i64(
            value,
            &[
                &["credential", "expires_at"],
                &["credential", "expiresAt"],
                &["credential", "expires_at_ms"],
                &["credential", "expiresAtMs"],
            ],
        )
        .is_some()
}

fn infer_explicit_tool(value: &Value) -> Option<ToolKind> {
    let marker = first_string(
        value,
        &[&["tool"], &["provider"], &["platform"], &["service"]],
    )?
    .to_ascii_lowercase();
    if marker.contains("claude") || marker.contains("anthropic") {
        Some(ToolKind::Claude)
    } else if marker.contains("codex") || marker.contains("openai") || marker.contains("chatgpt") {
        Some(ToolKind::Codex)
    } else {
        None
    }
}

fn infer_oauth_tool(value: &Value, access_token: &str) -> Option<ToolKind> {
    if value.pointer("/claudeAiOauth").is_some()
        || value.pointer("/oauthAccount").is_some()
        || access_token.starts_with("sk-ant-oat")
        || access_token.starts_with("sk-ant-sid")
    {
        return Some(ToolKind::Claude);
    }
    if value.pointer("/tokens").is_some()
        || value.pointer("/account_id").is_some()
        || value.pointer("/accountId").is_some()
        || access_token.starts_with("at-")
        || decode_jwt_payload(access_token)
            .as_ref()
            .is_some_and(|payload| payload.get("https://api.openai.com/auth").is_some())
    {
        return Some(ToolKind::Codex);
    }
    None
}

fn extract_name(value: &Value) -> Option<String> {
    first_string(
        value,
        &[
            &["name"],
            &["display_name"],
            &["displayName"],
            &["account_name"],
            &["accountName"],
            &["email"],
            &["user", "email"],
            &["account", "email"],
            &["oauthAccount", "emailAddress"],
            &["claude_config_raw", "oauthAccount", "emailAddress"],
            &["claudeConfigRaw", "oauthAccount", "emailAddress"],
        ],
    )
}

fn first_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        value_at_path(value, path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn first_i64(value: &Value, paths: &[&[&str]]) -> Option<i64> {
    paths
        .iter()
        .find_map(|path| value_at_path(value, path).and_then(value_as_i64))
}

fn first_bool(value: &Value, paths: &[&[&str]]) -> Option<bool> {
    paths
        .iter()
        .find_map(|path| value_at_path(value, path).and_then(Value::as_bool))
}

fn value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| current.get(*segment))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn codex_account_id_from_jwt(payload: &Value) -> Option<String> {
    first_string(
        payload,
        &[
            &["https://api.openai.com/auth", "chatgpt_account_id"],
            &["https://api.openai.com/auth", "account_id"],
            &["account_id"],
        ],
    )
}

fn email_from_jwt(payload: &Value) -> Option<String> {
    first_string(
        payload,
        &[&["email"], &["https://api.openai.com/profile", "email"]],
    )
}

fn normalize_timestamp_ms(timestamp: i64) -> i64 {
    if timestamp.abs() < 10_000_000_000 {
        timestamp.saturating_mul(1_000)
    } else {
        timestamp
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_parsed_account(account: &ParsedAccount) -> Result<(), String> {
    validate_secret(account.credential.secret())?;
    if let StoredCredential::OAuth {
        refresh_token: Some(refresh_token),
        ..
    } = &account.credential
    {
        validate_secret(refresh_token)?;
    }
    Ok(())
}

fn validate_secret(secret: &str) -> Result<(), String> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err("凭证不能为空".to_string());
    }
    if trimmed.len() > MAX_SECRET_BYTES {
        return Err("凭证长度超出限制".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("凭证不能包含空白字符".to_string());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Err("凭证不能是 URL".to_string());
    }
    Ok(())
}

fn normalize_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("账号名称不能为空".to_string());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("账号名称不能超过 {MAX_NAME_CHARS} 个字符"));
    }
    if name.chars().any(char::is_control) {
        return Err("账号名称不能包含控制字符".to_string());
    }
    Ok(name.to_string())
}

fn validate_priority(priority: i32) -> Result<i32, String> {
    if !(0..=MAX_PRIORITY).contains(&priority) {
        return Err(format!("账号优先级必须在 0 到 {MAX_PRIORITY} 之间"));
    }
    Ok(priority)
}

fn default_account_name(tool: ToolKind, auth_kind: AccountAuthKind) -> String {
    match (tool, auth_kind) {
        (ToolKind::Claude, AccountAuthKind::ApiKey) => "Claude API Key".to_string(),
        (ToolKind::Claude, AccountAuthKind::OAuth) => "Claude OAuth".to_string(),
        (ToolKind::Codex, AccountAuthKind::ApiKey) => "Codex API Key".to_string(),
        (ToolKind::Codex, AccountAuthKind::OAuth) => "Codex OAuth".to_string(),
    }
}

fn sort_summaries(summaries: &mut [AccountSummary]) {
    summaries.sort_by(|left, right| {
        tool_rank(left.tool)
            .cmp(&tool_rank(right.tool))
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn tool_rank(tool: ToolKind) -> u8 {
    match tool {
        ToolKind::Claude => 0,
        ToolKind::Codex => 1,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn load_store(path: &Path) -> Result<AccountStore, String> {
    let backup_path = account_store_backup_path(path);
    let primary_exists = path_is_present(path, "账号存储")?;
    let backup_exists = path_is_present(&backup_path, "账号存储备份")?;

    let mut store = if !primary_exists {
        if !backup_exists {
            return Ok(AccountStore::default());
        }
        let store = read_store_file(&backup_path, path)?;
        recover_store_backup(path, &backup_path, false)?;
        store
    } else {
        match read_store_file(path, path) {
            Ok(store) => {
                // A backup can remain when the process exits after replacing the
                // destination but before cleanup. The primary has already passed
                // authentication, so the stale copy can be discarded safely.
                if backup_exists {
                    let _ = discard_store_backup(&backup_path);
                }
                store
            }
            Err(primary_error) if backup_exists => {
                // A crash can leave a partially written primary alongside the
                // last authenticated backup. Validate the backup before replacing
                // the damaged file and keep the original error if it is unusable.
                let backup_store = match read_store_file(&backup_path, path) {
                    Ok(store) => store,
                    Err(_) => return Err(primary_error),
                };
                recover_store_backup(path, &backup_path, true)?;
                backup_store
            }
            Err(error) => return Err(error),
        }
    };

    if store.version == LEGACY_STORE_VERSION {
        migrate_store_v1(path, &mut store)?;
    }
    Ok(store)
}

fn account_store_backup_path(store_path: &Path) -> PathBuf {
    let mut backup_name = store_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "accounts.json".into());
    backup_name.push(".bak");
    store_path.with_file_name(backup_name)
}

fn account_store_migration_backup_path(store_path: &Path) -> PathBuf {
    let mut backup_name = store_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "accounts.json".into());
    backup_name.push(".v1-authenticated.bak");
    store_path.with_file_name(backup_name)
}

fn migrate_store_v1(path: &Path, store: &mut AccountStore) -> Result<(), String> {
    let original = fs::read(path).map_err(|error| format!("无法读取待迁移账号存储: {error}"))?;
    let migration_backup = account_store_migration_backup_path(path);
    if !path_is_present(&migration_backup, "账号迁移备份")? {
        atomic_write_secure(&migration_backup, &original)?;
    }
    store.version = STORE_VERSION;
    if let Err(error) = save_store(path, store) {
        let _ = atomic_write_secure(path, &original);
        store.version = LEGACY_STORE_VERSION;
        return Err(format!("账号存储 v2 迁移失败，已保留原版本: {error}"));
    }
    Ok(())
}

fn path_is_present(path: &Path, label: &str) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!("{label}不能是符号链接"));
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("无法检查{label}: {error}")),
    }
}

fn read_store_file(file_path: &Path, key_path: &Path) -> Result<AccountStore, String> {
    let metadata =
        fs::metadata(file_path).map_err(|error| format!("无法读取账号存储元数据: {error}"))?;
    if metadata.len() > MAX_STORE_BYTES {
        return Err("账号存储文件异常过大，已拒绝读取".to_string());
    }
    let bytes = fs::read(file_path).map_err(|error| format!("无法读取账号存储: {error}"))?;
    let envelope: EncryptedStoreEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| format!("账号存储格式已损坏: {error}"))?;
    let plaintext = decrypt_store(key_path, envelope)?;
    let store: AccountStore = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("账号存储解密内容已损坏: {error}"))?;
    if !matches!(store.version, LEGACY_STORE_VERSION | STORE_VERSION) {
        return Err(format!("不支持的账号存储版本: {}", store.version));
    }
    validate_store(&store)?;
    secure_file(file_path)?;
    Ok(store)
}

fn recover_store_backup(
    store_path: &Path,
    backup_path: &Path,
    replace_existing: bool,
) -> Result<(), String> {
    let parent = store_path
        .parent()
        .ok_or_else(|| "账号存储路径无效".to_string())?;
    let quarantine_path = if replace_existing {
        let file_name = store_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("accounts.json");
        let path = parent.join(format!(".{file_name}.corrupt.{}.tmp", Uuid::new_v4()));
        fs::rename(store_path, &path)
            .map_err(|_| "无法隔离损坏的账号存储，已保留现有文件".to_string())?;
        Some(path)
    } else {
        None
    };

    if let Err(error) = fs::rename(backup_path, store_path) {
        if let Some(quarantine_path) = quarantine_path.as_ref() {
            let _ = fs::rename(quarantine_path, store_path);
        }
        return Err(format!("无法恢复账号存储备份: {error}"));
    }

    let result = (|| -> Result<(), String> {
        secure_file(store_path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if let Some(quarantine_path) = quarantine_path {
        let _ = fs::remove_file(quarantine_path);
    }
    result
}

fn discard_store_backup(backup_path: &Path) -> Result<(), String> {
    if !path_is_present(backup_path, "账号存储备份")? {
        return Ok(());
    }
    fs::remove_file(backup_path).map_err(|_| "无法清理账号存储备份".to_string())
}

fn validate_store(store: &AccountStore) -> Result<(), String> {
    if !matches!(store.version, LEGACY_STORE_VERSION | STORE_VERSION) {
        return Err(format!("不支持的账号存储版本: {}", store.version));
    }
    if store.accounts.len() > MAX_ACCOUNTS {
        return Err("账号存储中的账号数量超出限制".to_string());
    }
    let mut ids = HashSet::with_capacity(store.accounts.len());
    for account in &store.accounts {
        if account.id.trim().is_empty() || !ids.insert(account.id.as_str()) {
            return Err("账号存储包含无效或重复的账号编号".to_string());
        }
        normalize_name(&account.name)?;
        validate_priority(account.priority)?;
        validate_secret(account.credential.secret())?;
        if let StoredCredential::OAuth {
            refresh_token: Some(refresh_token),
            ..
        } = &account.credential
        {
            validate_secret(refresh_token)?;
        }
        if account.credential.contains_secret(&account.name) {
            return Err("账号名称不能包含凭证内容".to_string());
        }
        if let Some(source_id) = account.source_id.as_deref() {
            validate_source_id(source_id)?;
        }
    }
    Ok(())
}

fn validate_source_id(source_id: &str) -> Result<(), String> {
    if source_id.is_empty()
        || source_id.chars().count() > MAX_SOURCE_ID_CHARS
        || !source_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ":._-".contains(character))
    {
        return Err("账号来源标识无效".to_string());
    }
    Ok(())
}

fn save_store(path: &Path, store: &AccountStore) -> Result<(), String> {
    let bytes = encode_store(path, store)?;
    atomic_write_secure(path, &bytes)
}

fn encode_store(key_path: &Path, store: &AccountStore) -> Result<Vec<u8>, String> {
    validate_store(store)?;
    let plaintext =
        serde_json::to_vec(store).map_err(|error| format!("无法编码账号存储: {error}"))?;
    let envelope = encrypt_store(key_path, plaintext)?;
    let mut bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|error| format!("无法编码加密账号存储: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err("账号存储内容过大，无法保存更多账号".to_string());
    }
    Ok(bytes)
}

fn restore_snapshot_path(store_path: &Path) -> PathBuf {
    let mut backup_name = store_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "accounts.json".into());
    backup_name.push(".pre-restore.bak");
    store_path.with_file_name(backup_name)
}

fn write_restore_snapshot(path: &Path, store: &AccountStore) -> Result<(), String> {
    let bytes = encode_store(path, store)?;
    atomic_write_secure(&restore_snapshot_path(path), &bytes)
}

fn rollback_restore_snapshot(path: &Path) -> Result<(), String> {
    let snapshot = restore_snapshot_path(path);
    read_store_file(&snapshot, path).map_err(|error| format!("恢复回滚快照校验失败: {error}"))?;
    let bytes = fs::read(&snapshot).map_err(|error| format!("无法读取恢复回滚快照: {error}"))?;
    atomic_write_secure(path, &bytes).map_err(|error| format!("无法自动回滚替换恢复: {error}"))
}

fn encrypt_store(path: &Path, mut plaintext: Vec<u8>) -> Result<EncryptedStoreEnvelope, String> {
    let key_bytes = read_or_create_account_key(path)?;
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "无法初始化账号存储加密".to_string())?;
    let mut nonce_bytes = [0u8; ACCOUNT_NONCE_BYTES];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "无法生成账号存储加密随机数".to_string())?;
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce_bytes),
        aead::Aad::from(ACCOUNT_STORE_AAD),
        &mut plaintext,
    )
    .map_err(|_| "无法加密账号存储".to_string())?;

    Ok(EncryptedStoreEnvelope {
        version: ENVELOPE_VERSION,
        algorithm: "AES-256-GCM".to_string(),
        nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
        ciphertext: URL_SAFE_NO_PAD.encode(plaintext),
    })
}

fn decrypt_store(path: &Path, envelope: EncryptedStoreEnvelope) -> Result<Vec<u8>, String> {
    if envelope.version != ENVELOPE_VERSION || envelope.algorithm != "AES-256-GCM" {
        return Err("不支持的账号存储加密格式".to_string());
    }
    let nonce_bytes = URL_SAFE_NO_PAD
        .decode(envelope.nonce)
        .ok()
        .and_then(|bytes| <[u8; ACCOUNT_NONCE_BYTES]>::try_from(bytes).ok())
        .ok_or_else(|| "账号存储加密随机数无效".to_string())?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(envelope.ciphertext)
        .map_err(|_| "账号存储密文格式无效".to_string())?;
    let key_bytes = read_account_key(path)?;
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "无法初始化账号存储解密".to_string())?;
    let plaintext_len = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_bytes),
            aead::Aad::from(ACCOUNT_STORE_AAD),
            &mut ciphertext,
        )
        .map_err(|_| "账号存储校验失败，文件可能已损坏或被篡改".to_string())?
        .len();
    ciphertext.truncate(plaintext_len);
    Ok(ciphertext)
}

fn account_key_path(store_path: &Path) -> PathBuf {
    store_path.with_extension("key")
}

fn read_or_create_account_key(store_path: &Path) -> Result<[u8; ACCOUNT_KEY_BYTES], String> {
    let key_path = account_key_path(store_path);
    if key_path.exists() {
        return read_account_key(store_path);
    }

    let mut key = [0u8; ACCOUNT_KEY_BYTES];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| "无法生成账号存储加密密钥".to_string())?;
    atomic_write_secure(&key_path, &key)?;
    Ok(key)
}

fn read_account_key(store_path: &Path) -> Result<[u8; ACCOUNT_KEY_BYTES], String> {
    let key_path = account_key_path(store_path);
    if !key_path.exists() {
        return Err("账号存储加密密钥不存在，无法读取已保存账号".to_string());
    }
    reject_symlink(&key_path, "账号存储加密密钥")?;
    let bytes =
        fs::read(&key_path).map_err(|error| format!("无法读取账号存储加密密钥: {error}"))?;
    let key = <[u8; ACCOUNT_KEY_BYTES]>::try_from(bytes)
        .map_err(|_| "账号存储加密密钥长度无效".to_string())?;
    secure_file(&key_path)?;
    Ok(key)
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map_err(|error| format!("无法检查{label}: {error}"))?
        .file_type()
        .is_symlink()
    {
        return Err(format!("{label}不能是符号链接"));
    }
    Ok(())
}

fn atomic_write_secure(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "账号存储路径无效".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建账号存储目录: {error}"))?;
    secure_directory(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("accounts.json");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| format!("无法创建账号临时存储: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("无法写入账号临时存储: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("无法同步账号临时存储: {error}"))?;
        drop(file);
        secure_file(&temp_path)?;
        replace_file(&temp_path, path)?;
        secure_file(path)?;
        sync_directory(parent)?;
        cleanup_replacement_backup(path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(|error| format!("无法提交账号存储: {error}"))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "账号存储路径无效".to_string())?;
    let backup = account_store_backup_path(destination);
    if path_is_present(&backup, "账号存储备份")? {
        discard_store_backup(&backup)?;
    }
    if !path_is_present(destination, "账号存储")? {
        return fs::rename(source, destination)
            .map_err(|error| format!("无法提交账号存储: {error}"));
    }
    fs::rename(destination, &backup).map_err(|error| format!("无法准备替换账号存储: {error}"))?;
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::rename(&backup, destination);
            Err(format!("无法提交账号存储: {error}"))
        }
    }
}

#[cfg(windows)]
fn cleanup_replacement_backup(path: &Path) -> Result<(), String> {
    // Keep the backup until the new destination has been secured and the
    // directory sync has completed. If the process exits before this point,
    // load_store can recover it on the next launch.
    let backup = account_store_backup_path(path);
    if path_is_present(&backup, "账号存储备份")? {
        discard_store_backup(&backup)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn cleanup_replacement_backup(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("无法保护账号存储: {error}"))
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("无法保护账号存储目录: {error}"))
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("无法同步账号存储目录: {error}"))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use tempfile::TempDir;

    fn pool() -> (TempDir, AccountPool) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = AccountPool::new(temp.path().join("private/accounts.json"));
        (temp, pool)
    }

    fn import_options(tool: ToolKind) -> ImportOptions {
        ImportOptions {
            tool: Some(tool),
            ..ImportOptions::default()
        }
    }

    fn jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload"));
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn imports_raw_keys_and_orders_enabled_candidates_by_priority() {
        let (_temp, pool) = pool();
        let first = pool
            .import_content(
                "sk-ant-api03-first-secret",
                ImportOptions {
                    priority: Some(20),
                    name: Some("secondary".to_string()),
                    ..import_options(ToolKind::Claude)
                },
            )
            .expect("first import");
        let second = pool
            .import_content(
                "sk-ant-api03-second-secret",
                ImportOptions {
                    priority: Some(5),
                    name: Some("primary".to_string()),
                    ..import_options(ToolKind::Claude)
                },
            )
            .expect("second import");
        pool.update(
            &first.accounts[0].id,
            UpdateAccountInput {
                enabled: Some(false),
                ..UpdateAccountInput::default()
            },
        )
        .expect("disable");

        let candidates = pool.candidates(ToolKind::Claude).expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, second.accounts[0].id);
        assert_eq!(candidates[0].priority, 5);
        assert_eq!(
            candidates[0].credential.secret(),
            "sk-ant-api03-second-secret"
        );
        assert_eq!(
            candidates[0].credential.auth_kind(),
            AccountAuthKind::ApiKey
        );
    }

    #[test]
    fn imports_claude_oauth_from_direct_and_cockpit_shapes() {
        let (_temp, pool) = pool();
        let direct = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-access-one",
                "refreshToken": "sk-ant-ort01-refresh-one",
                "expiresAt": 1_900_000_000_000i64
            },
            "oauthAccount": { "emailAddress": "one@example.com" }
        });
        let cockpit = json!({
            "id": "cockpit-id",
            "email": "two@example.com",
            "auth_mode": "oauth",
            "claude_credentials_raw": {
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-access-two",
                    "refreshToken": "sk-ant-ort01-refresh-two",
                    "expiresAt": 1_910_000_000_000i64
                }
            }
        });
        pool.import_content(&direct.to_string(), ImportOptions::default())
            .expect("direct");
        pool.import_content(&cockpit.to_string(), ImportOptions::default())
            .expect("cockpit");

        let candidates = pool.candidates(ToolKind::Claude).expect("candidates");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].credential.auth_kind(), AccountAuthKind::OAuth);
        assert!(candidates.iter().any(|candidate| {
            candidate.name == "one@example.com"
                && candidate.credential.refresh_token() == Some("sk-ant-ort01-refresh-one")
                && candidate.credential.expires_at_ms() == Some(1_900_000_000_000)
        }));
    }

    #[test]
    fn imports_claude_oauth_stable_ids_from_common_wrappers() {
        let payloads = [
            json!({
                "claudeAiOauth": {"accessToken": "sk-ant-oat01-id-one"},
                "oauthAccount": {"accountUuid": "claude-id-one"}
            }),
            json!({
                "claude_credentials_raw": {
                    "claudeAiOauth": {"accessToken": "sk-ant-oat01-id-two"}
                },
                "claude_config_raw": {"oauthAccount": {"accountUUID": "claude-id-two"}}
            }),
            json!({
                "claudeCredentialsRaw": {
                    "claudeAiOauth": {"accessToken": "sk-ant-oat01-id-three"}
                },
                "claudeConfigRaw": {"oauthAccount": {"account_id": "claude-id-three"}}
            }),
            json!({
                "credentials": {"claudeAiOauth": {"accessToken": "sk-ant-oat01-id-four"}},
                "config": {"oauthAccount": {"accountId": "claude-id-four"}}
            }),
        ];

        let expected_ids = [
            "claude-id-one",
            "claude-id-two",
            "claude-id-three",
            "claude-id-four",
        ];
        for (payload, expected_id) in payloads.into_iter().zip(expected_ids) {
            let parsed = parse_import_content(&payload.to_string(), Some(ToolKind::Claude))
                .expect("Claude OAuth wrapper");
            let StoredCredential::OAuth { account_id, .. } = &parsed[0].credential else {
                panic!("expected OAuth credential");
            };
            assert_eq!(account_id.as_deref(), Some(expected_id));
        }
    }

    #[test]
    fn claude_top_level_account_uuid_keeps_record_across_full_token_rotation() {
        let (_temp, pool) = pool();
        let first = pool
            .import_content(
                &json!({
                    "claudeAiOauth": {
                        "accessToken": "sk-ant-oat01-before-rotation",
                        "refreshToken": "sk-ant-ort01-before-rotation"
                    },
                    "account_uuid": "stable-claude-account"
                })
                .to_string(),
                ImportOptions::default(),
            )
            .expect("initial OAuth import");
        let second = pool
            .import_content(
                &json!({
                    "claudeAiOauth": {
                        "accessToken": "sk-ant-oat01-after-rotation",
                        "refreshToken": "sk-ant-ort01-after-rotation"
                    },
                    "account_uuid": "stable-claude-account"
                })
                .to_string(),
                ImportOptions::default(),
            )
            .expect("rotated OAuth import");

        assert_eq!(second.imported, 0);
        assert_eq!(second.updated, 1);
        assert_eq!(second.accounts[0].id, first.accounts[0].id);
        let candidates = pool.candidates(ToolKind::Claude).expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].credential.secret(),
            "sk-ant-oat01-after-rotation"
        );
    }

    #[test]
    fn imports_cockpit_api_key_shape_without_explicit_tool_marker() {
        let (_temp, pool) = pool();
        let payload = json!({
            "auth_mode": "api_key",
            "api_key": "sk-ant-api03-cockpit-only-key",
            "email": "claude-api@example.com"
        });
        let result = pool
            .import_content(&payload.to_string(), ImportOptions::default())
            .expect("cockpit API key");
        assert_eq!(result.imported, 1);
        assert_eq!(result.accounts[0].tool, ToolKind::Claude);
        assert_eq!(result.accounts[0].name, "claude-api@example.com");
    }

    #[test]
    fn generic_credential_secret_defaults_to_api_key_without_oauth_evidence() {
        let cases = [
            (
                json!({"credential": {"secret": "sk-proj-generic-codex"}}),
                ToolKind::Codex,
            ),
            (
                json!({"credential": {"secret": "sk-ant-api03-generic-claude"}}),
                ToolKind::Claude,
            ),
            (
                json!({
                    "tool": "codex",
                    "auth_mode": "api_key",
                    "credential": {"secret": "at-explicit-api-key"}
                }),
                ToolKind::Codex,
            ),
        ];

        for (payload, expected_tool) in cases {
            let parsed = parse_import_content(&payload.to_string(), None)
                .expect("generic credential API key");
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].tool, expected_tool);
            assert_eq!(parsed[0].credential.kind(), AccountAuthKind::ApiKey);
        }
    }

    #[test]
    fn generic_credential_secret_respects_explicit_oauth_evidence() {
        let codex_jwt = jwt(json!({"sub": "generic-codex-oauth"}));
        let cases = [
            json!({
                "tool": "codex",
                "auth_mode": "oauth",
                "credential": {"secret": "at-generic-oauth"}
            }),
            json!({
                "tool": "claude",
                "credential": {
                    "authKind": "oauth",
                    "secret": "sk-ant-oat01-generic-oauth"
                }
            }),
            json!({
                "tool": "codex",
                "auth_mode": "personal_access_token",
                "credential": {"secret": "generic-personal-access-token"}
            }),
            json!({
                "tool": "claude",
                "credential": {
                    "secret": "generic-claude-oauth",
                    "refreshToken": "generic-claude-refresh"
                }
            }),
            json!({
                "tool": "codex",
                "credential": {"secret": codex_jwt}
            }),
        ];

        for payload in cases {
            let parsed = parse_import_content(&payload.to_string(), None)
                .expect("generic credential OAuth token");
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].credential.kind(), AccountAuthKind::OAuth);
        }
    }

    #[test]
    fn imports_codex_auth_and_extracts_jwt_identity() {
        let (_temp, pool) = pool();
        let access = jwt(json!({
            "email": "codex@example.com",
            "exp": 1_900_000_000i64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-from-jwt"
            }
        }));
        let auth = json!({
            "tokens": {
                "id_token": "unused-id-token",
                "access_token": access,
                "refresh_token": "codex-refresh-token"
            }
        });
        let result = pool
            .import_content(&auth.to_string(), ImportOptions::default())
            .expect("codex auth");
        assert_eq!(result.accounts[0].name, "codex@example.com");
        assert_eq!(result.accounts[0].tool, ToolKind::Codex);

        let candidate = pool
            .candidates(ToolKind::Codex)
            .expect("candidates")
            .remove(0);
        assert_eq!(candidate.credential.account_id(), Some("account-from-jwt"));
        assert_eq!(
            candidate.credential.refresh_token(),
            Some("codex-refresh-token")
        );
        assert_eq!(
            candidate.credential.expires_at_ms(),
            Some(1_900_000_000_000)
        );
    }

    #[test]
    fn shared_codex_oauth_helpers_prefer_explicit_identity_and_parse_expiry() {
        let access = jwt(json!({
            "exp": 1_900_000_000i64,
            "https://api.openai.com/auth": {"chatgpt_account_id": "jwt-account"}
        }));
        let explicit = json!({
            "tokens": {
                "accountId": "explicit-account",
                "expiresAt": 1_910_000_000i64
            }
        });
        assert_eq!(
            codex_oauth_account_id(&explicit, &access).as_deref(),
            Some("explicit-account")
        );
        assert_eq!(
            codex_oauth_expires_at_ms(&explicit, &access),
            Some(1_910_000_000_000)
        );
        assert_eq!(
            codex_oauth_account_id(&json!({}), &access).as_deref(),
            Some("jwt-account")
        );
        assert_eq!(
            codex_oauth_expires_at_ms(&json!({}), &access),
            Some(1_900_000_000_000)
        );
    }

    #[test]
    fn imports_cockpit_codex_personal_access_token_shapes_as_oauth() {
        let (_temp, pool) = pool();
        let payload = json!({
            "accounts": [
                {"personal_access_token": "at-root-personal-token"},
                {"tokens": {"personalAccessToken": "at-tokens-personal-token"}},
                {"credentials": {"atToken": "at-credentials-personal-token"}}
            ]
        });
        let result = pool
            .import_content(&payload.to_string(), ImportOptions::default())
            .expect("personal access token shapes");
        assert_eq!(result.imported, 3);
        let candidates = pool.candidates(ToolKind::Codex).expect("codex candidates");
        assert_eq!(candidates.len(), 3);
        assert!(candidates
            .iter()
            .all(|candidate| candidate.credential.auth_kind() == AccountAuthKind::OAuth));
    }

    #[test]
    fn treats_codex_selected_and_openai_claim_jwts_as_oauth() {
        let (_temp, pool) = pool();
        let selected_jwt = jwt(json!({"sub": "selected-codex-jwt"}));
        let selected = pool
            .import_content(&selected_jwt, import_options(ToolKind::Codex))
            .expect("Codex-selected JWT");
        assert_eq!(selected.accounts[0].auth_kind, AccountAuthKind::OAuth);

        let openai_jwt = jwt(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "jwt-account"}
        }));
        let inferred = pool
            .import_content(&openai_jwt, ImportOptions::default())
            .expect("OpenAI-claim JWT");
        assert_eq!(inferred.accounts[0].tool, ToolKind::Codex);
        assert_eq!(inferred.accounts[0].auth_kind, AccountAuthKind::OAuth);
    }

    #[test]
    fn imports_mixed_array_and_common_wrappers() {
        let (_temp, pool) = pool();
        let payload = json!({
            "exported_at": "ignored",
            "accounts": [
                {
                    "platform": "anthropic",
                    "authMode": "api_key",
                    "apiKey": "sk-ant-api03-array-key",
                    "name": "Claude array"
                },
                {
                    "platform": "openai",
                    "type": "oauth",
                    "credentials": {
                        "access_token": "codex-array-access",
                        "refresh_token": "codex-array-refresh",
                        "account_id": "array-account"
                    },
                    "name": "Codex array"
                }
            ]
        });
        let result = pool
            .import_content(&payload.to_string(), ImportOptions::default())
            .expect("wrapped import");
        assert_eq!(result.imported, 2);
        assert_eq!(pool.list().expect("list").len(), 2);
        assert_eq!(
            pool.candidates(ToolKind::Codex).expect("codex")[0]
                .credential
                .account_id(),
            Some("array-account")
        );
    }

    #[test]
    fn imports_cockpit_full_backup_platform_wrappers() {
        let (_temp, pool) = pool();
        let payload = json!({
            "schema": "cockpit-tools.account-transfer",
            "platforms": {
                "codex": {
                    "account_count": 1,
                    "exported_data": [{
                        "tokens": {
                            "access_token": "codex-backup-access",
                            "refresh_token": "codex-backup-refresh",
                            "account_id": "codex-backup-account"
                        }
                    }]
                },
                "claude_manager": {
                    "account_count": 1,
                    "exported_data": [{
                        "claude_credentials_raw": {"claudeAiOauth": {
                            "accessToken": "sk-ant-oat01-backup-one"
                        }},
                        "claude_config_raw": {"oauthAccount": {
                            "account_uuid": "claude-backup-one"
                        }}
                    }]
                },
                "claudeManager": {
                    "accountCount": 1,
                    "exportedData": [{
                        "claudeAiOauth": {"accessToken": "sk-ant-oat01-backup-two"},
                        "accountId": "claude-backup-two"
                    }]
                },
                "cursor": {
                    "exported_data": [{"base_url": "https://unrelated.example/v1"}]
                }
            }
        });

        let result = pool
            .import_content(&payload.to_string(), ImportOptions::default())
            .expect("Cockpit full backup");
        assert_eq!(result.imported, 3);
        assert_eq!(pool.candidates(ToolKind::Codex).expect("Codex").len(), 1);
        assert_eq!(pool.candidates(ToolKind::Claude).expect("Claude").len(), 2);
    }

    #[test]
    fn imports_line_delimited_json_objects() {
        let (_temp, pool) = pool();
        let content = format!(
            "{}\n{}",
            json!({"tool": "claude", "api_key": "sk-ant-api03-ndjson-one"}),
            json!({"tool": "claude", "api_key": "sk-ant-api03-ndjson-two"})
        );
        let result = pool
            .import_content(&content, ImportOptions::default())
            .expect("ndjson");
        assert_eq!(result.imported, 2);
    }

    #[test]
    fn rejects_custom_provider_base_urls_in_common_nested_shapes() {
        let (_temp, pool) = pool();
        let payloads = [
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "sk-ant-api03-proxy-key",
                    "ANTHROPIC_BASE_URL": "https://proxy.example"
                }
            }),
            json!({
                "tool": "codex",
                "OPENAI_API_KEY": "sk-proj-proxy-key",
                "openaiBaseUrl": "https://openai-proxy.example/v1"
            }),
            json!({
                "api_base_url": "https://provider.example/v1",
                "accounts": [{
                    "tool": "claude",
                    "apiKey": "sk-ant-api03-wrapped-proxy-key"
                }]
            }),
            json!({
                "platform": "anthropic",
                "provider": {
                    "base_url": "https://provider.example/anthropic"
                },
                "authMode": "api_key",
                "apiKey": "sk-ant-api03-provider-proxy-key"
            }),
            json!({
                "OPENAI_API_KEY": "sk-proj-legacy-proxy-key",
                "openai_api_base": "https://legacy-openai-proxy.example/v1"
            }),
        ];

        for payload in payloads {
            let error = pool
                .import_content(&payload.to_string(), ImportOptions::default())
                .expect_err("custom provider URL must be rejected");
            assert!(error.contains("非官方 API 地址"));
            assert!(!error.contains("proxy.example"));
        }
        let mismatch = json!({
            "ANTHROPIC_API_KEY": "sk-ant-api03-mismatched-official-url",
            "api_base_url": "https://api.openai.com/v1"
        });
        assert!(pool
            .import_content(&mismatch.to_string(), ImportOptions::default())
            .is_err());
        assert!(pool.list().expect("empty pool").is_empty());
    }

    #[test]
    fn accepts_explicit_official_provider_base_urls() {
        let (_temp, pool) = pool();
        let payload = json!({
            "accounts": [
                {
                    "env": {
                        "ANTHROPIC_API_KEY": "sk-ant-api03-official-url",
                        "ANTHROPIC_BASE_URL": "https://api.anthropic.com/"
                    }
                },
                {
                    "tool": "codex",
                    "OPENAI_API_KEY": "sk-proj-official-url",
                    "apiBaseUrl": "https://api.openai.com/v1"
                },
                {
                    "OPENAI_API_KEY": "sk-proj-official-legacy-url",
                    "openai_api_base": "https://api.openai.com/v1"
                }
            ]
        });
        let result = pool
            .import_content(&payload.to_string(), ImportOptions::default())
            .expect("official provider URLs");
        assert_eq!(result.imported, 3);
    }

    #[test]
    fn summaries_and_debug_output_never_contain_secrets() {
        let (_temp, pool) = pool();
        let secret = "sk-ant-api03-never-display-this";
        let result = pool
            .import_content(secret, import_options(ToolKind::Claude))
            .expect("import");
        let summary_json = serde_json::to_string(&result.accounts).expect("serialize summary");
        let summary_debug = format!("{:?}", result.accounts);
        assert!(!summary_json.contains(secret));
        assert!(!summary_debug.contains(secret));
        assert!(!summary_json.to_ascii_lowercase().contains("secret"));
        assert!(summary_json.contains("\"credentialState\":\"normal\""));
        assert!(!summary_json.contains("\"credential\":"));
        assert!(!summary_json.contains("accessToken"));

        let stored = fs::read_to_string(pool.path()).expect("encrypted store");
        assert!(!stored.contains(secret));
        assert!(!stored.contains("accessToken"));
        assert!(!stored.contains("credential"));
    }

    #[test]
    fn duplicate_import_updates_credentials_without_overwriting_local_metadata() {
        let (_temp, pool) = pool();
        let secret = "sk-proj-duplicate-openai-key";
        let first = pool
            .import_content(
                secret,
                ImportOptions {
                    name: Some("Original".to_string()),
                    priority: Some(20),
                    ..import_options(ToolKind::Codex)
                },
            )
            .expect("first");
        pool.update(
            &first.accounts[0].id,
            UpdateAccountInput {
                name: Some("My account".to_string()),
                ..UpdateAccountInput::default()
            },
        )
        .expect("rename");
        let second = pool
            .import_content(
                secret,
                ImportOptions {
                    priority: Some(3),
                    ..import_options(ToolKind::Codex)
                },
            )
            .expect("second");
        assert_eq!(second.imported, 0);
        assert_eq!(second.updated, 1);
        assert_eq!(second.accounts[0].id, first.accounts[0].id);
        assert_eq!(second.accounts[0].name, "My account");
        assert_eq!(second.accounts[0].priority, 20);
    }

    #[test]
    fn supports_update_and_delete() {
        let (_temp, pool) = pool();
        let imported = pool
            .import_content("sk-openai-delete-key", import_options(ToolKind::Codex))
            .expect("import");
        let id = &imported.accounts[0].id;
        let updated = pool
            .update(
                id,
                UpdateAccountInput {
                    name: Some("Renamed".to_string()),
                    enabled: Some(false),
                    priority: Some(7),
                },
            )
            .expect("update");
        assert_eq!(updated.name, "Renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.priority, 7);
        assert!(pool.delete(id).expect("delete"));
        assert!(!pool.delete(id).expect("delete missing"));
        assert!(pool.list().expect("list").is_empty());
    }

    #[test]
    fn oauth_identity_uses_non_empty_refresh_tokens_without_account_ids() {
        let first = StoredCredential::OAuth {
            access_token: "access-one".to_string(),
            refresh_token: Some("refresh-shared".to_string()),
            account_id: None,
            expires_at_ms: None,
        };
        let second = StoredCredential::OAuth {
            access_token: "access-two".to_string(),
            refresh_token: Some("refresh-shared".to_string()),
            account_id: None,
            expires_at_ms: None,
        };
        assert!(first.same_identity(&second));

        let different_refresh = StoredCredential::OAuth {
            access_token: "access-two".to_string(),
            refresh_token: Some("refresh-other".to_string()),
            account_id: None,
            expires_at_ms: None,
        };
        assert!(!first.same_identity(&different_refresh));

        let identified = StoredCredential::OAuth {
            access_token: "access-two".to_string(),
            refresh_token: Some("refresh-shared".to_string()),
            account_id: Some("account-1".to_string()),
            expires_at_ms: None,
        };
        assert!(first.same_identity(&identified));

        let conflicting_identity = StoredCredential::OAuth {
            access_token: "access-two".to_string(),
            refresh_token: Some("refresh-shared".to_string()),
            account_id: Some("account-2".to_string()),
            expires_at_ms: None,
        };
        assert!(!identified.same_identity(&conflicting_identity));
    }

    #[test]
    fn rejects_secret_as_account_name() {
        let (_temp, pool) = pool();
        let secret = "sk-ant-api03-secret-name";
        let result = pool
            .import_content(
                &json!({
                    "auth_mode": "api_key",
                    "api_key": secret,
                    "name": secret,
                    "tool": "claude"
                })
                .to_string(),
                ImportOptions::default(),
            )
            .expect("import");
        assert_eq!(result.accounts[0].name, "Claude API Key");
        assert!(!serde_json::to_string(&result)
            .expect("json")
            .contains(secret));
    }

    #[test]
    fn imports_local_claude_and_codex_files() {
        let (temp, pool) = pool();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".claude")).expect("claude dir");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        fs::write(
            home.join(".claude/.credentials.json"),
            json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-local-access",
                    "refreshToken": "sk-ant-ort01-local-refresh"
                }
            })
            .to_string(),
        )
        .expect("claude auth");
        fs::write(
            home.join(".codex/auth.json"),
            json!({ "OPENAI_API_KEY": "sk-proj-local-codex" }).to_string(),
        )
        .expect("codex auth");

        let result = pool.import_local_from(&home).expect("local import");
        assert_eq!(result.imported, 2);
        assert!(result
            .accounts
            .iter()
            .all(|account| account.source == AccountSource::Local));
    }

    #[test]
    fn codex_local_custom_provider_filters_api_keys_but_keeps_oauth() {
        assert!(codex_config_allows_official_api(
            "model_provider = \"openai\"\n[model_providers.openai]\nbase_url = \"https://api.openai.com/v1\""
        ));
        assert!(codex_config_allows_official_api(
            "openai_base_url = \"https://api.openai.com/v1\""
        ));
        assert!(!codex_config_allows_official_api(
            "openai_base_url = \"https://proxy.example/v1\""
        ));
        assert!(!codex_config_allows_official_api(
            "openai_api_base = \"https://legacy-proxy.example/v1\""
        ));
        assert!(!codex_config_allows_official_api(
            "model_provider = \"proxy\"\n[model_providers.proxy]\nbase_url = \"https://proxy.example/v1\""
        ));
        assert!(!codex_config_allows_official_api(
            "model_providers.proxy = { base_url = \"https://proxy.example/v1\" }"
        ));

        let (temp, pool) = pool();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        fs::write(
            home.join(".codex/config.toml"),
            "model_provider = \"proxy\"\n[model_providers.proxy]\nbase_url = \"https://proxy.example/v1\"",
        )
        .expect("codex config");
        fs::write(
            home.join(".codex/auth.json"),
            json!({
                "accounts": [
                    {"OPENAI_API_KEY": "sk-proj-custom-provider"},
                    {"tokens": {
                        "access_token": "at-official-oauth",
                        "account_id": "official-oauth-account"
                    }}
                ]
            })
            .to_string(),
        )
        .expect("codex auth");

        let result = pool
            .import_local_from(&home)
            .expect("OAuth remains importable");
        assert_eq!(result.imported, 1);
        assert_eq!(result.accounts[0].auth_kind, AccountAuthKind::OAuth);
        let candidates = pool.candidates(ToolKind::Codex).expect("Codex candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].credential.secret(), "at-official-oauth");
    }

    #[test]
    fn periodic_local_refresh_updates_only_known_accounts_and_preserves_metadata() {
        let (temp, pool) = pool();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        let auth_path = home.join(".codex/auth.json");
        fs::write(
            &auth_path,
            json!({
                "accounts": [{"tokens": {
                    "access_token": "known-access-v1",
                    "refresh_token": "known-refresh",
                    "account_id": "known-account"
                }}]
            })
            .to_string(),
        )
        .expect("initial auth");
        let imported = pool.import_local_from(&home).expect("initial import");
        let id = imported.accounts[0].id.clone();
        pool.update(
            &id,
            UpdateAccountInput {
                name: Some("用户自定义名称".to_string()),
                enabled: Some(false),
                priority: Some(7),
            },
        )
        .expect("customize");

        fs::write(
            &auth_path,
            json!({
                "accounts": [
                    {"tokens": {
                        "access_token": "known-access-v2",
                        "refresh_token": "known-refresh",
                        "account_id": "known-account"
                    }},
                    {"tokens": {
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "account_id": "new-account"
                    }}
                ]
            })
            .to_string(),
        )
        .expect("rotated auth");
        let refreshed = pool
            .refresh_known_local_with_keychain(&home, None)
            .expect("refresh");
        assert_eq!(refreshed.updated, 1);
        assert_eq!(refreshed.discovered, 1);
        assert_eq!(pool.list().unwrap().len(), 1, "new accounts require review");
        let summary = &pool.list().unwrap()[0];
        assert_eq!(summary.id, id);
        assert_eq!(summary.name, "用户自定义名称");
        assert!(!summary.enabled);
        assert_eq!(summary.priority, 7);
        assert!(pool.candidates(ToolKind::Codex).unwrap().is_empty());
        pool.update(
            &id,
            UpdateAccountInput {
                enabled: Some(true),
                ..UpdateAccountInput::default()
            },
        )
        .expect("enable for credential inspection");
        assert_eq!(
            pool.candidates(ToolKind::Codex).unwrap()[0]
                .credential
                .secret(),
            "known-access-v2"
        );
    }

    #[test]
    fn merges_testable_claude_keychain_json_with_local_files() {
        let (temp, pool) = pool();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".claude")).expect("claude dir");
        fs::write(
            home.join(".claude/.credentials.json"),
            json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-file-access",
                    "refreshToken": "sk-ant-ort01-shared-refresh",
                    "expiresAt": now_ms().saturating_add(300_000)
                }
            })
            .to_string(),
        )
        .expect("file auth");
        let keychain = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-keychain-access",
                "refreshToken": "sk-ant-ort01-shared-refresh",
                "expiresAt": now_ms().saturating_add(600_000)
            }
        })
        .to_string();

        let result = pool
            .import_local_with_keychain(&home, Some(&keychain))
            .expect("keychain merge");
        assert_eq!(result.imported, 1);
        assert_eq!(result.updated, 1);
        assert_eq!(result.accounts[0].source, AccountSource::Local);
        assert_eq!(
            pool.candidates(ToolKind::Claude).expect("candidate")[0]
                .credential
                .secret(),
            "sk-ant-oat01-keychain-access"
        );
    }

    #[test]
    fn oversized_encrypted_store_is_rejected_before_replacing_existing_file() {
        let (_temp, pool) = pool();
        pool.import_content("sk-proj-existing-store", import_options(ToolKind::Codex))
            .expect("initial store");
        let original = fs::read(pool.path()).expect("read initial store");
        let oversized = AccountStore {
            version: STORE_VERSION,
            accounts: vec![StoredAccount {
                id: "x".repeat((MAX_STORE_BYTES as usize * 3 / 4) + 4_096),
                tool: ToolKind::Codex,
                name: "Oversized".to_string(),
                enabled: true,
                priority: DEFAULT_ACCOUNT_PRIORITY,
                source: AccountSource::Json,
                created_at_ms: now_ms(),
                updated_at_ms: now_ms(),
                source_id: None,
                credential: StoredCredential::ApiKey {
                    secret: "sk-proj-oversized-store".to_string(),
                },
            }],
        };

        let error = save_store(pool.path(), &oversized).expect_err("store must exceed size cap");
        assert!(error.contains("账号存储内容过大"));
        assert_eq!(fs::read(pool.path()).expect("existing store"), original);
    }

    #[test]
    fn writes_atomically_with_private_unix_permissions() {
        let (temp, pool) = pool();
        pool.import_content(
            "sk-ant-api03-permission-key",
            import_options(ToolKind::Claude),
        )
        .expect("import");
        let parent = pool.path().parent().expect("parent");
        let entries = fs::read_dir(parent)
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&pool.path().file_name().expect("file").to_os_string()));
        assert!(entries.contains(
            &account_key_path(pool.path())
                .file_name()
                .expect("key file")
                .to_os_string()
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(pool.path())
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(account_key_path(pool.path()))
                    .expect("key metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(parent)
                    .expect("parent metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        assert!(temp.path().exists());
    }

    #[test]
    fn recovers_authenticated_store_when_only_stable_backup_remains() {
        let (_temp, pool) = pool();
        let imported = pool
            .import_content(
                "sk-ant-api03-backup-recovery",
                import_options(ToolKind::Claude),
            )
            .expect("import");
        let backup = account_store_backup_path(pool.path());
        assert_eq!(
            backup.file_name().and_then(|name| name.to_str()),
            Some("accounts.json.bak")
        );
        fs::rename(pool.path(), &backup).expect("simulate interrupted replacement");

        let accounts = pool.list().expect("recover backup");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, imported.accounts[0].id);
        assert!(pool.path().exists());
        assert!(!backup.exists());
    }

    #[test]
    fn recovers_valid_backup_over_a_damaged_primary_without_leaking_paths() {
        let (temp, pool) = pool();
        let imported = pool
            .import_content(
                "sk-proj-damaged-primary-recovery",
                import_options(ToolKind::Codex),
            )
            .expect("import");
        let backup = account_store_backup_path(pool.path());
        fs::copy(pool.path(), &backup).expect("copy valid backup");
        fs::write(pool.path(), b"not-json").expect("damage primary");

        let accounts = pool.list().expect("recover valid backup");
        assert_eq!(accounts[0].id, imported.accounts[0].id);
        assert!(!backup.exists());
        let stored = fs::read_to_string(pool.path()).expect("restored encrypted store");
        assert!(!stored.contains(
            temp.path()
                .to_str()
                .expect("temporary directory should be UTF-8")
        ));
    }

    #[test]
    fn does_not_overwrite_corrupt_store() {
        let (_temp, pool) = pool();
        fs::create_dir_all(pool.path().parent().expect("parent")).expect("mkdir");
        fs::write(pool.path(), b"not-json").expect("write corrupt");
        let error = pool
            .import_content("sk-ant-api03-new-key", import_options(ToolKind::Claude))
            .expect_err("must reject corrupt store");
        assert!(error.contains("损坏"));
        assert_eq!(fs::read(pool.path()).expect("unchanged"), b"not-json");
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        let (_temp, pool) = pool();
        pool.import_content(
            "sk-ant-api03-authenticated-encryption",
            import_options(ToolKind::Claude),
        )
        .expect("import");
        let mut envelope: Value =
            serde_json::from_slice(&fs::read(pool.path()).expect("read store")).expect("envelope");
        let ciphertext = envelope["ciphertext"].as_str().expect("ciphertext");
        let replacement = if ciphertext.starts_with('A') {
            "B"
        } else {
            "A"
        };
        envelope["ciphertext"] = Value::String(format!("{replacement}{}", &ciphertext[1..]));
        fs::write(
            pool.path(),
            serde_json::to_vec(&envelope).expect("serialize tampered"),
        )
        .expect("tamper");

        let error = pool.list().expect_err("tampering must fail");
        assert!(error.contains("校验失败"));
    }

    #[test]
    fn credential_expiration_is_explicit_for_router() {
        let credential = AccountCredential {
            auth_kind: AccountAuthKind::OAuth,
            secret: "access".to_string(),
            refresh_token: None,
            account_id: None,
            expires_at_ms: Some(1_000),
        };
        assert!(!credential.is_expired_at(999));
        assert!(credential.is_expired_at(1_000));
    }

    #[test]
    fn has_enabled_ignores_expired_oauth_but_keeps_api_keys() {
        let (_temp, expired_pool) = pool();
        let expired = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-expired-pool-token",
                "expiresAt": now_ms().saturating_sub(1)
            }
        });
        let result = expired_pool
            .import_content(&expired.to_string(), ImportOptions::default())
            .expect("expired OAuth can be maintained in the pool");
        assert!(result.accounts[0].enabled);
        assert!(!expired_pool
            .has_enabled(ToolKind::Claude)
            .expect("has enabled"));

        let (_near_temp, near_pool) = pool();
        let near_expiry = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-near-expiry-pool-token",
                "expiresAt": now_ms().saturating_add(30_000)
            }
        });
        near_pool
            .import_content(&near_expiry.to_string(), ImportOptions::default())
            .expect("near-expiry OAuth");
        assert!(!near_pool
            .has_enabled(ToolKind::Claude)
            .expect("near-expiry has enabled"));

        expired_pool
            .import_content(
                "sk-ant-api03-unexpired-api-key",
                import_options(ToolKind::Claude),
            )
            .expect("API key");
        assert!(expired_pool
            .has_enabled(ToolKind::Claude)
            .expect("has API key"));
    }

    #[test]
    fn authenticated_v1_store_migrates_atomically_and_keeps_the_original_backup() {
        let (_temp, pool) = pool();
        let legacy = AccountStore {
            version: LEGACY_STORE_VERSION,
            accounts: vec![StoredAccount {
                id: Uuid::new_v4().to_string(),
                tool: ToolKind::Claude,
                name: "Legacy".to_string(),
                enabled: true,
                priority: DEFAULT_ACCOUNT_PRIORITY,
                source: AccountSource::Local,
                created_at_ms: 1,
                updated_at_ms: 1,
                source_id: None,
                credential: StoredCredential::ApiKey {
                    secret: "sk-ant-api03-legacy-store".to_string(),
                },
            }],
        };
        save_store(pool.path(), &legacy).expect("write v1 store");
        let original = fs::read(pool.path()).expect("legacy envelope");

        assert_eq!(pool.list().expect("migrated list").len(), 1);
        let migration_backup = account_store_migration_backup_path(pool.path());
        assert_eq!(
            fs::read(&migration_backup).expect("migration backup"),
            original
        );
        assert_eq!(
            read_store_file(&migration_backup, pool.path())
                .expect("authenticated v1 backup")
                .version,
            LEGACY_STORE_VERSION
        );
        assert_eq!(
            read_store_file(pool.path(), pool.path())
                .expect("migrated v2 store")
                .version,
            STORE_VERSION
        );
    }

    #[test]
    fn local_source_slot_matches_full_oauth_rotation_without_resetting_metadata() {
        let (temp, pool) = pool();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        let auth_path = home.join(".codex/auth.json");
        fs::write(
            &auth_path,
            json!({"tokens": {
                "access_token": "codex-source-access-v1",
                "refresh_token": "codex-source-refresh-v1"
            }})
            .to_string(),
        )
        .expect("initial auth");
        let imported = pool.import_local_from(&home).expect("initial local import");
        let id = imported.accounts[0].id.clone();
        pool.update(
            &id,
            UpdateAccountInput {
                name: Some("保留名称".to_string()),
                enabled: Some(false),
                priority: Some(9),
            },
        )
        .expect("metadata");

        fs::write(
            &auth_path,
            json!({"tokens": {
                "access_token": "codex-source-access-v2",
                "refresh_token": "codex-source-refresh-v2"
            }})
            .to_string(),
        )
        .expect("rotated auth");
        let refreshed = pool
            .refresh_known_local_with_keychain(&home, None)
            .expect("source slot refresh");
        assert_eq!(refreshed.updated, 1);
        assert_eq!(refreshed.discovered, 0);
        let account = &pool.list().unwrap()[0];
        assert_eq!(account.id, id);
        assert_eq!(account.name, "保留名称");
        assert_eq!(account.priority, 9);
        assert!(!account.enabled);
        pool.update(
            &id,
            UpdateAccountInput {
                enabled: Some(true),
                ..UpdateAccountInput::default()
            },
        )
        .expect("enable");
        assert_eq!(
            pool.candidates(ToolKind::Codex).unwrap()[0]
                .credential
                .secret(),
            "codex-source-access-v2"
        );
    }

    #[test]
    fn interrupted_replace_can_restore_the_authenticated_local_snapshot() {
        let (_temp, pool) = pool();
        pool.import_content(
            "sk-ant-api03-before-replace",
            ImportOptions {
                tool: Some(ToolKind::Claude),
                name: Some("Before".to_string()),
                ..ImportOptions::default()
            },
        )
        .expect("seed");
        let before = load_store(pool.path()).expect("before store");
        write_restore_snapshot(pool.path(), &before).expect("snapshot");

        pool.import_content(
            "sk-proj-after-replace",
            ImportOptions {
                tool: Some(ToolKind::Codex),
                ..ImportOptions::default()
            },
        )
        .expect("mutate after snapshot");
        assert_eq!(pool.list().unwrap().len(), 2);

        rollback_restore_snapshot(pool.path()).expect("automatic rollback");
        let restored = pool.list().expect("restored");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, "Before");
    }
}
