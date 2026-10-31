//! CLI presentation over the same durable account transactions as the GUI.

use std::sync::Arc;

use patina_core::accounts::error::AuthError;
use patina_core::accounts::login::{
    CancelToken, DeviceCodeLogin, LoginError, LoginEvent, StorageKind,
};
use patina_core::accounts::{AccountKind, AccountService, RemovalOutcome};
use tokio::sync::mpsc;

use crate::cli::AccountCmd;

/// Run an account command; the caller owns interactive login presentation.
/// Ordinary output uses the CLI's EPIPE-safe stdout writer.
pub async fn run_account_command(
    service: Arc<AccountService>,
    command: AccountCmd,
    events: impl FnMut(LoginEvent),
    cancel: CancelToken,
) -> Result<(), AuthError> {
    run_account_command_with_output(service, command, events, cancel, |line| {
        crate::outln!("{line}");
    })
    .await
}

/// Injectable output boundary. No transaction or credential logic lives in the CLI.
pub async fn run_account_command_with_output(
    service: Arc<AccountService>,
    command: AccountCmd,
    mut events: impl FnMut(LoginEvent),
    cancel: CancelToken,
    mut output: impl FnMut(&str),
) -> Result<(), AuthError> {
    match command {
        AccountCmd::Login => {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let login = service.start(tx, cancel);
            tokio::pin!(login);
            let mut terminal = None;
            // Poll both the service and its events: waiting for start first would
            // hide the code until login ended. Drain after completion as Done can
            // already be queued. Never turn a committed Done into cancellation.
            let mut finished = false;
            loop {
                tokio::select! {
                    () = &mut login, if !finished => finished = true,
                    event = rx.recv() => match event {
                        Some(event) => {
                            match &event {
                                LoginEvent::Done { .. } => terminal = Some(Ok(())),
                                LoginEvent::Failed(error) => terminal = Some(Err(login_error(error))),
                                _ => {}
                            }
                            events(event);
                        }
                        None => break,
                    }
                }
            }
            terminal.unwrap_or(Err(AuthError::StorageUnavailable))?;
        }
        AccountCmd::List => {
            service.reload().await?;
            let snapshot = service.snapshot().await?;
            if snapshot.accounts().is_empty() {
                output("(no accounts)");
            }
            for account in snapshot.accounts() {
                let mark = if snapshot.current().is_some_and(|a| a.id == account.id) {
                    '*'
                } else {
                    ' '
                };
                let kind = match account.kind {
                    AccountKind::Msa => "Msa",
                    AccountKind::Offline => "Offline",
                };
                output(&format!(
                    "{mark} {}  {kind}  {}",
                    account.id, account.username
                ));
            }
        }
        AccountCmd::AddOffline { name } => {
            let account = service.add_offline(&name).await?;
            output(&format!("Account {} ({})", account.id, account.username));
        }
        #[cfg(feature = "dev-offline")]
        AccountCmd::AddDevOffline { name } => {
            let account = service.add_dev_offline(&name).await?;
            output(&format!(
                "Development account {} ({})",
                account.id, account.username
            ));
        }
        AccountCmd::Use { id } => {
            service.set_current(&id).await?;
            output(&format!("Current account: {id}"));
        }
        AccountCmd::Remove { id } => match service.remove(&id).await? {
            RemovalOutcome::Removed => output(&format!("Removed account {id}")),
            RemovalOutcome::CleanupPending => output(&format!(
                "Removed account {id}; credential cleanup pending (will retry)"
            )),
        },
    }
    Ok(())
}

fn login_error(error: &LoginError) -> AuthError {
    match error {
        LoginError::Auth(error) => error.clone(),
        LoginError::Cancelled => AuthError::Cancelled,
        LoginError::Expired => AuthError::Expired,
        // Compatibility events must not expose arbitrary text in CLI diagnostics.
        LoginError::Unavailable | LoginError::Other(_) => AuthError::StorageUnavailable,
    }
}

/// Explicit allowlist, never a Debug dump. Only Code displays the short user code.
/// The CLI does not launch a browser or collect passwords: the user opens this URL.
pub fn login_event_message(event: &LoginEvent) -> Option<String> {
    match event {
        LoginEvent::Code(code) => Some(format!(
            "Open {} in your browser and enter code {}. Press Ctrl-C to cancel.",
            code.verification_uri, code.user_code
        )),
        LoginEvent::Waiting => Some("Waiting for Microsoft approval...".into()),
        LoginEvent::Storage(StorageKind::Keyring) => {
            Some("Credentials saved in the system keyring.".into())
        }
        LoginEvent::Storage(StorageKind::LocalFile) => {
            Some("Credentials saved in a private local file (system keyring unavailable).".into())
        }
        LoginEvent::Done { username, uuid } => {
            Some(format!("Signed in and saved account: {username} ({uuid})"))
        }
        // The runner returns the typed failure for the command's error output.
        LoginEvent::Failed(_) => None,
        _ => None,
    }
}

/// Capture the selected stable identity before any preparation/download warnings.
pub async fn resolve_stored_account(
    service: &AccountService,
    id: Option<&str>,
    cancel: CancelToken,
) -> Result<patina_core::accounts::LaunchGrant, AuthError> {
    use patina_core::accounts::AccountError;
    let id = match id {
        Some(id) => id.to_owned(),
        None => {
            service.reload().await?;
            service
                .snapshot()
                .await?
                .current()
                .ok_or_else(|| AuthError::Account(AccountError::NotFound("current".into())))?
                .id
                .clone()
        }
    };
    service.resolve_launch(&id, cancel).await
}

/// Retain the captured grant across preparation (including warnings), then use
/// the service's lock-held actual-spawn gate. The preparation callback also lets
/// tests exercise the real orchestration without downloads or starting a game.
pub async fn launch_stored_account<F>(
    service: Arc<AccountService>,
    grant: patina_core::accounts::LaunchGrant,
    prepare: impl FnOnce(patina_core::launch::LaunchAccount) -> F,
    progress: patina_core::progress::ProgressSink,
) -> Result<patina_core::launch::GameSession, patina_core::accounts::AuthenticatedLaunchError>
where
    F: std::future::Future<
            Output = Result<patina_core::launch::PendingLaunch, patina_core::launch::SessionError>,
        >,
{
    let pending = prepare(grant.account().clone())
        .await
        .map_err(patina_core::accounts::AuthenticatedLaunchError::Session)?;
    service.launch_pending(grant, pending, progress).await
}
