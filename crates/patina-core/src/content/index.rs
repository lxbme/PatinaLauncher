//! Persistent, incrementally refreshed index of the mods installed in every
//! instance, with cached icons, plus the global mod search over it.
//!
//! The index lives in `<data>/cache/mod-index.json`; icons are stored once
//! per distinct image as `<data>/cache/mod-icons/<sha1>.png`.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use super::mods::{ModFormat, ModMeta, list_mod_files, read_mod_meta, split_mod_name};
use crate::fsutil::write_atomic;
use crate::instance::{Instance, InstanceStore};
use crate::loader::LoaderKind;
use crate::paths::DataDirs;

const INDEX_FILE: &str = "mod-index.json";
const ICON_DIR: &str = "mod-icons";
const INDEX_VERSION: u32 = 1;

/// One searchable mod file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModHit {
    pub instance_id: String,
    pub file_name: String,
    pub mod_id: Option<String>,
    pub name: String,
    pub version: Option<String>,
    /// Absolute path to the cached icon, when the mod has one.
    pub icon_path: Option<PathBuf>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshStats {
    /// Jars whose metadata was (re)read.
    pub parsed: usize,
    /// Entries kept because size and mtime were unchanged.
    pub reused: usize,
    /// Entries dropped (deleted files or instances).
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    instance_id: String,
    file_name: String,
    size: u64,
    mtime_nanos: u64,
    enabled: bool,
    mod_id: Option<String>,
    name: String,
    version: Option<String>,
    /// sha1 (lowercase hex) of the cached icon.
    icon: Option<String>,
    /// The jar has an icon that could not be cached; re-parse next time.
    #[serde(default)]
    icon_missing: bool,
    /// Loader preference the metadata was parsed with (`ModFormat` name).
    #[serde(default)]
    prefer: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    entries: Vec<Record>,
}

type Key = (String, String);

/// The index is a rebuildable cache: with several processes sharing a data
/// directory the last writer wins and the next refresh self-heals.
#[derive(Debug)]
pub struct ModIndex {
    cache_dir: PathBuf,
    records: BTreeMap<Key, Record>,
}

fn key(r: &Record) -> Key {
    (r.instance_id.clone(), r.file_name.clone())
}

fn prefer_of(inst: &Instance) -> Option<ModFormat> {
    inst.config.loader().map(|(kind, _)| match kind {
        LoaderKind::Forge => ModFormat::Forge,
        LoaderKind::Neoforge => ModFormat::NeoForge,
        LoaderKind::Fabric => ModFormat::Fabric,
    })
}

fn prefer_tag(prefer: Option<ModFormat>) -> Option<String> {
    prefer.map(|f| format!("{f:?}"))
}

fn is_icon_hash(h: &str) -> bool {
    h.len() == 40 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_icon_file_name(name: &str) -> bool {
    name.strip_suffix(".png").is_some_and(|stem| {
        stem.len() == 40 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

impl ModIndex {
    /// Loads the index; a missing, corrupt or other-version file yields an
    /// empty index.
    pub fn open(data: &DataDirs) -> ModIndex {
        let cache_dir = data.cache();
        let records = Self::load(&cache_dir).unwrap_or_default();
        ModIndex { cache_dir, records }
    }

    fn load(cache_dir: &std::path::Path) -> Option<BTreeMap<Key, Record>> {
        let path = cache_dir.join(INDEX_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!("cannot read mod index {}: {e}", path.display());
                }
                return None;
            }
        };
        match serde_json::from_str::<Stored>(&text) {
            Ok(s) if s.version == INDEX_VERSION => {
                Some(
                    s.entries
                        .into_iter()
                        .map(|mut r| {
                            // The file is untrusted: never let a crafted hash
                            // reach a path join.
                            if r.icon.as_deref().is_some_and(|h| !is_icon_hash(h)) {
                                r.icon = None;
                                r.icon_missing = true;
                            }
                            (key(&r), r)
                        })
                        .collect(),
                )
            }
            Ok(s) => {
                tracing::info!("ignoring mod index of version {}", s.version);
                None
            }
            Err(e) => {
                tracing::warn!("corrupt mod index {}, rebuilding: {e}", path.display());
                None
            }
        }
    }

    /// Brings the index up to date with the healthy instances of `store`.
    /// Only files whose size or mtime changed are parsed (in
    /// `spawn_blocking`, one batch per instance); entries of deleted files and
    /// of vanished or broken instances are dropped. A mods directory that
    /// cannot be listed keeps that instance's previous entries. Takes no
    /// instance locks. Saves the index and prunes unreferenced icons.
    pub async fn refresh(&mut self, store: &InstanceStore) -> io::Result<RefreshStats> {
        let lister = store.clone();
        let outcome = tokio::task::spawn_blocking(move || lister.list())
            .await
            .map_err(io::Error::other)?
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut stats = RefreshStats::default();
        let mut next: BTreeMap<Key, Record> = BTreeMap::new();
        for inst in &outcome.instances {
            let previous: BTreeMap<String, Record> = self
                .records
                .range((inst.id.clone(), String::new())..)
                .take_while(|((id, _), _)| *id == inst.id)
                .map(|((_, f), r)| (f.clone(), r.clone()))
                .collect();
            let batch = Batch {
                instance_id: inst.id.clone(),
                mods_dir: inst.game_dir().join("mods"),
                prefer: prefer_of(inst),
                icon_dir: self.cache_dir.join(ICON_DIR),
                previous,
            };
            let done = tokio::task::spawn_blocking(move || batch.run())
                .await
                .map_err(io::Error::other)?;
            match done {
                Some((records, parsed, reused)) => {
                    stats.parsed += parsed;
                    stats.reused += reused;
                    next.extend(records.into_iter().map(|r| (key(&r), r)));
                }
                None => {
                    next.extend(
                        self.records
                            .iter()
                            .filter(|((id, _), _)| *id == inst.id)
                            .map(|(k, r)| (k.clone(), r.clone())),
                    );
                }
            }
        }
        // A re-parsed entry replaces its predecessor; only vanished keys count.
        stats.removed = self
            .records
            .keys()
            .filter(|k| !next.contains_key(*k))
            .count();
        self.records = next;
        let cache_dir = self.cache_dir.clone();
        let records: Vec<Record> = self.records.values().cloned().collect();
        tokio::task::spawn_blocking(move || Self::persist(&cache_dir, records))
            .await
            .map_err(io::Error::other)??;
        Ok(stats)
    }

    /// Writes the index, then (only if that worked) prunes unreferenced icons.
    fn persist(cache_dir: &std::path::Path, records: Vec<Record>) -> io::Result<()> {
        std::fs::create_dir_all(cache_dir)?;
        let used: std::collections::HashSet<String> = records
            .iter()
            .filter_map(|r| r.icon.as_ref().map(|h| format!("{h}.png")))
            .collect();
        let stored = Stored {
            version: INDEX_VERSION,
            entries: records,
        };
        let json = serde_json::to_vec(&stored).map_err(io::Error::other)?;
        write_atomic(&cache_dir.join(INDEX_FILE), &json)?;
        Self::prune_icons(cache_dir, &used);
        Ok(())
    }

    /// Deletes cached icons that no entry references (only `<40 hex>.png`).
    fn prune_icons(cache_dir: &std::path::Path, used: &std::collections::HashSet<String>) {
        let dir = cache_dir.join(ICON_DIR);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in rd.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if is_icon_file_name(&name)
                && !used.contains(&name)
                && let Err(e) = std::fs::remove_file(entry.path())
            {
                tracing::warn!("cannot remove stale icon {name}: {e}");
            }
        }
    }

    /// Case-insensitive search over name, mod id and file name. The query is
    /// trimmed; an empty query finds nothing. Ranking: name prefix, then exact
    /// mod id, then substring; ties by name, instance id and file name.
    pub fn search(&self, query: &str, limit: usize) -> Vec<ModHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(u8, String, &Record)> = Vec::new();
        for r in self.records.values() {
            let name = r.name.to_lowercase();
            let id = r.mod_id.as_deref().map(str::to_lowercase);
            let tier = if name.starts_with(&q) {
                0
            } else if id.as_deref() == Some(q.as_str()) {
                1
            } else if name.contains(&q)
                || id.is_some_and(|i| i.contains(&q))
                || r.file_name.to_lowercase().contains(&q)
            {
                2
            } else {
                continue;
            };
            found.push((tier, name, r));
        }
        found.sort_by(|a, b| {
            (a.0, &a.1, &a.2.instance_id, &a.2.file_name).cmp(&(
                b.0,
                &b.1,
                &b.2.instance_id,
                &b.2.file_name,
            ))
        });
        found
            .into_iter()
            .take(limit)
            .map(|(_, _, r)| {
                let icon_path = r
                    .icon
                    .as_ref()
                    .map(|h| self.cache_dir.join(ICON_DIR).join(format!("{h}.png")))
                    .filter(|p| p.is_file());
                ModHit {
                    instance_id: r.instance_id.clone(),
                    file_name: r.file_name.clone(),
                    mod_id: r.mod_id.clone(),
                    name: r.name.clone(),
                    version: r.version.clone(),
                    icon_path,
                    enabled: r.enabled,
                }
            })
            .collect()
    }
}

/// One instance's refresh work, run on a blocking thread.
struct Batch {
    instance_id: String,
    mods_dir: PathBuf,
    prefer: Option<ModFormat>,
    icon_dir: PathBuf,
    previous: BTreeMap<String, Record>,
}

impl Batch {
    /// `None` when the directory cannot be listed. Otherwise the new records
    /// plus how many were parsed and reused.
    fn run(self) -> Option<(Vec<Record>, usize, usize)> {
        let files = match list_mod_files(&self.mods_dir) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "cannot list mods of instance {}: {e}; keeping previous entries",
                    self.instance_id
                );
                return None;
            }
        };
        let (mut parsed, mut reused) = (0, 0);
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            if let Some(prev) = self.previous.get(&f.file_name)
                && prev.size == f.size
                && prev.mtime_nanos == f.modified_nanos
                && prev.enabled == f.enabled
                && prev.prefer == prefer_tag(self.prefer)
                && !prev.icon_missing
                && prev
                    .icon
                    .as_ref()
                    .is_none_or(|h| self.icon_dir.join(format!("{h}.png")).is_file())
            {
                reused += 1;
                out.push(prev.clone());
                continue;
            }
            parsed += 1;
            let meta = read_mod_meta(&f.path, self.prefer);
            out.push(self.record(&f, meta));
        }
        Some((out, parsed, reused))
    }

    fn record(&self, f: &super::mods::ModFile, meta: Option<ModMeta>) -> Record {
        let stem = split_mod_name(&f.file_name).map_or(f.file_name.as_str(), |(s, _)| s);
        let (mod_id, name, version, icon, icon_missing) = match meta {
            Some(m) => {
                let icon = m.icon.as_deref().and_then(|b| self.store_icon(b));
                let missing = m.icon.is_some() && icon.is_none();
                (m.id, m.name, m.version, icon, missing)
            }
            None => (None, stem.to_string(), None, None, false),
        };
        Record {
            instance_id: self.instance_id.clone(),
            file_name: f.file_name.clone(),
            size: f.size,
            mtime_nanos: f.modified_nanos,
            enabled: f.enabled,
            mod_id,
            name,
            version,
            icon,
            icon_missing,
            prefer: prefer_tag(self.prefer),
        }
    }

    /// Writes the icon once under its sha1; `None` when it cannot be stored.
    fn store_icon(&self, bytes: &[u8]) -> Option<String> {
        let hash = hex::encode(Sha1::digest(bytes));
        let path = self.icon_dir.join(format!("{hash}.png"));
        if !path.is_file() {
            let written =
                std::fs::create_dir_all(&self.icon_dir).and_then(|()| write_atomic(&path, bytes));
            if let Err(e) = written {
                tracing::warn!("cannot cache mod icon {hash}: {e}");
                return None;
            }
        }
        Some(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::forge::test_support::write_zip;

    struct Env {
        _tmp: tempfile::TempDir,
        data: DataDirs,
        store: InstanceStore,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let store = InstanceStore::new(data.instances());
        Env {
            _tmp: tmp,
            data,
            store,
        }
    }

    fn png(extra: u8) -> Vec<u8> {
        let mut v = crate::content::PNG_SIGNATURE.to_vec();
        v.extend([extra; 8]);
        v
    }

    fn add_mod(inst: &Instance, file: &str, id: &str, name: &str, icon: Option<&[u8]>) {
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"id":"{id}","name":"{name}","version":"1.0"{}}}"#,
            if icon.is_some() {
                r#","icon":"icon.png""#
            } else {
                ""
            }
        );
        let mut entries: Vec<(&str, &[u8])> = vec![("fabric.mod.json", json.as_bytes())];
        if let Some(i) = icon {
            entries.push(("icon.png", i));
        }
        write_zip(&dir.join(file), &entries);
    }

    fn icon_files(e: &Env) -> Vec<String> {
        let d = e.data.cache().join(ICON_DIR);
        let Ok(rd) = std::fs::read_dir(d) else {
            return Vec::new();
        };
        let mut v: Vec<String> = rd
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn refresh_is_incremental() {
        let e = env();
        let inst = e.store.create("Pack", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        add_mod(&inst, "b.jar", "b", "Beta", None);
        let mut idx = ModIndex::open(&e.data);
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused, s.removed), (2, 0, 0));
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused, s.removed), (0, 2, 0));
        // Same size, newer mtime: still detected.
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        let f = std::fs::File::options()
            .write(true)
            .open(inst.game_dir().join("mods/a.jar"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(f);
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused), (1, 1));
        // The state survives a reopen.
        let mut idx2 = ModIndex::open(&e.data);
        let s = idx2.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused), (0, 2));
        // A deleted file is removed.
        std::fs::remove_file(inst.game_dir().join("mods/b.jar")).unwrap();
        let s = idx2.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused, s.removed), (0, 1, 1));
        assert!(idx2.search("beta", 10).is_empty());
    }

    #[tokio::test]
    async fn removed_instance_is_dropped() {
        let e = env();
        let a = e.store.create("A", "1.20.1").unwrap();
        let b = e.store.create("B", "1.20.1").unwrap();
        add_mod(&a, "a.jar", "a", "Alpha", None);
        add_mod(&b, "b.jar", "b", "Beta", None);
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(idx.search("a", 10).len(), 2);
        std::fs::remove_dir_all(&b.dir).unwrap();
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused, s.removed), (0, 1, 1));
        let hits = idx.search("a", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].instance_id, "A");
    }

    #[tokio::test]
    async fn search_ranking() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "z1.jar", "zzz", "Sodium Extra", None);
        add_mod(&inst, "z2.jar", "sodium", "Rendering Engine", None);
        add_mod(&inst, "z3.jar", "other", "Super Sodium Tweaks", None);
        add_mod(&inst, "z4.jar", "sodium-x", "Sodium", None);
        add_mod(&inst, "z5.jar", "q", "Unrelated", None);
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        let names: Vec<String> = idx
            .search("  SODIUM ", 10)
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(
            names,
            [
                "Sodium",
                "Sodium Extra",
                "Rendering Engine",
                "Super Sodium Tweaks"
            ]
        );
        assert_eq!(idx.search("sodium", 2).len(), 2);
        assert!(idx.search("   ", 10).is_empty());
        assert_eq!(idx.search("z5.JAR", 10).len(), 1);
    }

    #[tokio::test]
    async fn corrupt_index_rebuilds() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        std::fs::create_dir_all(e.data.cache()).unwrap();
        for body in ["{ not json", r#"{"version":99,"entries":[]}"#] {
            std::fs::write(e.data.cache().join(INDEX_FILE), body).unwrap();
            let mut idx = ModIndex::open(&e.data);
            assert!(idx.search("alpha", 10).is_empty());
            let s = idx.refresh(&e.store).await.unwrap();
            assert_eq!(s.parsed, 1);
            assert_eq!(idx.search("alpha", 10).len(), 1);
        }
    }

    #[tokio::test]
    async fn icons_cached_by_hash() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        let same = png(1);
        add_mod(&inst, "a.jar", "a", "Alpha", Some(&same));
        add_mod(&inst, "b.jar", "b", "Beta", Some(&same));
        add_mod(&inst, "c.jar", "c", "Gamma", Some(&png(2)));
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(icon_files(&e).len(), 2);
        let hit = &idx.search("alpha", 1)[0];
        let p = hit.icon_path.clone().unwrap();
        assert!(p.starts_with(e.data.cache()));
        assert_eq!(std::fs::read(&p).unwrap(), same);
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            format!("{}.png", hex::encode(Sha1::digest(&same)))
        );
        // A stray non-icon file is never deleted; unreferenced icons are.
        let stray = e.data.cache().join(ICON_DIR).join("keep.txt");
        std::fs::write(&stray, "x").unwrap();
        std::fs::remove_file(inst.game_dir().join("mods/c.jar")).unwrap();
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(icon_files(&e).len(), 2); // 1 icon + keep.txt
        assert!(stray.exists());
    }

    #[tokio::test]
    async fn unreadable_mods_dir_keeps_previous_entries() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        // `mods` becomes a regular file: listing fails with a non-NotFound error.
        let mods = inst.game_dir().join("mods");
        std::fs::remove_dir_all(&mods).unwrap();
        std::fs::write(&mods, "x").unwrap();
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!(s.removed, 0);
        assert_eq!(idx.search("alpha", 10).len(), 1);
    }

    fn read_stored(e: &Env) -> serde_json::Value {
        let t = std::fs::read_to_string(e.data.cache().join(INDEX_FILE)).unwrap();
        serde_json::from_str(&t).unwrap()
    }

    #[tokio::test]
    async fn crafted_icon_hash_is_neutralised() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        let mut v = read_stored(&e);
        v["entries"][0]["icon"] = serde_json::json!("../../x");
        std::fs::write(e.data.cache().join(INDEX_FILE), v.to_string()).unwrap();
        let mut idx = ModIndex::open(&e.data);
        assert_eq!(idx.search("alpha", 1)[0].icon_path, None);
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused), (1, 0));
    }

    #[tokio::test]
    async fn loader_switch_reparses() {
        let e = env();
        let mut inst = e.store.create("P", "1.20.1").unwrap();
        let dir = inst.game_dir().join("mods");
        std::fs::create_dir_all(&dir).unwrap();
        write_zip(
            &dir.join("m.jar"),
            &[
                ("fabric.mod.json", br#"{"id":"f","name":"FabricName"}"#),
                (
                    "META-INF/mods.toml",
                    b"[[mods]]\nmodId=\"g\"\ndisplayName=\"ForgeName\"\n",
                ),
            ],
        );
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(idx.search("fabricname", 1).len(), 1);
        let comp = crate::instance::Component {
            uid: crate::instance::ComponentUid::Forge,
            version: "47.1.0".into(),
            profile: Some("forge".into()),
        };
        inst.config.set_loader(Some(comp)).unwrap();
        e.store.save(&inst).unwrap();
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused), (1, 0));
        assert_eq!(idx.search("forgename", 1).len(), 1);
        assert!(idx.search("fabricname", 1).is_empty());
    }

    #[tokio::test]
    async fn icon_store_failure_is_retried() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", Some(&png(1)));
        // A regular file where the icon directory should be.
        std::fs::create_dir_all(e.data.cache()).unwrap();
        std::fs::write(e.data.cache().join(ICON_DIR), "x").unwrap();
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(idx.search("alpha", 1)[0].icon_path, None);
        std::fs::remove_file(e.data.cache().join(ICON_DIR)).unwrap();
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.reused), (1, 0));
        assert!(idx.search("alpha", 1)[0].icon_path.is_some());
    }

    #[tokio::test]
    async fn icon_survives_when_mods_dir_unreadable() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", Some(&png(1)));
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        let mods = inst.game_dir().join("mods");
        std::fs::remove_dir_all(&mods).unwrap();
        std::fs::write(&mods, "x").unwrap();
        idx.refresh(&e.store).await.unwrap();
        assert_eq!(icon_files(&e).len(), 1);
        assert!(idx.search("alpha", 1)[0].icon_path.is_some());
    }

    #[tokio::test]
    async fn failed_index_write_errs_and_prunes_nothing() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        let icons = e.data.cache().join(ICON_DIR);
        std::fs::create_dir_all(&icons).unwrap();
        let stale = icons.join(format!("{}.png", "0".repeat(40)));
        std::fs::write(&stale, png(9)).unwrap();
        // A directory where the index file goes: the atomic rename fails.
        std::fs::create_dir(e.data.cache().join(INDEX_FILE)).unwrap();
        let mut idx = ModIndex::open(&e.data);
        assert!(idx.refresh(&e.store).await.is_err());
        assert!(stale.exists());
    }

    #[tokio::test]
    async fn disable_rename_updates_enabled() {
        let e = env();
        let inst = e.store.create("P", "1.20.1").unwrap();
        add_mod(&inst, "a.jar", "a", "Alpha", None);
        let mut idx = ModIndex::open(&e.data);
        idx.refresh(&e.store).await.unwrap();
        assert!(idx.search("alpha", 1)[0].enabled);
        let mods = inst.game_dir().join("mods");
        std::fs::rename(mods.join("a.jar"), mods.join("a.jar.disabled")).unwrap();
        let s = idx.refresh(&e.store).await.unwrap();
        assert_eq!((s.parsed, s.removed), (1, 1));
        let hits = idx.search("alpha", 5);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].enabled);
        std::fs::rename(mods.join("a.jar.disabled"), mods.join("a.jar")).unwrap();
        idx.refresh(&e.store).await.unwrap();
        assert!(idx.search("alpha", 5)[0].enabled);
    }
}
