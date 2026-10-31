//! Prompt-free Secret Service access. Construction does not connect to a bus.
//! Synchronous trait entry points belong on the caller's blocking pool. Each call
//! owns a cancellable async runtime; no blocking Secret Service calls are spawned.
use crate::accounts::credentials::{
    CredentialBackend, CredentialError, CredentialRecord, validate_reference,
};

pub(crate) struct Keyring;
impl Keyring {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::*;
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use oo7::{
        Key, Secret,
        dbus::api::{self, DBusSecret},
        zbus::{self, zvariant::OwnedObjectPath},
    };
    use std::{future::Future, sync::Arc, time::Duration};

    const DEADLINE: Duration = Duration::from_secs(5);
    const DESTINATION: &str = "org.freedesktop.secrets";
    const SERVICE_PATH: &str = "/org/freedesktop/secrets";

    pub(crate) fn run_bounded<T>(
        deadline: Duration,
        future: impl Future<Output = Result<T, CredentialError>>,
    ) -> Result<T, CredentialError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| CredentialError::Unavailable)?;
        // Runtime is local to this blocking-pool call. Dropping it also cancels
        // connection reader/subscription tasks; no abandoned remote-wait thread.
        runtime.block_on(async {
            tokio::time::timeout(deadline, future)
                .await
                .unwrap_or(Err(CredentialError::Unavailable))
        })
    }
    pub(crate) fn map_zbus(error: zbus::Error) -> CredentialError {
        match error {
            zbus::Error::InputOutput(ref error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput
                ) =>
            {
                CredentialError::Corrupt
            }
            zbus::Error::InputOutput(_)
            | zbus::Error::Connection(..)
            | zbus::Error::Handshake(_)
            | zbus::Error::Address(_) => CredentialError::Unavailable,
            zbus::Error::MethodError(name, _, _) => classify_name(name.as_str()),
            zbus::Error::FDO(error) => match *error {
                zbus::fdo::Error::ZBus(error) => map_zbus(error),
                error => {
                    use zbus::DBusError;
                    classify_name(error.name().as_str())
                }
            },
            _ => CredentialError::Corrupt,
        }
    }
    fn classify_name(name: &str) -> CredentialError {
        match name {
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.DBus.Error.Disconnected"
            | "org.freedesktop.DBus.Error.AccessDenied"
            | "org.freedesktop.DBus.Error.AuthFailed"
            | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired"
            | "org.freedesktop.DBus.Error.NoServer"
            | "org.freedesktop.DBus.Error.IOError"
            | "org.freedesktop.DBus.Error.NoMemory"
            | "org.freedesktop.DBus.Error.LimitsExceeded"
            | "org.freedesktop.DBus.Error.Spawn.ExecFailed"
            | "org.freedesktop.DBus.Error.Spawn.ForkFailed"
            | "org.freedesktop.DBus.Error.Spawn.ChildExited"
            | "org.freedesktop.DBus.Error.Spawn.ChildSignaled"
            | "org.freedesktop.DBus.Error.Spawn.Failed"
            | "org.freedesktop.Secret.Error.IsLocked"
            | "org.freedesktop.DBus.Error.NotSupported" => CredentialError::Unavailable,
            _ => CredentialError::Corrupt,
        }
    }
    fn map_oo7(error: oo7::dbus::Error) -> CredentialError {
        match error {
            oo7::dbus::Error::ZBus(e) => map_zbus(e),
            oo7::dbus::Error::Service(oo7::dbus::ServiceError::ZBus(e)) => map_zbus(e),
            oo7::dbus::Error::Service(oo7::dbus::ServiceError::IsLocked(_))
            | oo7::dbus::Error::IO(_) => CredentialError::Unavailable,
            _ => CredentialError::Corrupt,
        }
    }
    pub(crate) fn require_no_prompt(path: &str) -> Result<(), CredentialError> {
        if path == "/" {
            Ok(())
        } else {
            Err(CredentialError::Unavailable)
        }
    }
    pub(crate) fn select_item(
        unlocked: Vec<String>,
        locked: Vec<String>,
    ) -> Result<Option<String>, CredentialError> {
        if unlocked.len() + locked.len() > 1 {
            return Err(CredentialError::Ambiguous);
        }
        if !locked.is_empty() {
            return Err(CredentialError::Unavailable);
        }
        Ok(unlocked.into_iter().next())
    }

    async fn connect() -> Result<zbus::Connection, CredentialError> {
        connect_address(&zbus::Address::session().map_err(map_zbus)?).await
    }
    pub(crate) async fn connect_address(
        address: &zbus::Address,
    ) -> Result<zbus::Connection, CredentialError> {
        use zbus::address::{Transport, transport::UnixSocket};
        let Transport::Unix(unix) = address.transport() else {
            return Err(CredentialError::Unavailable);
        };
        let stream = match unix.path() {
            UnixSocket::File(path) => tokio::net::UnixStream::connect(path)
                .await
                .map_err(|_| CredentialError::Unavailable)?,
            UnixSocket::Abstract(name) => {
                use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
                use std::os::unix::ffi::OsStrExt;
                let socket = net::socket_with(
                    AddressFamily::UNIX,
                    SocketType::STREAM,
                    SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
                    None,
                )
                .map_err(|_| CredentialError::Unavailable)?;
                let addr = SocketAddrUnix::new_abstract_name(name.as_bytes())
                    .map_err(|_| CredentialError::Unavailable)?;
                match net::connect(&socket, &addr) {
                    Ok(()) => {}
                    Err(rustix::io::Errno::INPROGRESS) => {}
                    // AF_UNIX EAGAIN means backlog full, not a pending connection.
                    Err(_) => return Err(CredentialError::Unavailable),
                }
                let stream =
                    tokio::net::UnixStream::from_std(std::os::unix::net::UnixStream::from(socket))
                        .map_err(|_| CredentialError::Unavailable)?;
                stream
                    .writable()
                    .await
                    .map_err(|_| CredentialError::Unavailable)?;
                if stream
                    .take_error()
                    .map_err(|_| CredentialError::Unavailable)?
                    .is_some()
                {
                    return Err(CredentialError::Unavailable);
                }
                stream
            }
            _ => return Err(CredentialError::Unavailable),
        };
        zbus::connection::Builder::unix_stream(stream)
            .method_timeout(DEADLINE)
            .build()
            .await
            .map_err(map_zbus)
    }
    async fn proxy<'a>(
        conn: &'a zbus::Connection,
        path: &'a str,
        interface: &'a str,
    ) -> Result<zbus::Proxy<'a>, CredentialError> {
        zbus::proxy::Builder::new(conn)
            .destination(DESTINATION)
            .map_err(map_zbus)?
            .path(path)
            .map_err(map_zbus)?
            .interface(interface)
            .map_err(map_zbus)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(map_zbus)
    }
    async fn find(
        conn: &zbus::Connection,
        reference: &str,
    ) -> Result<Option<String>, CredentialError> {
        let service = proxy(conn, SERVICE_PATH, "org.freedesktop.Secret.Service").await?;
        let attrs = std::collections::HashMap::from([
            ("service", "patina-launcher"),
            ("username", reference),
        ]);
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = service
            .call("SearchItems", &(attrs,))
            .await
            .map_err(map_zbus)?;
        select_item(
            unlocked.into_iter().map(|p| p.to_string()).collect(),
            locked.into_iter().map(|p| p.to_string()).collect(),
        )
    }
    async fn session(
        conn: &zbus::Connection,
    ) -> Result<(Arc<api::Session>, Arc<Key>), CredentialError> {
        let service = api::Service::new(conn).await.map_err(map_oo7)?;
        let private = Key::generate_private_key().map_err(|_| CredentialError::Corrupt)?;
        let public = Key::generate_public_key(&private).map_err(|_| CredentialError::Corrupt)?;
        let (server, session) = service.open_session(Some(public)).await.map_err(map_oo7)?;
        let server = server.ok_or(CredentialError::Corrupt)?;
        if server.as_ref().is_empty()
            || server.as_ref().len() > 128
            || server.as_ref().iter().all(|b| *b == 0)
        {
            return Err(CredentialError::Corrupt);
        }
        let key = Key::generate_aes_key(&private, &server).map_err(|_| CredentialError::Corrupt)?;
        Ok((Arc::new(session), Arc::new(key)))
    }
    pub(crate) async fn read_connected(
        conn: &zbus::Connection,
        reference: &str,
    ) -> Result<Option<CredentialRecord>, CredentialError> {
        let Some(path) = find(conn, reference).await? else {
            return Ok(None);
        };
        let (session, key) = session(conn).await?;
        let item = api::Item::new(
            conn,
            OwnedObjectPath::try_from(path).map_err(|_| CredentialError::Corrupt)?,
        )
        .await
        .map_err(map_oo7)?;
        let secret = item.secret(&session).await.map_err(map_oo7)?;
        if secret.session().inner().path() != session.inner().path()
            || secret.parameters().len() != 16
            || secret.value().is_empty()
            || secret.value().len() > crate::accounts::credentials::MAX_RECORD_BYTES + 16
        {
            return Err(CredentialError::Corrupt);
        }
        let secret = secret.decrypt(Some(&key)).map_err(map_oo7)?;
        CredentialRecord::decode(secret.as_bytes()).map(Some)
    }
    pub(crate) async fn write_connected(
        conn: &zbus::Connection,
        reference: &str,
        bytes: &[u8],
    ) -> Result<(), CredentialError> {
        let existing = find(conn, reference).await?;
        let (session, key) = session(conn).await?;
        let secret =
            DBusSecret::new_encrypted(session, Secret::blob(bytes), &key).map_err(map_oo7)?;
        if let Some(path) = existing {
            let item = proxy(conn, &path, "org.freedesktop.Secret.Item").await?;
            item.call::<_, _, ()>("SetSecret", &(secret,))
                .await
                .map_err(map_zbus)?;
        } else {
            let service = proxy(conn, SERVICE_PATH, "org.freedesktop.Secret.Service").await?;
            let path: OwnedObjectPath = service
                .call("ReadAlias", &("default",))
                .await
                .map_err(map_zbus)?;
            if path.as_str() == "/" {
                return Err(CredentialError::Unavailable);
            }
            let collection =
                proxy(conn, path.as_str(), "org.freedesktop.Secret.Collection").await?;
            let locked: bool = collection.get_property("Locked").await.map_err(map_zbus)?;
            if locked {
                return Err(CredentialError::Unavailable);
            }
            let attributes = [("service", "patina-launcher"), ("username", reference)];
            let properties = api::Properties::for_item("Patina credential", &attributes);
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = collection
                .call("CreateItem", &(properties, secret, true))
                .await
                .map_err(map_zbus)?;
            require_no_prompt(prompt.as_str())?;
            if item.as_str() == "/" {
                return Err(CredentialError::Corrupt);
            }
        }
        Ok(())
    }
    pub(crate) async fn delete_connected(
        conn: &zbus::Connection,
        reference: &str,
    ) -> Result<(), CredentialError> {
        let Some(path) = find(conn, reference).await? else {
            return Ok(());
        };
        let item = proxy(conn, &path, "org.freedesktop.Secret.Item").await?;
        let prompt: OwnedObjectPath = item.call("Delete", &()).await.map_err(map_zbus)?;
        require_no_prompt(prompt.as_str())
    }
    impl CredentialBackend for Keyring {
        fn read(&self, reference: &str) -> Result<Option<CredentialRecord>, CredentialError> {
            validate_reference(reference)?;
            run_bounded(DEADLINE, async {
                read_connected(&connect().await?, reference).await
            })
        }
        fn write(&self, reference: &str, record: &CredentialRecord) -> Result<(), CredentialError> {
            validate_reference(reference)?;
            let bytes = record.encode()?;
            run_bounded(DEADLINE, async {
                write_connected(&connect().await?, reference, &bytes).await
            })
        }
        fn delete(&self, reference: &str) -> Result<(), CredentialError> {
            validate_reference(reference)?;
            run_bounded(DEADLINE, async {
                delete_connected(&connect().await?, reference).await
            })
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl CredentialBackend for Keyring {
    fn read(&self, _: &str) -> Result<Option<CredentialRecord>, CredentialError> {
        Err(CredentialError::Unavailable)
    }
    fn write(&self, _: &str, _: &CredentialRecord) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
    fn delete(&self, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}
