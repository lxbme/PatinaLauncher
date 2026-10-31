//! The view model of a [`TaskProgress`]: a step description, a bar fraction
//! and an amount text.

use patina_core::progress::{Phase, TaskProgress, Unit};
use rust_i18n::t;

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

/// What the header's progress area shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressView {
    /// The localized step description.
    pub step: String,
    /// The bar's fill in `0.0..=1.0`; `None` when the total is unknown
    /// (an indeterminate bar).
    pub fraction: Option<f32>,
    /// `128/312 MB` or `2041/5215`; `None` when the total is unknown.
    pub amount: Option<String>,
}

/// The view of one progress tick, on its own. `done` above `total` is
/// clamped; `total == 0` is indeterminate. Use [`ProgressTracker`] to also
/// keep the bar from going back within a phase.
pub fn view_of(p: &TaskProgress) -> ProgressView {
    let step = p.phase.display_name();
    if p.total == 0 {
        return ProgressView {
            step,
            fraction: None,
            amount: None,
        };
    }
    let done = p.done.min(p.total);
    let fraction = (done as f64 / p.total as f64).clamp(0., 1.) as f32;
    let amount = match p.unit {
        Unit::Items => format!("{done}/{}", p.total),
        // Whole MB, or KB while the whole job is under 1 MB.
        Unit::Bytes if p.total >= MIB => {
            t!("ops.amount_mb", done = done / MIB, total = p.total / MIB).to_string()
        }
        Unit::Bytes => t!("ops.amount_kb", done = done / KIB, total = p.total / KIB).to_string(),
    };
    ProgressView {
        step,
        fraction: Some(fraction),
        amount: Some(amount),
    }
}

/// Folds the ticks of one operation into views whose bar only grows within a
/// phase (the core's `done` may arrive out of order) and restarts when the
/// phase changes.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    phase: Option<Phase>,
    done: u64,
    fraction: f32,
}

impl ProgressTracker {
    pub fn update(&mut self, p: &TaskProgress) -> ProgressView {
        if self.phase != Some(p.phase) {
            *self = Self {
                phase: Some(p.phase),
                ..Self::default()
            };
        }
        let mut p = *p;
        p.done = p.done.max(self.done);
        self.done = p.done;
        let mut view = view_of(&p);
        if let Some(fraction) = &mut view.fraction {
            *fraction = fraction.max(self.fraction);
            self.fraction = *fraction;
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(phase: Phase, done: u64, total: u64, unit: Unit) -> TaskProgress {
        TaskProgress {
            phase,
            done,
            total,
            unit,
        }
    }

    #[test]
    fn progress_view_model_is_sane() {
        let _locale = crate::i18n::locale_lock();
        crate::i18n::set_language(crate::i18n::Language::En);
        // 0/0: indeterminate, nothing to show as an amount.
        let v = view_of(&tick(Phase::FetchMetadata, 0, 0, Unit::Items));
        assert_eq!((v.fraction, v.amount), (None, None));
        // done > total: clamped.
        let v = view_of(&tick(Phase::DownloadLibraries, 9, 4, Unit::Items));
        assert_eq!(v.fraction, Some(1.0));
        assert_eq!(v.amount.as_deref(), Some("4/4"));
        // Bytes: whole MB, KB below 1 MB.
        let v = view_of(&tick(
            Phase::DownloadGameFiles,
            128 * MIB + 7,
            312 * MIB,
            Unit::Bytes,
        ));
        assert_eq!(v.amount.as_deref(), Some("128/312 MB"));
        assert!((v.fraction.unwrap() - 128. / 312.).abs() < 0.001);
        let v = view_of(&tick(
            Phase::DownloadInstaller,
            100 * KIB,
            300 * KIB,
            Unit::Bytes,
        ));
        assert_eq!(v.amount.as_deref(), Some("100/300 KB"));
        assert_eq!(
            view_of(&tick(Phase::DownloadAssetIndex, 2041, 5215, Unit::Items))
                .amount
                .as_deref(),
            Some("2041/5215")
        );
        // done going backwards: the bar and the amount hold their maximum.
        let mut t = ProgressTracker::default();
        let p = |done| tick(Phase::DownloadLibraries, done, 100, Unit::Items);
        assert_eq!(t.update(&p(60)).fraction, Some(0.6));
        let back = t.update(&p(40));
        assert_eq!(back.fraction, Some(0.6));
        assert_eq!(back.amount.as_deref(), Some("60/100"));
        // A new phase starts over.
        let next = t.update(&tick(Phase::RunProcessor, 1, 10, Unit::Items));
        assert_eq!(next.fraction, Some(0.1));
    }
}
