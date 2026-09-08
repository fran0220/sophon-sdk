//! Conversation transfer, not an execution or filesystem backup.
//!
//! The actor owns the admission fence and flush/capture boundary. The format
//! preserves current native model context and unfiltered persisted events.
//! Native compaction/rewind can already have destroyed old model contexts.
//! Filesystem rewind snapshots, tool resources, credentials, telemetry and
//! process custody are never restored. Text and tool output are NOT sanitized.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::info::Info;
use super::persistence::{CHAT_FORMAT_VERSION, Summary};
use super::storage as st;

pub const PORTABLE_FORMAT_VERSION: u32 = 1;
/// A schema/semantic compatibility identifier, not a promise that arbitrary
/// Grok Build 1.0.16 or SDK 0.4.1 revisions implement this contract.
pub const PORTABLE_COMPATIBILITY: &str = "sophon-conversation-v1-grok-build-1.0.16-chat-1";
pub const MAX_PORTABLE_BYTES: usize = 200 * 1024 * 1024;

/// Checked against current native data without attaching an actor. Only
/// MatchesSnapshot can reconcile an uncertain import response as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableImportStatus {
    Missing,
    MatchesSnapshot,
    Different,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableCompleteness {
    pub current_native_conversation: bool,
    pub unfiltered_persisted_events: bool,
    pub historical_model_context_branches: bool,
    pub filesystem_rollback: bool,
    pub execution_custody: bool,
    pub content_sanitized: bool,
}

impl Default for PortableCompleteness {
    fn default() -> Self {
        Self {
            current_native_conversation: true,
            unfiltered_persisted_events: true,
            historical_model_context_branches: false,
            filesystem_rollback: false,
            execution_custody: false,
            content_sanitized: false,
        }
    }
}

/// Opaque native payload. Serialize for storage; never edit/replay its contents.
/// Use `from_slice` at untrusted byte boundaries (bounded before JSON parsing).
/// Import validates again, including values obtained through generic serde.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableSession {
    format_version: u32,
    compatibility: String,
    session_id: String,
    completeness: PortableCompleteness,
    revision: String,
    #[serde(deserialize_with = "unique_files")]
    files: BTreeMap<String, String>,
}

fn unique_files<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Files;
    impl<'de> serde::de::Visitor<'de> for Files {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unique native file entries")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            let mut files = BTreeMap::new();
            let mut total = 0usize;
            while let Some(name) = map.next_key::<String>()? {
                if !FILES.contains(&name.as_str()) || files.contains_key(&name) {
                    return Err(A::Error::custom("unknown or duplicate native file"));
                }
                let data: String = map.next_value()?;
                total = total.saturating_add(data.len());
                if total > MAX_PORTABLE_BYTES {
                    return Err(A::Error::custom("native files exceed 200 MiB"));
                }
                files.insert(name, data);
            }
            Ok(files)
        }
    }
    deserializer.deserialize_map(Files)
}

impl std::fmt::Debug for PortableSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortableSession")
            .field("session_id", &self.session_id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PortabilityError {
    #[error("session or agent has admitted work; retry after it settles")]
    Busy,
    #[error("session actor is unavailable")]
    Unavailable,
    #[error("session ID already exists locally; import never overwrites: {0}")]
    ExistingSession(String),
    #[error("session ID has a live actor: {0}")]
    ActiveSession(String),
    #[error("incompatible portable conversation format")]
    Incompatible,
    #[error("portable conversation is incomplete: {0}")]
    Incomplete(String),
    #[error("malformed portable conversation: {0}")]
    Malformed(String),
    #[error("portable conversation exceeds 200 MiB")]
    TooLarge,
    #[error("native persistence failed: {0}")]
    Persistence(#[from] io::Error),
}

impl PortableSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn format_version(&self) -> u32 {
        self.format_version
    }
    pub fn compatibility(&self) -> &str {
        &self.compatibility
    }
    /// Content-addressed capture revision, NOT a monotonic cloud revision.
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn completeness(&self) -> &PortableCompleteness {
        &self.completeness
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, PortabilityError> {
        if bytes.len() > MAX_PORTABLE_BYTES {
            return Err(PortabilityError::TooLarge);
        }
        let result: Self = serde_json::from_slice(bytes).map_err(malformed)?;
        result.validate()?;
        Ok(result)
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, PortabilityError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(malformed)?;
        if bytes.len() > MAX_PORTABLE_BYTES {
            return Err(PortabilityError::TooLarge);
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), PortabilityError> {
        if self.format_version != PORTABLE_FORMAT_VERSION
            || self.compatibility != PORTABLE_COMPATIBILITY
        {
            return Err(PortabilityError::Incompatible);
        }
        if self.completeness != PortableCompleteness::default() {
            return Err(PortabilityError::Incomplete(
                "unsupported completeness declaration".into(),
            ));
        }
        uuid::Uuid::parse_str(&self.session_id).map_err(malformed)?;
        if self.files.values().map(String::len).sum::<usize>() > MAX_PORTABLE_BYTES {
            return Err(PortabilityError::TooLarge);
        }
        if self.files.keys().any(|key| !FILES.contains(&key.as_str())) {
            return Err(PortabilityError::Malformed("unknown native file".into()));
        }
        for required in [st::SUMMARY_FILE, st::CHAT_HISTORY_FILE, st::UPDATES_FILE] {
            if !self.files.contains_key(required) {
                return Err(PortabilityError::Incomplete(format!("missing {required}")));
            }
        }
        if revision(&self.files) != self.revision {
            return Err(PortabilityError::Malformed(
                "capture revision mismatch".into(),
            ));
        }
        let summary_value: Value =
            serde_json::from_str(&self.files[st::SUMMARY_FILE]).map_err(malformed)?;
        if summary_value.as_object().is_none_or(|object| {
            object
                .keys()
                .any(|key| !SUMMARY_FIELDS.contains(&key.as_str()))
        }) {
            return Err(PortabilityError::Malformed(
                "nonportable summary metadata".into(),
            ));
        }
        if summary_value
            .get("info")
            .and_then(Value::as_object)
            .is_none_or(|info| info.keys().any(|key| key != "id" && key != "cwd"))
        {
            return Err(malformed("nonportable identity metadata"));
        }
        let summary: Summary = serde_json::from_value(summary_value).map_err(malformed)?;
        if summary.chat_format_version != CHAT_FORMAT_VERSION {
            return Err(PortabilityError::Incompatible);
        }
        if summary.info.id.0.as_ref() != self.session_id || !summary.info.cwd.is_empty() {
            return Err(PortabilityError::Malformed(
                "summary identity/custody mismatch".into(),
            ));
        }
        for line in self.files[st::CHAT_HISTORY_FILE]
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            use crate::sampling::{ContentPart, ConversationItem};
            let item: ConversationItem = serde_json::from_str(line).map_err(malformed)?;
            let parts = match &item {
                ConversationItem::User(user) => user.content.as_slice(),
                ConversationItem::ToolResult(tool) => tool.images.as_slice(),
                _ => &[],
            };
            for part in parts {
                if let ContentPart::Image { url } = part {
                    use base64::Engine;
                    let Some((_, data)) = url
                        .split_once(";base64,")
                        .filter(|(mime, data)| mime.starts_with("data:image/") && !data.is_empty())
                    else {
                        return Err(PortabilityError::Incomplete(
                            "conversation image is not self-contained inline data".into(),
                        ));
                    };
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(malformed)?;
                }
            }
        }
        if self.files[st::CHAT_HISTORY_FILE]
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            != summary.num_chat_messages
        {
            return Err(PortabilityError::Incomplete(
                "native conversation/summary count mismatch".into(),
            ));
        }
        for line in self.files[st::UPDATES_FILE]
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let update = st::SessionUpdateEnvelope::from_str(line).map_err(malformed)?;
            let id = match &update {
                st::SessionUpdate::Acp(update) => &update.session_id,
                st::SessionUpdate::Xai(update) => &update.session_id,
            };
            if id.0.as_ref() != self.session_id {
                return Err(PortabilityError::Malformed(
                    "event identity mismatch".into(),
                ));
            }
        }
        if let Some(plan) = self.files.get(st::PLAN_FILE) {
            serde_json::from_str::<crate::tools::todo::TodoState>(plan).map_err(malformed)?;
        }
        if let Some(mode) = self.files.get(st::PLAN_MODE_FILE) {
            let mode: super::plan_mode::PlanModeSnapshot =
                serde_json::from_str(mode).map_err(malformed)?;
            if !matches!(mode.state, super::plan_mode::PlanModeState::Inactive)
                || mode.pending_exit_reminder
                || mode.awaiting_plan_approval
            {
                return Err(PortabilityError::Incomplete(
                    "active/pending plan-mode custody".into(),
                ));
            }
        }
        if let Some(usage) = self.files.get(st::USAGE_FILE) {
            let usage: super::usage_file::SessionUsageFile =
                serde_json::from_str(usage).map_err(malformed)?;
            if !usage.session_id.is_empty() && usage.session_id != self.session_id {
                return Err(PortabilityError::Malformed(
                    "usage identity mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}

fn malformed(error: impl std::fmt::Display) -> PortabilityError {
    PortabilityError::Malformed(error.to_string())
}

const FILES: &[&str] = &[
    st::SUMMARY_FILE,
    st::CHAT_HISTORY_FILE,
    st::UPDATES_FILE,
    st::PLAN_FILE,
    st::PLAN_MODE_FILE,
    st::USAGE_FILE,
    "plan.md",
];
// Positive allowlist: future native metadata is not automatically transferable.
const SUMMARY_FIELDS: &[&str] = &[
    "info",
    "session_summary",
    "created_at",
    "updated_at",
    "num_messages",
    "num_chat_messages",
    "current_model_id",
    "chat_format_version",
    "inherited_prefix_len",
    "reasoning_effort",
    "last_active_at",
    "generated_title",
    "title_is_manual",
    "last_turn_summary",
    "last_turn_summary_prompt_id",
    "last_recap",
];

fn revision(files: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    for (name, data) in files {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((data.len() as u64).to_le_bytes());
        hash.update(data.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// Called inline ONLY by the persistence actor after its checked flush.
pub(crate) fn capture(dir: &Path, info: &Info) -> Result<PortableSession, PortabilityError> {
    // Native out-of-band title writers use this same lock. Keep their summary
    // mutations outside the multi-file cut as well as actor-owned writes.
    let summary_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("summary.json.lock"))?;
    fs2::FileExt::try_lock_exclusive(&summary_lock).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            PortabilityError::Busy
        } else {
            error.into()
        }
    })?;
    for unsupported in ["goal", "workflows"] {
        if dir.join(unsupported).try_exists()? {
            return Err(PortabilityError::Incomplete(format!(
                "{unsupported} orchestration is not portable"
            )));
        }
    }
    let resources_path = dir.join("resources_state.json");
    if resources_path.try_exists()? {
        use std::io::Read;
        let resources: Value = serde_json::from_reader(
            std::fs::File::open(resources_path)?.take(MAX_PORTABLE_BYTES as u64 + 1),
        )
        .map_err(malformed)?;
        if let Some(scheduler) = resources
            .get("state")
            .and_then(|state| state.get("grok_build.Scheduler"))
        {
            let scheduler: xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerState = serde_json::from_value(scheduler.clone()).map_err(malformed)?;
            if !scheduler.tasks.is_empty() {
                return Err(PortabilityError::Incomplete(
                    "persisted scheduled tasks are not portable".into(),
                ));
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    for name in FILES {
        let path = dir.join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(PortabilityError::Incomplete(format!("nonregular {name}")));
        }
        if total.saturating_add(metadata.len()) > MAX_PORTABLE_BYTES as u64 {
            return Err(PortabilityError::TooLarge);
        }
        use std::io::Read;
        // Windows FlushFileBuffers requires a writable handle as well.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let mut data = String::new();
        (&mut file)
            .take(MAX_PORTABLE_BYTES as u64 - total + 1)
            .read_to_string(&mut data)?;
        total = total.saturating_add(data.len() as u64);
        if total > MAX_PORTABLE_BYTES as u64 {
            return Err(PortabilityError::TooLarge);
        }
        // Include files such as plan.md that are not on the append dirty set.
        st::sync_file_durable(&file)?;
        files.insert((*name).to_owned(), data);
    }
    let summary = files
        .get_mut(st::SUMMARY_FILE)
        .ok_or_else(|| PortabilityError::Incomplete("missing native summary".into()))?;
    let mut value: Value = serde_json::from_str(summary).map_err(malformed)?;
    let native: Summary = serde_json::from_value(value.clone()).map_err(malformed)?;
    if native.info.id != info.id || native.info.cwd != info.cwd {
        return Err(PortabilityError::Incomplete(
            "native summary identity differs from its owner".into(),
        ));
    }
    if native.pending_cwd_switch_reminder.is_some() {
        return Err(PortabilityError::Incomplete(
            "pending cwd relocation".into(),
        ));
    }
    value
        .as_object_mut()
        .ok_or_else(|| malformed("summary object required"))?
        .retain(|key, _| SUMMARY_FIELDS.contains(&key.as_str()));
    value["info"] = serde_json::json!({"id": info.id, "cwd": ""});
    *summary = serde_json::to_string(&value).map_err(malformed)?;
    // Empty, genuinely new sessions may not yet have append-only files.
    if native.num_chat_messages == 0 {
        files.entry(st::CHAT_HISTORY_FILE.into()).or_default();
    }
    if native.num_messages == 0 {
        files.entry(st::UPDATES_FILE.into()).or_default();
    }
    let result = PortableSession {
        format_version: PORTABLE_FORMAT_VERSION,
        compatibility: PORTABLE_COMPATIBILITY.into(),
        session_id: info.id.to_string(),
        completeness: PortableCompleteness::default(),
        revision: revision(&files),
        files,
    };
    result.to_vec()?;
    Ok(result)
}

/// Validates everything before touching the destination. Staging is invisible
/// to native session discovery; publication is one no-replace directory rename.
pub(crate) fn import(snapshot: &PortableSession, cwd: &Path) -> Result<String, PortabilityError> {
    snapshot.to_vec()?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(malformed(
            "destination cwd must be an existing absolute directory",
        ));
    }
    let cwd = cwd
        .to_str()
        .ok_or_else(|| malformed("destination cwd must be UTF-8"))?;
    let info = Info {
        id: agent_client_protocol::SessionId::new(snapshot.session_id.clone()),
        cwd: cwd.into(),
    };
    let _import_lock = lock_import()?;
    if super::persistence::find_persisted_session_dir_by_id_result(&snapshot.session_id)?.is_some()
    {
        return Err(PortabilityError::ExistingSession(
            snapshot.session_id.clone(),
        ));
    }
    crate::util::grok_home::ensure_sessions_cwd_dir(cwd)?;
    let target = super::persistence::session_dir(&info);
    if target.try_exists()? {
        return Err(PortabilityError::ExistingSession(
            snapshot.session_id.clone(),
        ));
    }
    // Outside sessions/, so even a crashed stage with summary.json cannot
    // appear in native list/id discovery. It contains no execution config.
    let stage = tempfile::Builder::new()
        .prefix(".portable-")
        .tempdir_in(crate::util::grok_home::grok_home())?;
    for (name, original) in &snapshot.files {
        let mut content = original.clone();
        if name == st::SUMMARY_FILE {
            let mut summary: Value = serde_json::from_str(&content).map_err(malformed)?;
            summary["info"] = serde_json::json!({"id": snapshot.session_id, "cwd": cwd});
            content = serde_json::to_string(&summary).map_err(malformed)?;
        }
        st::write_bytes_atomic(&stage.path().join(name), content.as_bytes())?;
    }
    st::sync_dir_durable(stage.path())?;
    publish(stage.path(), &target).map_err(|error| {
        if target.exists() {
            PortabilityError::ExistingSession(snapshot.session_id.clone())
        } else {
            error.into()
        }
    })?;
    // A parent-sync error can mean publication happened: return a persistence
    // error, never claim rollback; retry then reports ExistingSession.
    st::sync_parent_dir_durable(&target)?;
    Ok(snapshot.session_id.clone())
}

fn lock_import() -> Result<std::fs::File, PortabilityError> {
    // Also serializes same-ID imports mapped to different directories.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(crate::util::grok_home::grok_home().join(".portable-import.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            PortabilityError::Busy
        } else {
            error.into()
        }
    })?;
    Ok(lock)
}

pub(crate) fn import_status(
    snapshot: &PortableSession,
    cwd: &Path,
) -> Result<PortableImportStatus, PortabilityError> {
    snapshot.to_vec()?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(malformed(
            "destination cwd must be an existing absolute directory",
        ));
    }
    let cwd = cwd
        .to_str()
        .ok_or_else(|| malformed("destination cwd must be UTF-8"))?;
    let info = Info {
        id: agent_client_protocol::SessionId::new(snapshot.session_id.clone()),
        cwd: cwd.into(),
    };
    let _lock = lock_import()?;
    let target = super::persistence::session_dir(&info);
    let found = super::persistence::find_persisted_session_dir_by_id_result(&snapshot.session_id)?;
    if found.as_ref().is_some_and(|found| found != &target) {
        return Ok(PortableImportStatus::Different);
    }
    if !target.try_exists()? {
        return Ok(PortableImportStatus::Missing);
    }
    let actual = capture(&target, &info)?;
    st::sync_parent_dir_durable(&target)?;
    Ok(if actual.revision == snapshot.revision {
        PortableImportStatus::MatchesSnapshot
    } else {
        PortableImportStatus::Different
    })
}

#[cfg(target_os = "linux")]
fn publish(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn publish(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn publish(source: &Path, target: &Path) -> io::Result<()> {
    // MoveFileExW cannot replace an existing directory.
    std::fs::rename(source, target)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn publish(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory publication unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Info) {
        let dir = tempfile::tempdir().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new(uuid::Uuid::new_v4().to_string()),
            cwd: dir.path().to_string_lossy().into_owned(),
        };
        let mut summary =
            Summary::new(&info, agent_client_protocol::ModelId::new("model")).unwrap();
        summary.num_chat_messages = 1;
        summary.sandbox_profile = Some("source-machine-profile".into());
        summary.agent_name = Some("source-machine-harness".into());
        std::fs::write(
            dir.path().join(st::SUMMARY_FILE),
            serde_json::to_vec(&summary).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(st::CHAT_HISTORY_FILE),
            r#"{"type":"user","content":[{"type":"text","text":"secret /source/path"}]}"#,
        )
        .unwrap();
        let event = |text: &str| serde_json::json!({"method":"session/update", "params":{"sessionId": info.id, "update":{"sessionUpdate":"user_message_chunk", "content":{"type":"text","text":text}}}});
        let rewind = serde_json::json!({"method":"_x.ai/session/update", "params":{"sessionId": info.id, "update":{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2026-01-01T00:00:00Z"}}});
        let events = [
            event("rewound-native-event"),
            rewind,
            event("current-event"),
        ];
        std::fs::write(
            dir.path().join(st::UPDATES_FILE),
            events
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        (dir, info)
    }

    #[test]
    fn portable_preserves_unfiltered_events_but_not_machine_custody() {
        let (dir, info) = fixture();
        std::fs::write(
            dir.path().join("rewind_points.jsonl"),
            "machine filesystem snapshot",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("resources_state.json"),
            r#"{"params":{"credential":"config"},"state":{}}"#,
        )
        .unwrap();
        let snapshot = capture(dir.path(), &info).unwrap();
        assert_eq!(
            snapshot.files[st::UPDATES_FILE],
            std::fs::read_to_string(dir.path().join(st::UPDATES_FILE)).unwrap()
        );
        assert!(snapshot.files[st::UPDATES_FILE].contains("rewound-native-event"));
        assert!(snapshot.files[st::CHAT_HISTORY_FILE].contains("secret /source/path"));
        assert!(!snapshot.files.contains_key("rewind_points.jsonl"));
        assert!(!snapshot.files.contains_key("resources_state.json"));
        let metadata: Value = serde_json::from_str(&snapshot.files[st::SUMMARY_FILE]).unwrap();
        assert_eq!(metadata["info"]["cwd"], "");
        assert!(metadata.get("sandbox_profile").is_none());
        assert!(metadata.get("agent_name").is_none());
        assert_eq!(
            PortableSession::from_slice(&snapshot.to_vec().unwrap())
                .unwrap()
                .revision(),
            snapshot.revision()
        );
    }

    #[test]
    fn portable_rejects_missing_corrupt_unsupported_state_and_external_images() {
        let (dir, info) = fixture();
        std::fs::create_dir(dir.path().join("goal")).unwrap();
        assert!(matches!(
            capture(dir.path(), &info),
            Err(PortabilityError::Incomplete(_))
        ));
        std::fs::remove_dir(dir.path().join("goal")).unwrap();
        std::fs::write(
            dir.path().join(st::CHAT_HISTORY_FILE),
            r#"{"type":"user","content":[{"type":"image","url":"file:///source/secret.png"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            capture(dir.path(), &info),
            Err(PortabilityError::Incomplete(_))
        ));
        std::fs::write(dir.path().join(st::CHAT_HISTORY_FILE), "malformed\n").unwrap();
        assert!(matches!(
            capture(dir.path(), &info),
            Err(PortabilityError::Malformed(_))
        ));
        std::fs::remove_file(dir.path().join(st::CHAT_HISTORY_FILE)).unwrap();
        assert!(matches!(
            capture(dir.path(), &info),
            Err(PortabilityError::Incomplete(_))
        ));
    }

    #[test]
    fn portable_decode_rejects_oversize_and_false_completeness() {
        assert!(matches!(
            PortableSession::from_slice(&vec![0; MAX_PORTABLE_BYTES + 1]),
            Err(PortabilityError::TooLarge)
        ));
        let (dir, info) = fixture();
        let mut snapshot = capture(dir.path(), &info).unwrap();
        snapshot.completeness.historical_model_context_branches = true;
        assert!(matches!(
            snapshot.validate(),
            Err(PortabilityError::Incomplete(_))
        ));
    }

    #[test]
    fn portable_publication_never_replaces_even_an_empty_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        let target = root.path().join("target");
        std::fs::create_dir(&stage).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(stage.join("payload"), "value").unwrap();
        assert!(publish(&stage, &target).is_err());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        assert_eq!(
            std::fs::read_to_string(stage.join("payload")).unwrap(),
            "value"
        );
    }
}
