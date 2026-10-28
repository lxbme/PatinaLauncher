//! JVM related types. `GcPreset` is a placeholder; Task 3 adds more variants.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcPreset {
    #[default]
    None,
}
