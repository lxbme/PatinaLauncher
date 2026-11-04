//! One dialog owner for pack inspection, installation and manifest updates.
mod model;
mod view;
pub use model::{PackView, open_local, open_online, open_preview, open_update, open_update_plan};
