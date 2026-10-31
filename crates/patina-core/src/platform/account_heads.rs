//! Private, descriptor-relative optional avatar cache. No credential policy changes.
#[cfg(target_os = "linux")]
pub(crate) use linux::HeadDirectory;
#[cfg(target_os = "linux")]
mod linux {
    use crate::{
        accounts::{credentials::CredentialError, error::AuthError},
        paths::DataDirs,
    };
    use rustix::{
        fd::OwnedFd,
        fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags},
        io::Errno,
    };
    use std::{
        collections::HashSet,
        fs::File,
        io::{Read, Write},
    };

    pub(crate) struct HeadDirectory(OwnedFd);
    fn error(e: Errno) -> AuthError {
        match e {
            Errno::LOOP | Errno::NOTDIR => AuthError::UnsafeCredentialPath,
            _ => AuthError::StorageUnavailable,
        }
    }
    pub(crate) fn valid_name(name: &str) -> bool {
        let Some((id, hash)) = name.strip_suffix(".png").and_then(|s| s.split_once('-')) else {
            return false;
        };
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && !hash.is_empty()
            && hash.len() <= 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }
    fn valid_temp(name: &str) -> bool {
        name.strip_prefix(".head-")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id))
    }
    fn directory(fd: &OwnedFd, private: bool) -> Result<(), AuthError> {
        let stat = fs::fstat(fd).map_err(error)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || (if private {
                stat.st_mode & 0o7777 != 0o700
            } else {
                stat.st_mode & 0o7022 != 0
            })
        {
            return Err(AuthError::UnsafeCredentialPath);
        }
        Ok(())
    }
    fn file(fd: &OwnedFd) -> Result<(), AuthError> {
        let s = fs::fstat(fd).map_err(error)?;
        if FileType::from_raw_mode(s.st_mode) != FileType::RegularFile
            || s.st_nlink != 1
            || s.st_uid != rustix::process::geteuid().as_raw()
            || s.st_mode & 0o7777 != 0o600
        {
            return Err(AuthError::UnsafeCredentialPath);
        }
        Ok(())
    }
    impl HeadDirectory {
        pub(crate) fn open(data: &DataDirs) -> Result<Self, AuthError> {
            let mut fd =
                super::super::credentials::open_account_data_dir(data).map_err(|e| match e {
                    CredentialError::UnsafePath => AuthError::UnsafeCredentialPath,
                    _ => AuthError::StorageUnavailable,
                })?;
            for (name, private) in [("cache", false), ("account-heads", true)] {
                match fs::mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(error(e)),
                }
                fd = fs::openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(error)?;
                directory(&fd, private)?;
            }
            Ok(Self(fd))
        }
        fn existing(&self, name: &str) -> Result<Option<OwnedFd>, AuthError> {
            if !valid_name(name) && !valid_temp(name) {
                return Err(AuthError::UnsafeCredentialPath);
            }
            directory(&self.0, true)?;
            match fs::openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    file(&fd)?;
                    Ok(Some(fd))
                }
                Err(Errno::NOENT) => Ok(None),
                Err(e) => Err(error(e)),
            }
        }
        pub(crate) fn read(&self, name: &str) -> Result<Option<Vec<u8>>, AuthError> {
            if !valid_name(name) {
                return Err(AuthError::UnsafeCredentialPath);
            }
            let Some(fd) = self.existing(name)? else {
                return Ok(None);
            };
            let mut bytes = Vec::new();
            File::from(fd)
                .take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| AuthError::StorageUnavailable)?;
            if bytes.len() > 256 * 1024 {
                return Ok(None);
            }
            Ok(Some(bytes))
        }
        pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> Result<(), AuthError> {
            if !valid_name(name) {
                return Err(AuthError::UnsafeCredentialPath);
            }
            if bytes.len() > 256 * 1024 {
                return Err(AuthError::StorageUnavailable);
            }
            if self.existing(name)?.is_some() {
                return Ok(());
            }
            let temp = format!(".head-{}", uuid::Uuid::new_v4());
            let fd = fs::openat(
                &self.0,
                &temp,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(error)?;
            let result = (|| {
                file(&fd)?;
                let mut file = File::from(fd);
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|_| AuthError::StorageUnavailable)?;
                // Never replace even a regular user-created file in our namespace.
                fs::renameat_with(&self.0, &temp, &self.0, name, RenameFlags::NOREPLACE)
                    .map_err(error)?;
                fs::fsync(&self.0).map_err(error)
            })();
            if result.is_err() {
                let _ = fs::unlinkat(&self.0, &temp, AtFlags::empty());
            }
            result
        }
        /// Under the account operation lock; only our canonical cache names.
        pub(crate) fn retain(&self, active: &HashSet<String>) -> Result<(), AuthError> {
            for entry in fs::Dir::read_from(&self.0).map_err(error)? {
                let entry = entry.map_err(error)?;
                let Ok(name) = entry.file_name().to_str() else {
                    continue;
                };
                if (valid_temp(name) || (valid_name(name) && !active.contains(name)))
                    && self.existing(name)?.is_some()
                {
                    fs::unlinkat(&self.0, name, AtFlags::empty()).map_err(error)?;
                }
            }
            fs::fsync(&self.0).map_err(error)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct HeadDirectory;
#[cfg(not(target_os = "linux"))]
impl HeadDirectory {
    pub(crate) fn open(
        _: &crate::paths::DataDirs,
    ) -> Result<Self, crate::accounts::error::AuthError> {
        Err(crate::accounts::error::AuthError::StorageUnavailable)
    }
    pub(crate) fn read(
        &self,
        _: &str,
    ) -> Result<Option<Vec<u8>>, crate::accounts::error::AuthError> {
        Ok(None)
    }
    pub(crate) fn write(&self, _: &str, _: &[u8]) -> Result<(), crate::accounts::error::AuthError> {
        Err(crate::accounts::error::AuthError::StorageUnavailable)
    }
    pub(crate) fn retain(
        &self,
        _: &std::collections::HashSet<String>,
    ) -> Result<(), crate::accounts::error::AuthError> {
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::HeadDirectory;
    use crate::{accounts::error::AuthError, paths::DataDirs};
    use std::{
        collections::HashSet,
        os::unix::fs::{PermissionsExt, symlink},
    };
    const NAME: &str = "0123456789abcdef0123456789abcdef-abcdef.png";
    #[test]
    fn cache_paths_reject_symlinks_modes_hardlinks_and_unsafe_names() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let cache = HeadDirectory::open(&data).unwrap();
        for name in [
            "../outside",
            "/tmp/outside",
            "bad.png",
            "0123456789abcdef0123456789abcdef-../a.png",
        ] {
            assert!(matches!(
                cache.read(name),
                Err(AuthError::UnsafeCredentialPath)
            ));
        }
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"user file").unwrap();
        let path = data.cache().join("account-heads").join(NAME);
        symlink(&outside, &path).unwrap();
        assert!(matches!(
            cache.read(NAME),
            Err(AuthError::UnsafeCredentialPath)
        ));
        assert!(cache.write(NAME, b"replacement").is_err());
        assert!(cache.retain(&HashSet::new()).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"user file");
        std::fs::remove_file(&path).unwrap();
        cache.write(NAME, b"head").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            cache.read(NAME),
            Err(AuthError::UnsafeCredentialPath)
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&path, tmp.path().join("hardlink")).unwrap();
        assert!(matches!(
            cache.read(NAME),
            Err(AuthError::UnsafeCredentialPath)
        ));
    }
    #[test]
    fn orphaned_owned_temps_are_cleaned_without_deleting_unrelated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let cache = HeadDirectory::open(&data).unwrap();
        cache.write(NAME, b"sanitized head").unwrap();
        let dir = data.cache().join("account-heads");
        let orphan = dir.join(format!(".head-{}", uuid::Uuid::new_v4()));
        std::fs::rename(dir.join(NAME), &orphan).unwrap();
        let user = dir.join(".head-user-notes");
        std::fs::write(&user, b"notes").unwrap();
        cache.retain(&HashSet::new()).unwrap();
        assert!(!orphan.exists());
        assert_eq!(std::fs::read(user).unwrap(), b"notes");
    }
    #[test]
    fn replaced_cache_directory_is_not_followed_and_user_files_are_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(tmp.path());
        let cache = HeadDirectory::open(&data).unwrap();
        cache.write(NAME, b"original").unwrap();
        cache.write(NAME, b"other").unwrap();
        assert_eq!(cache.read(NAME).unwrap().unwrap(), b"original");
        let user = data.cache().join("account-heads/user-notes.txt");
        std::fs::write(&user, b"notes").unwrap();
        cache.retain(&HashSet::new()).unwrap();
        assert!(user.exists());
        let heads = data.cache().join("account-heads");
        std::fs::rename(&heads, tmp.path().join("old-heads")).unwrap();
        symlink(tmp.path().join("old-heads"), &heads).unwrap();
        assert!(matches!(
            HeadDirectory::open(&data),
            Err(AuthError::UnsafeCredentialPath)
        ));
        std::fs::remove_file(&heads).unwrap();
        std::fs::create_dir(&heads).unwrap();
        std::fs::set_permissions(&heads, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            HeadDirectory::open(&data),
            Err(AuthError::UnsafeCredentialPath)
        ));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod service_tests {
    use crate::{
        accounts::{
            AccountService,
            error::{AuthError, AuthStage},
            testing::{
                AuthClock, AuthHttp, MemoryCredentials, MsaClient, credential_store, seed_verified,
            },
        },
        paths::DataDirs,
    };
    use std::{io::Cursor, os::unix::fs::symlink, sync::Arc};
    use tokio::sync::watch;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    struct Clock;
    impl AuthClock for Clock {
        fn unix_seconds(&self) -> u64 {
            1_800_000_000
        }
    }
    #[tokio::test]
    async fn service_reader_rejects_file_and_directory_replacement_after_cache_head() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDirs::new(temp.path());
        let server = MockServer::start().await;
        let stages = [
            AuthStage::DeviceCode,
            AuthStage::MicrosoftToken,
            AuthStage::XboxLive,
            AuthStage::Xsts,
            AuthStage::MinecraftToken,
            AuthStage::Entitlement,
            AuthStage::Profile,
        ];
        let http = AuthHttp::for_test(std::array::from_fn(|i| {
            (stages[i], format!("{}/{i}", server.uri()))
        }))
        .unwrap();
        let clock: Arc<dyn AuthClock> = Arc::new(Clock);
        let mut service = AccountService::for_test(
            data.clone(),
            MsaClient::for_test(http, clock.clone()),
            credential_store(data.clone(), Arc::new(MemoryCredentials::default())),
            clock,
        );
        service.set_skin_endpoint_for_test(&server.uri()).unwrap();
        let account = seed_verified(
            &service,
            "Player",
            uuid::Uuid::from_u128(1),
            "SYNTHETIC_REFRESH",
            1_800_000_000,
            1_800_003_600,
        )
        .await
        .unwrap();
        let metadata_path = data.root().join("accounts.toml");
        let mut metadata: toml::Value =
            toml::from_str(&std::fs::read_to_string(&metadata_path).unwrap()).unwrap();
        metadata["accounts"][0]
            .as_table_mut()
            .unwrap()
            .insert("skin_hash".into(), toml::Value::String("abcdef".into()));
        std::fs::write(metadata_path, toml::to_string(&metadata).unwrap()).unwrap();
        service.reload().await.unwrap();
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 64))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Mock::given(path("/texture/abcdef"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.into_inner()))
            .mount(&server)
            .await;
        let (_tx, token) = watch::channel(false);
        let cached = service
            .cache_head(&account.id, token.clone())
            .await
            .unwrap()
            .unwrap();
        let outside = temp.path().join("user-file");
        std::fs::write(&outside, b"PRIVATE_USER_DATA").unwrap();
        std::fs::remove_file(&cached).unwrap();
        symlink(&outside, &cached).unwrap();
        assert!(matches!(
            service.read_cached_head(&account.id, token.clone()).await,
            Err(AuthError::UnsafeCredentialPath)
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"PRIVATE_USER_DATA");
        let heads = data.cache().join("account-heads");
        std::fs::rename(&heads, temp.path().join("previous-heads")).unwrap();
        symlink(temp.path().join("previous-heads"), &heads).unwrap();
        assert!(matches!(
            service.read_cached_head(&account.id, token).await,
            Err(AuthError::UnsafeCredentialPath)
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"PRIVATE_USER_DATA");
    }
}
