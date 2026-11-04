use super::{service::*, state::*, updates::*, *};
use crate::{
    accounts::login::CancelToken,
    progress::{Phase, ProgressSink, Unit, report},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

impl PackService {
    pub async fn apply_update(
        &self,
        plan: PackUpdatePlan,
        decisions: PackDecisions,
        mut cancel: CancelToken,
        progress: ProgressSink,
    ) -> Result<PackOutcome, PackError> {
        check_cancel(&cancel)?;
        let guard = self.store.lock(&plan.instance.id)?;
        crate::launch::running::refuse_while_running(&plan.instance.dir, &plan.instance.id)?;
        // Shared resource preparation is auth-free and writes only cache/staging, not the old game.
        let components = self
            .prepare_components(
                &plan.preview,
                &plan.stage,
                Some(&plan.instance.config),
                &progress,
                &mut cancel,
            )
            .await?;
        check_cancel(&cancel)?;
        let store = self.store.clone();
        blocking(move || {
            let _guard = guard;
            apply(&store, plan, decisions, components, progress)
        })
        .await
    }
}
pub(super) fn apply(
    store: &crate::instance::InstanceStore,
    plan: PackUpdatePlan,
    decisions: PackDecisions,
    components: Vec<crate::instance::Component>,
    progress: ProgressSink,
) -> Result<PackOutcome, PackError> {
    let i = &plan.instance;
    let game = i.game_dir();
    crate::launch::running::refuse_while_running(&i.dir, &i.id)?;
    if plan.old.ready {
        ensure_ready(&i.dir)?;
    } else if !i.dir.join(PENDING_FILE).is_file() {
        return Err(PackError::NotReady);
    }
    // Keep settings writes out of the short backup/apply/rollback section as well.
    let _config_guard = crate::instance::lock_config(&i.dir)?;
    if metadata_snapshot(&i.dir)? != plan.metadata {
        return Err(PackError::StalePlan);
    }
    for (path, expected) in &plan.observed {
        if fingerprint(&checked_game_path(&game, path)?)? != *expected {
            return Err(PackError::StalePlan);
        }
    }
    let mut actions = BTreeMap::<String, Option<PathBuf>>::new();
    let mut target = plan.target.clone();
    target.components = components.clone();
    target.ready = true;
    for change in &plan.changes {
        let decision = decisions.decision(change);
        if decision == PackDecision::KeepLocal {
            target.retained.insert(
                change
                    .target_path
                    .as_ref()
                    .unwrap_or(&change.current_path)
                    .clone(),
                change.current.clone(),
            );
            if let Some(path) = &change.target_path
                && path != &change.current_path
            {
                target
                    .bindings
                    .insert(path.clone(), change.current_path.clone());
            }
            continue;
        }
        if change.current == change.target
            && change.target_path.as_deref() == Some(change.current_path.as_str())
        {
            continue;
        }
        if change.current.is_some() {
            actions.insert(change.current_path.clone(), None);
        }
        if let Some(path) = &change.target_path {
            let staged = plan.stage.path.join("files").join(path);
            if fingerprint(&staged)? != change.target {
                return Err(PackError::Verification);
            }
            actions.insert(path.clone(), Some(staged));
        }
    }
    // Never replace/delete an existing user world, even via a bulk UsePack choice.
    for path in actions.keys() {
        if path.starts_with("saves/") {
            let world = path.split('/').nth(1).ok_or(PackError::Invalid)?;
            if game.join("saves").join(world).exists() {
                return Err(PackError::Collision(path.clone()));
            }
        }
        checked_game_path(&game, path)?;
    }
    let recovery_root = i.dir.join("modpack-recovery");
    if std::fs::symlink_metadata(&recovery_root).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(PackError::UnsafePath("modpack-recovery".into()));
    }
    let recovery = recovery_root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&recovery)?;
    let mut backups = BTreeMap::<String, bool>::new();
    let backup_result = (|| {
        for path in actions.keys() {
            let original = checked_game_path(&game, path)?;
            let exists = fingerprint(&original)?.is_some();
            if exists {
                let dest = recovery.join("files").join(path);
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(original, dest)?;
            }
            backups.insert(path.clone(), exists);
        }
        for (name, bytes) in &plan.metadata {
            if let Some(bytes) = bytes {
                crate::fsutil::write_atomic(&recovery.join("metadata").join(name), bytes)?;
            }
        }
        crate::fsutil::write_atomic(
            &recovery.join("affected.json"),
            &serde_json::to_vec(&backups)?,
        )?;
        Ok::<_, PackError>(())
    })();
    if let Err(e) = backup_result {
        let _ = std::fs::remove_dir_all(&recovery);
        return Err(e);
    }
    let still_current = (|| {
        if metadata_snapshot(&i.dir)? != plan.metadata {
            return Err(PackError::StalePlan);
        }
        for (path, expected) in &plan.observed {
            if fingerprint(&checked_game_path(&game, path)?)? != *expected {
                return Err(PackError::StalePlan);
            }
        }
        Ok::<_, PackError>(())
    })();
    if let Err(error) = still_current {
        let _ = std::fs::remove_dir_all(&recovery);
        return Err(error);
    }
    crate::fsutil::write_atomic(
        &i.dir.join(PENDING_FILE),
        recovery.to_string_lossy().as_bytes(),
    )?;
    report(&progress, Phase::Finalize, 0, 1, Unit::Items);
    let mut created_dirs = BTreeSet::new();
    let result = (|| {
        for (path, source) in &actions {
            let dest = checked_game_path(&game, path)?;
            if let Some(source) = source {
                if let Some(parent) = dest.parent() {
                    let mut missing = parent;
                    while missing != game && !missing.exists() {
                        created_dirs.insert(missing.to_path_buf());
                        missing = missing.parent().ok_or(PackError::Invalid)?;
                    }
                    std::fs::create_dir_all(parent)?;
                }
                crate::fsutil::replace_atomic(&dest, |tmp| std::fs::copy(source, tmp).map(drop))?;
            } else if dest.exists() {
                std::fs::remove_file(dest)?;
            }
            #[cfg(test)]
            fault("apply")?;
        }
        let mut updated = store.load(&i.id)?;
        updated.config.components = components;
        store.save(&updated)?;
        save_pack_sources(i, &target, actions.keys().cloned().collect())?;
        save_state(&i.dir, &target)?;
        #[cfg(test)]
        fault("commit")?;
        Ok::<_, PackError>(())
    })();
    if let Err(error) = result {
        let rollback = (|| {
            for (path, existed) in &backups {
                #[cfg(test)]
                fault("rollback")?;
                let dest = checked_game_path(&game, path)?;
                if *existed {
                    crate::fsutil::replace_atomic(&dest, |tmp| {
                        std::fs::copy(recovery.join("files").join(path), tmp).map(drop)
                    })?;
                } else {
                    match std::fs::remove_file(dest) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            for (name, bytes) in plan
                .metadata
                .iter()
                .filter(|(name, _)| name.as_str() != PENDING_FILE)
            {
                #[cfg(test)]
                if name == STATE_FILE {
                    fault("restore-state")?;
                }
                match bytes {
                    Some(bytes) => crate::fsutil::write_atomic(&i.dir.join(name), bytes)?,
                    None => match std::fs::remove_file(i.dir.join(name)) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    },
                }
            }
            for dir in created_dirs.iter().rev() {
                let _ = std::fs::remove_dir(dir);
            }
            // Keep the recovery barrier until every file and ordinary metadata restore succeeds.
            match plan.metadata.get(PENDING_FILE).and_then(Option::as_deref) {
                Some(previous) => crate::fsutil::write_atomic(&i.dir.join(PENDING_FILE), previous)?,
                None => std::fs::remove_file(i.dir.join(PENDING_FILE))?,
            }
            Ok::<_, PackError>(())
        })();
        if let Err(restore) = rollback {
            return Err(PackError::Recovery {
                path: recovery,
                error: format!("{error}; {restore}"),
            });
        }
        let _ = std::fs::remove_dir_all(recovery);
        return Err(error);
    }
    if let Err(e) = std::fs::remove_file(i.dir.join(PENDING_FILE)) {
        return Err(PackError::Recovery {
            path: recovery,
            error: e.to_string(),
        });
    }
    let _ = std::fs::remove_dir_all(recovery);
    report(&progress, Phase::Finalize, 1, 1, Unit::Items);
    Ok(PackOutcome {
        instance: store.load(&i.id)?,
        retained: target
            .retained
            .keys()
            .map(|p| target.bindings.get(p).unwrap_or(p))
            .chain(plan.preserved.iter())
            .collect::<BTreeSet<_>>()
            .len(),
    })
}

#[cfg(test)]
thread_local! {static FAILURE:std::cell::Cell<u8>=const{std::cell::Cell::new(0)};}
#[cfg(test)]
fn fault(stage: &str) -> Result<(), PackError> {
    FAILURE.with(|f| match (f.get(), stage) {
        (1, "apply") => {
            f.set(0);
            Err(PackError::Io("injected apply failure".into()))
        }
        (3, "apply") => {
            f.set(4);
            Ok(())
        }
        (4, "apply") | (5, "commit") => {
            f.set(0);
            Err(PackError::Io("injected late failure".into()))
        }
        (6, "commit") => {
            f.set(7);
            Err(PackError::Io(
                "injected commit failure before metadata rollback".into(),
            ))
        }
        (7, "restore-state") => {
            f.set(0);
            Err(PackError::Io(
                "injected final metadata restore failure".into(),
            ))
        }
        (2, "apply") | (2, "rollback") => Err(PackError::Io(format!("injected {stage} failure"))),
        _ => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{instance::InstanceStore, modpack::model::PackPreview};
    use sha2::{Digest, Sha512};
    use std::sync::Arc;
    pub(super) fn plan() -> (tempfile::TempDir, InstanceStore, PackUpdatePlan) {
        let tmp = tempfile::tempdir().unwrap();
        let store = InstanceStore::new(tmp.path().join("instances"));
        let i = store.create("Pack", "1").unwrap();
        std::fs::create_dir_all(i.game_dir().join("config")).unwrap();
        std::fs::write(i.game_dir().join("config/a"), b"old").unwrap();
        let old_hash = hex::encode(Sha512::digest(b"old"));
        let new_hash = hex::encode(Sha512::digest(b"new"));
        let old = PackState {
            schema: 1,
            archive_digest: String::new(),
            name: "Pack".into(),
            version: "1".into(),
            source: None,
            components: i.config.components.clone(),
            files: BTreeMap::from([(
                "config/a".into(),
                BaselineFile {
                    sha512: old_hash.clone(),
                    source: None,
                },
            )]),
            optional: BTreeMap::new(),
            ready: true,
            retained: BTreeMap::new(),
            bindings: BTreeMap::new(),
        };
        save_state(&i.dir, &old).unwrap();
        let mut target = old.clone();
        target.version = "2".into();
        target.files.insert(
            "config/a".into(),
            BaselineFile {
                sha512: new_hash.clone(),
                source: None,
            },
        );
        target.files.insert(
            "new.txt".into(),
            BaselineFile {
                sha512: new_hash.clone(),
                source: None,
            },
        );
        let stage = Staging {
            path: tmp.path().join("stage"),
        };
        std::fs::create_dir_all(stage.path.join("files/config")).unwrap();
        std::fs::write(stage.path.join("files/config/a"), b"new").unwrap();
        std::fs::write(stage.path.join("files/new.txt"), b"new").unwrap();
        let changes = vec![
            PackChange {
                id: "config/a".into(),
                current_path: "config/a".into(),
                target_path: Some("config/a".into()),
                kind: PackChangeKind::FollowPack,
                reason: None,
                current: Some(old_hash.clone()),
                target: Some(new_hash.clone()),
            },
            PackChange {
                id: "new.txt".into(),
                current_path: "new.txt".into(),
                target_path: Some("new.txt".into()),
                kind: PackChangeKind::FollowPack,
                reason: None,
                current: None,
                target: Some(new_hash),
            },
        ];
        let metadata = metadata_snapshot(&i.dir).unwrap();
        let preview = PackPreview {
            name: "Pack".into(),
            version: "2".into(),
            mc: "1".into(),
            loader: None,
            files: vec![],
            overrides: Arc::new(BTreeMap::new()),
            digest: String::new(),
            source: None,
            project_icon: None,
        };
        let p = PackUpdatePlan {
            instance: i,
            old,
            target,
            preview,
            stage: Arc::new(stage),
            changes,
            observed: BTreeMap::from([
                ("config/a".into(), Some(old_hash)),
                ("new.txt".into(), None),
            ]),
            metadata,
            preserved: vec![],
        };
        (tmp, store, p)
    }
    #[test]
    fn apply_failure_restores_files_components_sources_and_baseline() {
        let (_tmp, store, p) = plan();
        let i = p.instance.clone();
        let metadata = p.metadata.clone();
        FAILURE.with(|f| f.set(1));
        assert!(
            apply(
                &store,
                p,
                PackDecisions::default(),
                i.config.components.clone(),
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(i.game_dir().join("config/a")).unwrap(),
            b"old"
        );
        assert!(!i.game_dir().join("new.txt").exists());
        assert_eq!(metadata_snapshot(&i.dir).unwrap(), metadata);
        assert!(ensure_ready(&i.dir).is_ok());
    }
    #[test]
    fn rollback_failure_keeps_recovery_and_blocks_launch() {
        let (_tmp, store, p) = plan();
        let i = p.instance.clone();
        FAILURE.with(|f| f.set(2));
        let e = apply(
            &store,
            p,
            PackDecisions::default(),
            i.config.components.clone(),
            None,
        )
        .unwrap_err();
        FAILURE.with(|f| f.set(0));
        let PackError::Recovery { path, .. } = e else {
            panic!("expected recovery error")
        };
        assert!(path.join("files/config/a").is_file());
        assert!(path.join("metadata/modpack-state.json").is_file());
        assert!(ensure_ready(&i.dir).is_err());
    }
    #[test]
    fn backup_failure_leaves_original_intact() {
        let (_tmp, store, p) = plan();
        let i = p.instance.clone();
        std::fs::write(i.dir.join("modpack-recovery"), b"obstruction").unwrap();
        assert!(
            apply(
                &store,
                p,
                PackDecisions::default(),
                i.config.components.clone(),
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(i.game_dir().join("config/a")).unwrap(),
            b"old"
        );
        assert!(ensure_ready(&i.dir).is_ok());
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;
    #[test]
    fn apply_failure_removes_newly_created_files() {
        let (_tmp, store, p) = super::tests::plan();
        let i = p.instance.clone();
        FAILURE.with(|f| f.set(3));
        assert!(
            apply(
                &store,
                p,
                PackDecisions::default(),
                i.config.components.clone(),
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(i.game_dir().join("config/a")).unwrap(),
            b"old"
        );
        assert!(!i.game_dir().join("new.txt").exists());
    }
    #[test]
    fn commit_failure_restores_all_metadata_after_it_was_written() {
        let (_tmp, store, p) = super::tests::plan();
        let i = p.instance.clone();
        let before = p.metadata.clone();
        let mut components = i.config.components.clone();
        components[0].version = "2".into();
        FAILURE.with(|f| f.set(5));
        assert!(apply(&store, p, PackDecisions::default(), components, None).is_err());
        assert_eq!(metadata_snapshot(&i.dir).unwrap(), before);
        assert!(!i.game_dir().join("new.txt").exists());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn running_instance_is_rejected_without_mutation() {
        let (_tmp, store, p) = super::tests::plan();
        let i = p.instance.clone();
        let _game = crate::launch::running::FakeGame::start(&i.dir, &i.id);
        assert!(
            apply(
                &store,
                p,
                PackDecisions::default(),
                i.config.components.clone(),
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(i.game_dir().join("config/a")).unwrap(),
            b"old"
        );
    }
}

#[cfg(test)]
mod rollback_barrier_tests {
    use super::*;

    #[test]
    fn final_metadata_restore_failure_keeps_pending_barrier_and_backup() {
        let (_tmp, store, plan) = super::tests::plan();
        let instance = plan.instance.clone();
        FAILURE.with(|failure| failure.set(6));
        let error = apply(
            &store,
            plan,
            PackDecisions::default(),
            instance.config.components.clone(),
            None,
        )
        .unwrap_err();
        FAILURE.with(|failure| failure.set(0));
        let PackError::Recovery { path, error } = error else {
            panic!("expected recovery failure")
        };
        assert!(error.contains("injected final metadata restore failure"));
        assert!(path.join("metadata/modpack-state.json").is_file());
        assert_eq!(std::fs::read(path.join("files/config/a")).unwrap(), b"old");
        assert!(
            ensure_ready(&instance.dir).is_err(),
            "a valid ready state must not bypass failed metadata recovery"
        );
        assert_eq!(
            std::fs::read_to_string(instance.dir.join(PENDING_FILE)).unwrap(),
            path.to_string_lossy()
        );
    }

    #[test]
    fn successful_rollback_returns_original_error_and_cleans_recovery() {
        let (_tmp, store, plan) = super::tests::plan();
        let instance = plan.instance.clone();
        let original_metadata = plan.metadata.clone();
        FAILURE.with(|failure| failure.set(1));
        let error = apply(
            &store,
            plan,
            PackDecisions::default(),
            instance.config.components.clone(),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(error, PackError::Io(ref message) if message == "injected apply failure"),
            "original application error must survive successful rollback: {error:?}"
        );
        assert_eq!(metadata_snapshot(&instance.dir).unwrap(), original_metadata);
        assert!(ensure_ready(&instance.dir).is_ok());
        assert_eq!(
            std::fs::read_dir(instance.dir.join("modpack-recovery"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn successful_install_rollback_restores_prior_installing_marker() {
        let (_tmp, store, mut plan) = super::tests::plan();
        let instance = plan.instance.clone();
        plan.old.ready = false;
        save_state(&instance.dir, &plan.old).unwrap();
        std::fs::write(instance.dir.join(PENDING_FILE), b"installing").unwrap();
        plan.metadata = metadata_snapshot(&instance.dir).unwrap();
        FAILURE.with(|failure| failure.set(1));
        let error = apply(
            &store,
            plan,
            PackDecisions::default(),
            instance.config.components.clone(),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, PackError::Io(ref message) if message == "injected apply failure"));
        assert_eq!(
            std::fs::read(instance.dir.join(PENDING_FILE)).unwrap(),
            b"installing"
        );
        assert!(ensure_ready(&instance.dir).is_err());
        assert_eq!(
            std::fs::read_dir(instance.dir.join("modpack-recovery"))
                .unwrap()
                .count(),
            0
        );
    }
}
