//! Descriptor-relative private storage. Never chmod existing entries or follow links.
use crate::{
    accounts::credentials::{
        CredentialBackend, CredentialError, CredentialRecord, validate_reference,
    },
    paths::DataDirs,
};
use zeroize::Zeroizing;

pub(crate) struct FileCredentials {
    data: DataDirs,
}
impl FileCredentials {
    pub(crate) fn new(data: DataDirs) -> Self {
        Self { data }
    }
}
impl CredentialBackend for FileCredentials {
    fn read(&self, reference: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        validate_reference(reference)?;
        PrivateCredentialDir::open(&self.data)?
            .read(&format!("{reference}.json"))?
            .map(|b| CredentialRecord::decode(&Zeroizing::new(b)))
            .transpose()
    }
    fn write(&self, reference: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
        validate_reference(reference)?;
        PrivateCredentialDir::open(&self.data)?
            .write(&format!("{reference}.json"), &record.encode()?)
    }
    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        validate_reference(reference)?;
        PrivateCredentialDir::open(&self.data)?.delete(&format!("{reference}.json"))
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::accounts::credentials::MAX_RECORD_BYTES;
    use rustix::{
        fd::OwnedFd,
        fs::{self, AtFlags, FileType, Mode, OFlags},
        io::Errno,
    };
    use std::{
        fs::File,
        io::{Read, Write},
        path::Component,
    };

    pub(crate) struct PrivateCredentialDir {
        fd: OwnedFd,
        metadata: bool,
    }
    pub(crate) struct CredentialLock {
        fd: OwnedFd,
    }
    impl Drop for CredentialLock {
        fn drop(&mut self) {
            // Closing alone can retain the lock in a concurrent fork's copy of
            // the open file description, until that child reaches CLOEXEC.
            let _ = fs::flock(&self.fd, fs::FlockOperation::Unlock);
        }
    }
    #[cfg(test)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum WriteStage {
        Create,
        Write,
        FileSync,
        Rename,
        DirectorySync,
    }

    fn error(e: Errno) -> CredentialError {
        match e {
            Errno::LOOP | Errno::NOTDIR => CredentialError::UnsafePath,
            _ => CredentialError::Io,
        }
    }
    fn check_dir(fd: &OwnedFd, private: bool) -> Result<(), CredentialError> {
        let stat = fs::fstat(fd).map_err(error)?;
        let mode = stat.st_mode & 0o7777;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || if private {
                mode != 0o700
            } else {
                mode & 0o022 != 0
            }
        {
            return Err(CredentialError::UnsafePath);
        }
        Ok(())
    }
    fn check_file(fd: &OwnedFd) -> Result<(), CredentialError> {
        check_file_stat(&fs::fstat(fd).map_err(error)?)
    }
    fn check_file_stat(stat: &fs::Stat) -> Result<(), CredentialError> {
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_nlink != 1
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
        {
            return Err(CredentialError::UnsafePath);
        }
        Ok(())
    }
    #[test]
    fn guard_drop_unlocks_even_with_inherited_open_file_description() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateCredentialDir::open(&DataDirs::new(tmp.path())).unwrap();
        let guard = dir.try_lock().unwrap().unwrap();
        // A concurrent fork can inherit this description until CLOEXEC runs.
        let inherited = rustix::io::dup(&guard.fd).unwrap();
        drop(guard);
        assert!(dir.try_lock().unwrap().is_some());
        drop(inherited);
    }
    #[test]
    fn foreign_owner_metadata_is_rejected_without_chown() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = PrivateCredentialDir::open(&DataDirs::new(tmp.path())).unwrap();
        dir.write("owned.json", b"synthetic").unwrap();
        let fd = dir.existing("owned.json", OFlags::RDONLY).unwrap().unwrap();
        let mut stat = fs::fstat(&fd).unwrap();
        assert_eq!(check_file_stat(&stat), Ok(()));
        stat.st_uid = stat.st_uid.wrapping_add(1);
        assert_eq!(check_file_stat(&stat), Err(CredentialError::UnsafePath));
    }
    fn name(name: &str) -> Result<(), CredentialError> {
        if name.is_empty()
            || name.len() > 128
            || name == "."
            || name == ".."
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        {
            return Err(CredentialError::UnsafePath);
        }
        Ok(())
    }
    /// Nonsecret metadata shares the descriptor-relative replacement primitive,
    /// but accepts legacy owner-controlled 0644 files. New files are still 0600.
    pub(crate) fn read_account_metadata(
        data: &DataDirs,
    ) -> Result<Option<Vec<u8>>, CredentialError> {
        let dir = PrivateCredentialDir {
            fd: PrivateCredentialDir::open_root(data)?,
            metadata: true,
        };
        let bytes = dir.read("accounts.toml")?;
        if let Some(fd) = dir.existing("accounts.toml", OFlags::RDONLY)? {
            // Reconcile a previous postrename error before journal cleanup can
            // delete an old reference. Visible bytes alone are not durable proof.
            fs::fsync(&fd).map_err(error)?;
            fs::fsync(&dir.fd).map_err(error)?;
        }
        Ok(bytes)
    }
    pub(crate) fn write_account_metadata(
        data: &DataDirs,
        bytes: &[u8],
        #[cfg(test)] fail: Option<WriteStage>,
    ) -> Result<(), CredentialError> {
        let dir = PrivateCredentialDir {
            fd: PrivateCredentialDir::open_root(data)?,
            metadata: true,
        };
        dir.write_with_temp(
            "accounts.toml",
            bytes,
            &format!(".accounts-temp-{}", uuid::Uuid::new_v4()),
            #[cfg(test)]
            fail,
        )
    }
    // Shared path traversal only; cache storage has its own policy/operations.
    pub(crate) fn open_account_data_dir(data: &DataDirs) -> Result<OwnedFd, CredentialError> {
        PrivateCredentialDir::open_root(data)
    }
    impl PrivateCredentialDir {
        fn open_root(data: &DataDirs) -> Result<OwnedFd, CredentialError> {
            let path = data.root();
            let absolute = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map_err(|_| CredentialError::Io)?
                    .join(path)
            };
            let mut fd = fs::open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(error)?;
            let components: Vec<_> = absolute.components().collect();
            for (index, component) in components.iter().enumerate() {
                match component {
                    Component::RootDir | Component::CurDir => continue,
                    Component::Normal(part) => {
                        let flags =
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
                        let next = match fs::openat(&fd, *part, flags, Mode::empty()) {
                            Ok(next) => next,
                            Err(Errno::NOENT) => {
                                // Creation is allowed only under an owned nonwritable parent.
                                check_dir(&fd, false)?;
                                match fs::mkdirat(&fd, *part, Mode::from_raw_mode(0o700)) {
                                    Ok(()) | Err(Errno::EXIST) => {}
                                    Err(e) => return Err(error(e)),
                                }
                                fs::fsync(&fd).map_err(error)?;
                                fs::openat(&fd, *part, flags, Mode::empty()).map_err(error)?
                            }
                            Err(e) => return Err(error(e)),
                        };
                        let stat = fs::fstat(&next).map_err(error)?;
                        // Root-owned sticky ancestors (e.g. /tmp) cannot rename an owned child.
                        let sticky_root = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
                        if stat.st_mode & 0o022 != 0 && !sticky_root {
                            return Err(CredentialError::UnsafePath);
                        }
                        if stat.st_uid != 0 && stat.st_uid != rustix::process::geteuid().as_raw() {
                            return Err(CredentialError::UnsafePath);
                        }
                        if index + 1 == components.len() {
                            check_dir(&next, false)?;
                        }
                        fd = next;
                    }
                    _ => return Err(CredentialError::UnsafePath),
                }
            }
            check_dir(&fd, false)?;
            Ok(fd)
        }
        pub(crate) fn open(data: &DataDirs) -> Result<Self, CredentialError> {
            let fd = Self::open_root(data)?;
            match fs::mkdirat(&fd, "credentials", Mode::from_raw_mode(0o700)) {
                Ok(()) => fs::fsync(&fd).map_err(error)?,
                Err(Errno::EXIST) => {}
                Err(e) => return Err(error(e)),
            }
            let fd = fs::openat(
                &fd,
                "credentials",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(error)?;
            check_dir(&fd, true)?;
            Ok(Self {
                fd,
                metadata: false,
            })
        }
        fn existing(
            &self,
            filename: &str,
            flags: OFlags,
        ) -> Result<Option<OwnedFd>, CredentialError> {
            name(filename)?;
            check_dir(&self.fd, !self.metadata)?;
            match fs::openat(
                &self.fd,
                filename,
                flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    if self.metadata {
                        let stat = fs::fstat(&fd).map_err(error)?;
                        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
                            || stat.st_nlink != 1
                            || stat.st_uid != rustix::process::geteuid().as_raw()
                            || stat.st_mode & 0o7022 != 0
                        {
                            return Err(CredentialError::UnsafePath);
                        }
                    } else {
                        check_file(&fd)?;
                    }
                    Ok(Some(fd))
                }
                Err(Errno::NOENT) => Ok(None),
                Err(e) => Err(error(e)),
            }
        }
        pub(crate) fn read(&self, filename: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            let Some(fd) = self.existing(filename, OFlags::RDONLY)? else {
                return Ok(None);
            };
            let mut bytes = Zeroizing::new(Vec::new());
            File::from(fd)
                .take((MAX_RECORD_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| CredentialError::Io)?;
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(CredentialError::Corrupt);
            }
            Ok(Some(std::mem::take(&mut *bytes)))
        }
        /// Atomically replace and sync. An I/O error can occur after replacement;
        /// callers must not interpret failure as restoration of the old bytes.
        pub(crate) fn write(&self, filename: &str, bytes: &[u8]) -> Result<(), CredentialError> {
            self.write_with_temp(
                filename,
                bytes,
                &format!(".temp-{}", uuid::Uuid::new_v4()),
                #[cfg(test)]
                None,
            )
        }
        #[cfg(test)]
        pub(crate) fn write_failing(
            &self,
            filename: &str,
            bytes: &[u8],
            stage: WriteStage,
        ) -> Result<(), CredentialError> {
            self.write_with_temp(
                filename,
                bytes,
                &format!(".temp-{}", uuid::Uuid::new_v4()),
                Some(stage),
            )
        }
        pub(crate) fn write_with_temp(
            &self,
            filename: &str,
            bytes: &[u8],
            temp: &str,
            #[cfg(test)] fail: Option<WriteStage>,
        ) -> Result<(), CredentialError> {
            name(filename)?;
            name(temp)?;
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(CredentialError::Corrupt);
            }
            self.existing(filename, OFlags::RDONLY)?;
            #[cfg(test)]
            if fail == Some(WriteStage::Create) {
                return Err(CredentialError::Io);
            }
            let fd = fs::openat(
                &self.fd,
                temp,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(error)?;
            let result = (|| {
                check_file(&fd)?;
                let mut file = File::from(fd);
                #[cfg(test)]
                if fail == Some(WriteStage::Write) {
                    file.write_all(&bytes[..bytes.len() / 2])
                        .map_err(|_| CredentialError::Io)?;
                    return Err(CredentialError::Io);
                }
                file.write_all(bytes).map_err(|_| CredentialError::Io)?;
                #[cfg(test)]
                if fail == Some(WriteStage::FileSync) {
                    return Err(CredentialError::Io);
                }
                file.sync_all().map_err(|_| CredentialError::Io)?;
                self.existing(filename, OFlags::RDONLY)?;
                #[cfg(test)]
                if fail == Some(WriteStage::Rename) {
                    return Err(CredentialError::Io);
                }
                fs::renameat(&self.fd, temp, &self.fd, filename).map_err(error)?;
                // Replacement has happened. A directory-sync error is not rollback.
                #[cfg(test)]
                if fail == Some(WriteStage::DirectorySync) {
                    return Err(CredentialError::Io);
                }
                fs::fsync(&self.fd).map_err(error)
            })();
            if result.is_err() {
                let _ = fs::unlinkat(&self.fd, temp, AtFlags::empty());
            }
            result
        }
        pub(crate) fn delete(&self, filename: &str) -> Result<(), CredentialError> {
            if self.existing(filename, OFlags::RDONLY)?.is_some() {
                fs::unlinkat(&self.fd, filename, AtFlags::empty()).map_err(error)?;
                fs::fsync(&self.fd).map_err(error)?;
            }
            Ok(())
        }
        fn cleanup_temps(&self) -> Result<(), CredentialError> {
            // Only the operation-lock holder may clean previous writers' temps.
            for entry in fs::Dir::read_from(&self.fd).map_err(error)? {
                let entry = entry.map_err(error)?;
                let Some(filename) = entry.file_name().to_str().ok() else {
                    continue;
                };
                if let Some(reference) = filename.strip_prefix(".temp-")
                    && validate_reference(reference).is_ok()
                {
                    self.delete(filename)?;
                }
            }
            Ok(())
        }
        pub(crate) fn try_lock(&self) -> Result<Option<CredentialLock>, CredentialError> {
            check_dir(&self.fd, !self.metadata)?;
            let fd = match self.existing("operation.lock", OFlags::RDWR)? {
                Some(fd) => fd,
                None => match fs::openat(
                    &self.fd,
                    "operation.lock",
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                ) {
                    Ok(fd) => {
                        check_file(&fd)?;
                        fs::fsync(&self.fd).map_err(error)?;
                        fd
                    }
                    Err(Errno::EXIST) => self
                        .existing("operation.lock", OFlags::RDWR)?
                        .ok_or(CredentialError::Io)?,
                    Err(e) => return Err(error(e)),
                },
            };
            match fs::flock(&fd, fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    let guard = CredentialLock { fd };
                    self.cleanup_temps()?;
                    Ok(Some(guard))
                }
                Err(Errno::WOULDBLOCK) => Ok(None),
                Err(e) => Err(error(e)),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported::*;
#[cfg(not(target_os = "linux"))]
mod unsupported {
    use super::*;
    pub(crate) fn read_account_metadata(_: &DataDirs) -> Result<Option<Vec<u8>>, CredentialError> {
        Err(CredentialError::Unavailable)
    }
    pub(crate) fn write_account_metadata(_: &DataDirs, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
    pub(crate) struct PrivateCredentialDir;
    pub(crate) struct CredentialLock;
    impl PrivateCredentialDir {
        pub(crate) fn open(_: &DataDirs) -> Result<Self, CredentialError> {
            Err(CredentialError::Unavailable)
        }
        pub(crate) fn read(&self, _: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            Err(CredentialError::Unavailable)
        }
        pub(crate) fn write(&self, _: &str, _: &[u8]) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
        pub(crate) fn delete(&self, _: &str) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
        pub(crate) fn try_lock(&self) -> Result<Option<CredentialLock>, CredentialError> {
            Err(CredentialError::Unavailable)
        }
    }
}
