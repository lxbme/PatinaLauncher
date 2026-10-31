//! What the saves tab lists: the worlds of `saves`, newest play first, with
//! the name from `level.dat` (the folder's when it has none), the last play
//! and whether there is an `icon.png`. Worlds cannot be deleted here. The
//! tab itself is [`LocalTab`](super::LocalTab).

use std::io;
use std::path::PathBuf;

use chrono::{Local, TimeZone};
use patina_core::content::saves::list_saves;
use patina_core::instance::Instance;

use super::local_tab::{LocalItem, icon_stamp};

/// The instance's saves folder.
pub fn dir(instance: &Instance) -> PathBuf {
    instance.game_dir().join("saves")
}

/// Lists the worlds (blocking). Icons are not read.
pub(super) fn load(instance: &Instance) -> io::Result<Vec<LocalItem>> {
    Ok(list_saves(&dir(instance))?
        .into_iter()
        .map(|world| {
            let (icon_size, icon_modified) = if world.has_icon {
                icon_stamp(&world.path.join("icon.png"))
            } else {
                (0, None)
            };
            let title = world
                .level_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| world.dir_name.clone());
            LocalItem {
                key: world.dir_name,
                title,
                path: world.path,
                description: None,
                last_played: world.last_played,
                is_dir: true,
                has_icon: world.has_icon,
                icon_size,
                icon_modified,
            }
        })
        .collect())
}

/// `YYYY-MM-DD HH:mm` of `millis` (unix milliseconds) in the local time
/// zone; empty for a time out of range.
pub fn format_last_played(millis: i64) -> String {
    format_in(&Local, millis)
}

/// [`format_last_played`] in time zone `tz`.
fn format_in<Tz: TimeZone>(tz: &Tz, millis: i64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    tz.timestamp_millis_opt(millis)
        .single()
        .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M").to_string())
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Utc};

    use super::*;

    #[test]
    fn last_played_is_year_to_minute_in_the_zone() {
        // 2026-10-30 12:34:56.789 UTC.
        let millis = 1_793_363_696_789;
        assert_eq!(format_in(&Utc, millis), "2026-10-30 12:34");
        let tokyo = FixedOffset::east_opt(9 * 3600).unwrap();
        assert_eq!(format_in(&tokyo, millis), "2026-10-30 21:34");
        let la = FixedOffset::west_opt(7 * 3600).unwrap();
        assert_eq!(format_in(&la, millis), "2026-10-30 05:34");
        // Midnight pads, a time out of range is empty.
        assert_eq!(format_in(&Utc, 0), "1970-01-01 00:00");
        assert_eq!(format_in(&Utc, i64::MAX), "");
    }
}
