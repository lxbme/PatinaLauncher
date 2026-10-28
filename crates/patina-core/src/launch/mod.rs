pub mod account;
pub mod args;

#[cfg(feature = "dev-offline")]
pub use account::dev_offline_account;
pub use account::{LaunchAccount, UserType, offline_uuid};
pub use args::{
    LAUNCHER_NAME, LAUNCHER_VERSION, LaunchCommand, LaunchSpec, build_command, substitute,
};
