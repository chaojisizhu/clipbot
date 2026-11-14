//! Tracks only Markdown image materials created by this application.
//!
//! Callers serialize access with their application-wide `Mutex<OwnedMaterials>`.
//! Reading due IDs does not transfer ownership: an ID stays recorded until QQ
//! confirms deletion (or reports that the material no longer exists).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MAX_OWNED_MATERIALS: usize = 100;
pub const DEFAULT_DELETE_DELAY_SECS: u64 = 300;
const LEDGER_NAME: &str = ".allvideo-markdown-materials.json";
const MAX_LEDGER_BYTES: u64 = 256 * 1024;
const MAX_ID_BYTES: usize = 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedMaterial {
    pub id: String,
    pub delete_after_unix: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    entries: Vec<OwnedMaterial>,
}

/// A bounded, persistent ownership ledger; never a list of all bot materials.
#[derive(Debug)]
pub struct OwnedMaterials {
    path: PathBuf,
    entries: Vec<OwnedMaterial>,
    dirty: bool,
}

impl OwnedMaterials {
    /// Loads the existing ledger, or atomically creates a private empty one.
    /// Invalid/unreadable existing files are left intact and return an error.
    pub fn new(directory: &Path) -> Result<Self> {
        let path = directory.join(LEDGER_NAME);
        let entries = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    bail!("Markdown 素材记录不是普通文件，拒绝覆盖");
                }
                if metadata.len() > MAX_LEDGER_BYTES {
                    bail!("Markdown 素材记录过大，拒绝覆盖");
                }
                let mut bytes = Vec::new();
                File::open(&path)
                    .context("读取 Markdown 素材记录失败")?
                    .take(MAX_LEDGER_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .context("读取 Markdown 素材记录失败")?;
                if bytes.len() as u64 > MAX_LEDGER_BYTES {
                    bail!("Markdown 素材记录过大，拒绝覆盖");
                }
                let ledger: Ledger = serde_json::from_slice(&bytes)
                    .context("Markdown 素材记录损坏，原文件已保留")?;
                if ledger.version != 1 {
                    bail!("Markdown 素材记录版本不支持，原文件已保留");
                }
                validate_entries(&ledger.entries)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                        .context("设置 Markdown 素材记录权限失败")?;
                }
                ledger.entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut owned = Self {
                    path,
                    entries: Vec::new(),
                    dirty: true,
                };
                owned.flush()?;
                return Ok(owned);
            }
            Err(error) => return Err(error).context("检查 Markdown 素材记录失败"),
        };
        Ok(Self {
            path,
            entries,
            dirty: false,
        })
    }

    pub fn entries(&self) -> &[OwnedMaterial] {
        &self.entries
    }

    /// A failed save keeps newly acquired ownership in memory. Stop creating
    /// new materials until deletion/flush repairs the persistent record.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Check this before uploading, while also accounting for in-flight work
    /// in the caller. No old ID is ever evicted to make room for a new one.
    pub fn ensure_capacity(&self, additional: usize) -> Result<()> {
        if self.dirty {
            bail!("Markdown 素材记录尚未写入，暂停创建新素材");
        }
        if additional > MAX_OWNED_MATERIALS.saturating_sub(self.entries.len()) {
            bail!("Markdown 素材记录已达上限，请先清理本项目到期素材");
        }
        Ok(())
    }

    pub fn track(&mut self, id: &str) -> Result<()> {
        self.track_at(id, unix_now()?.saturating_add(DEFAULT_DELETE_DELAY_SECS))
    }

    pub fn track_at(&mut self, id: &str, delete_after_unix: u64) -> Result<()> {
        validate_id(id)?;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            // Re-acquiring the same platform ID must not shorten the previous
            // delivery uncertainty window.
            entry.delete_after_unix = entry.delete_after_unix.max(delete_after_unix);
        } else {
            self.ensure_capacity(1)?;
            self.entries.push(OwnedMaterial {
                id: id.to_owned(),
                delete_after_unix,
            });
        }
        self.dirty = true;
        self.flush()
    }

    /// Returns a snapshot, without removing IDs or changing their deadline.
    pub fn due(&self, now: u64) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.delete_after_unix <= now)
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Changes deadlines only for IDs already owned by this ledger.
    pub fn mark_due(&mut self, ids: &[String], when: u64) -> Result<()> {
        for entry in &mut self.entries {
            if ids.iter().any(|id| id == &entry.id) && entry.delete_after_unix != when {
                entry.delete_after_unix = when;
                self.dirty = true;
            }
        }
        self.flush_if_dirty()
    }

    /// Call only after the platform confirms successful deletion or absence.
    pub fn mark_removed(&mut self, ids: &[String]) -> Result<()> {
        let previous_len = self.entries.len();
        self.entries
            .retain(|entry| !ids.iter().any(|id| id == &entry.id));
        if self.entries.len() != previous_len {
            self.dirty = true;
        }
        self.flush_if_dirty()
    }

    pub fn flush(&mut self) -> Result<()> {
        validate_entries(&self.entries)?;
        let bytes = serde_json::to_vec(&Ledger {
            version: 1,
            entries: self.entries.clone(),
        })?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            bail!("Markdown 素材记录过大，拒绝写入");
        }
        atomic_write(&self.path, &bytes)?;
        self.dirty = false;
        Ok(())
    }

    fn flush_if_dirty(&mut self) -> Result<()> {
        if self.dirty {
            self.flush()?;
        }
        Ok(())
    }
}

pub fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix 纪元，无法记录素材清理期限")?
        .as_secs())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("Markdown 素材 ID 为空、过长或包含非法字符");
    }
    Ok(())
}

fn validate_entries(entries: &[OwnedMaterial]) -> Result<()> {
    if entries.len() > MAX_OWNED_MATERIALS {
        bail!("Markdown 素材记录超过上限，原记录已保留");
    }
    let mut ids = HashSet::with_capacity(entries.len());
    for entry in entries {
        validate_id(&entry.id)?;
        if !ids.insert(entry.id.as_str()) {
            bail!("Markdown 素材记录存在重复 ID，原记录已保留");
        }
    }
    Ok(())
}

struct PendingFile(PathBuf);

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Markdown 素材记录缺少父目录")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .context("创建 Markdown 素材记录目录失败")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!("Markdown 素材记录不是普通文件，拒绝覆盖");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("检查 Markdown 素材记录失败"),
    }
    let temporary = parent.join(format!(
        ".allvideo-markdown-materials-{:032x}.tmp",
        rand::random::<u128>()
    ));
    let _pending = PendingFile(temporary.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("创建私有 Markdown 素材记录失败")?;
    file.write_all(bytes)
        .context("写入 Markdown 素材记录失败")?;
    file.sync_all().context("同步 Markdown 素材记录失败")?;
    drop(file);
    fs::rename(&temporary, path).context("原子更新 Markdown 素材记录失败")?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("同步 Markdown 素材记录目录失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_survives_reload_until_confirmed_removed() {
        let directory = tempfile::tempdir().unwrap();
        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        owned.track_at("our-image", 300).unwrap();
        assert!(owned.due(299).is_empty());
        assert_eq!(owned.due(300), ["our-image"]);
        assert_eq!(owned.due(301), ["our-image"]);
        drop(owned);

        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        assert_eq!(owned.due(300), ["our-image"]);
        owned.mark_due(&["unowned-image".into()], 0).unwrap();
        assert_eq!(owned.entries()[0].delete_after_unix, 300);
        owned.mark_due(&["our-image".into()], 0).unwrap();
        assert_eq!(owned.due(0), ["our-image"]);
        owned.mark_removed(&["unowned-image".into()]).unwrap();
        assert_eq!(owned.entries().len(), 1);
        owned.mark_removed(&["our-image".into()]).unwrap();
        assert!(
            OwnedMaterials::new(directory.path())
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn default_deadline_and_duplicate_tracking_keep_uncertainty_window() {
        let directory = tempfile::tempdir().unwrap();
        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        let before = unix_now().unwrap();
        owned.track("image").unwrap();
        let deadline = owned.entries()[0].delete_after_unix;
        assert!(deadline >= before + DEFAULT_DELETE_DELAY_SECS);
        assert!(deadline <= unix_now().unwrap() + DEFAULT_DELETE_DELAY_SECS);
        owned.track_at("image", 0).unwrap();
        assert_eq!(owned.entries().len(), 1);
        assert_eq!(owned.entries()[0].delete_after_unix, deadline);
    }

    #[test]
    fn corrupt_or_unsupported_existing_records_are_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LEDGER_NAME);
        for bytes in [
            b"{broken".as_slice(),
            br#"{"version":2,"entries":[]}"#.as_slice(),
            br#"{"version":1,"entries":[{"id":"same","delete_after_unix":1},{"id":"same","delete_after_unix":2}]}"#.as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(OwnedMaterials::new(directory.path()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn capacity_never_evicts_owned_ids() {
        let directory = tempfile::tempdir().unwrap();
        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        for index in 0..MAX_OWNED_MATERIALS {
            owned.track_at(&format!("image-{index}"), 300).unwrap();
        }
        let before = owned.entries().to_vec();
        assert!(owned.ensure_capacity(1).is_err());
        assert!(owned.track_at("image-too-many", 300).is_err());
        assert_eq!(owned.entries(), before);
        assert_eq!(
            OwnedMaterials::new(directory.path()).unwrap().entries(),
            before
        );
    }

    #[test]
    fn failed_save_keeps_new_ownership_and_blocks_more_uploads() {
        let directory = tempfile::tempdir().unwrap();
        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        let backup = directory.path().join("ledger-backup");
        fs::rename(&owned.path, &backup).unwrap();
        fs::create_dir(&owned.path).unwrap();

        assert!(owned.track_at("upload-completed", 300).is_err());
        assert_eq!(owned.entries()[0].id, "upload-completed");
        assert!(owned.is_dirty());
        assert!(owned.ensure_capacity(1).is_err());
        assert!(owned.track_at("must-not-upload-again", 300).is_err());
        assert_eq!(owned.entries().len(), 1);

        fs::remove_dir(&owned.path).unwrap();
        fs::rename(backup, &owned.path).unwrap();
        owned.flush().unwrap();
        assert!(!owned.is_dirty());
        assert_eq!(
            OwnedMaterials::new(directory.path()).unwrap().entries(),
            owned.entries()
        );
    }

    #[test]
    fn repeated_updates_replace_atomically_without_leaving_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut owned = OwnedMaterials::new(directory.path()).unwrap();
        owned.track_at("first", 300).unwrap();
        owned.track_at("second", 301).unwrap();
        owned.mark_removed(&["first".into()]).unwrap();
        assert_eq!(
            OwnedMaterials::new(directory.path()).unwrap().entries(),
            owned.entries()
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&owned.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn application_mutex_serializes_concurrent_workers() {
        let directory = tempfile::tempdir().unwrap();
        let owned = std::sync::Arc::new(std::sync::Mutex::new(
            OwnedMaterials::new(directory.path()).unwrap(),
        ));
        let mut workers = Vec::new();
        for worker in 0..4 {
            let owned = owned.clone();
            workers.push(std::thread::spawn(move || {
                for image in 0..4 {
                    owned
                        .lock()
                        .unwrap()
                        .track_at(&format!("{worker}-{image}"), 300)
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let owned = owned.lock().unwrap();
        assert_eq!(owned.entries().len(), 16);
        assert_eq!(
            OwnedMaterials::new(directory.path()).unwrap().entries(),
            owned.entries()
        );
    }
}
