//! Current-attempt diagnosis, owned by instance activity, never persisted.

use super::InstanceActivity;
use gpui_kit::{Context, Task};
use patina_core::launch::diagnosis::Diagnosis;

pub(super) struct DiagnosisEntry {
    pub(super) token: u64,
    pub(super) result: Option<Diagnosis>,
    pub(super) task: Task<()>,
}

impl InstanceActivity {
    pub fn diagnosis(&self, id: &str) -> Option<&Diagnosis> {
        self.diagnoses.get(id)?.result.as_ref()
    }

    pub(crate) fn diagnosis_token(&self, id: &str) -> Option<u64> {
        self.diagnoses.get(id).map(|entry| entry.token)
    }

    pub(super) fn begin_diagnosis_attempt(&mut self, id: &str, token: u64) {
        self.diagnoses.insert(
            id.to_string(),
            DiagnosisEntry {
                token,
                result: None,
                task: Task::ready(()),
            },
        );
    }

    pub(super) fn clear_diagnosis(&mut self, id: &str) {
        self.diagnoses.remove(id);
    }

    pub(super) fn apply_diagnosis(
        &mut self,
        id: &str,
        token: u64,
        result: Option<Diagnosis>,
        cx: &mut Context<Self>,
    ) {
        if self.loaded && self.entry(id).is_none() {
            return;
        }
        let Some(entry) = self
            .diagnoses
            .get_mut(id)
            .filter(|entry| entry.token == token)
        else {
            return;
        };
        entry.result = result;
        cx.notify();
    }

    pub(super) fn diagnose_launch_error(
        &mut self,
        id: &str,
        error: crate::launch::OpError,
        cx: &mut Context<Self>,
    ) {
        let Some(token) = self.diagnosis_token(id) else {
            return;
        };
        let work = crate::core_bridge::run_blocking(cx, move || error.launch_diagnosis());
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = work.await.ok().flatten();
            this.update(cx, |this, cx| this.apply_diagnosis(&key, token, result, cx))
                .ok();
        });
        if let Some(entry) = self.diagnoses.get_mut(id) {
            entry.task = task;
        }
    }

    pub(super) fn diagnose_saved_log(
        &mut self,
        id: &str,
        token: u64,
        path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        use crate::core_bridge::run_blocking;
        use patina_core::launch::diagnosis::analyze_saved_log;
        let Some(path) = path else {
            return;
        };
        let Some(super::Entry::Healthy(instance)) = self.entry(id) else {
            return;
        };
        // SessionExit is the only source; no current.log or historical search.
        let log_dir = instance.dir.join("patina-logs");
        if path.parent() != Some(log_dir.as_path()) {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains("-crash") || !name.ends_with(".log") {
            return;
        }
        let work = run_blocking(cx, move || {
            if std::fs::symlink_metadata(&log_dir)
                .ok()?
                .file_type()
                .is_symlink()
            {
                return None;
            }
            analyze_saved_log(&path).ok().flatten()
        });
        let key = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = work.await.ok().flatten();
            this.update(cx, |this, cx| this.apply_diagnosis(&key, token, result, cx))
                .ok();
        });
        if let Some(entry) = self.diagnoses.get_mut(id).filter(|e| e.token == token) {
            entry.task = task;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext};
    use patina_core::launch::diagnosis::{EvidenceContext, analyze};

    fn result() -> Option<Diagnosis> {
        analyze(
            "java.io.IOException: No space left on device",
            EvidenceContext::TerminalError,
        )
    }

    #[gpui_kit::test]
    fn diagnosis_missing_saved_path_and_other_instance_never_use_current_log(
        cx: &mut TestAppContext,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let store = patina_core::instance::InstanceStore::new(tmp.path());
        let first = store.create("first", "1.21.1").unwrap();
        let second = store.create("second", "1.21.1").unwrap();
        let path = first.dir.join("patina-logs/current.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "java.io.IOException: No space left on device").unwrap();
        let activity = cx.new(|_| InstanceActivity::new());
        activity.update(cx, |a, cx| {
            a.loaded = true;
            a.list = std::rc::Rc::new(store.list().unwrap());
            a.begin_diagnosis_attempt(&first.id, 1);
            // These paths must return before any read is scheduled (there is
            // deliberately no Core runtime installed in this test).
            a.diagnose_saved_log(&first.id, 1, None, cx);
            a.diagnose_saved_log(&first.id, 1, Some(path), cx);
            a.diagnose_saved_log(
                &first.id,
                1,
                Some(second.dir.join("patina-logs/fixture-crash.log")),
                cx,
            );
            assert!(a.diagnosis(&first.id).is_none());
        });
    }

    #[gpui_kit::test]
    fn diagnosis_retry_discards_old_result(cx: &mut TestAppContext) {
        let activity = cx.new(|_| InstanceActivity::new());
        activity.update(cx, |a, cx| {
            a.begin_diagnosis_attempt("a", 1);
            a.begin_diagnosis_attempt("a", 2);
            a.apply_diagnosis("a", 1, result(), cx);
            assert!(a.diagnosis("a").is_none());
            a.apply_diagnosis("a", 2, result(), cx);
            assert!(a.diagnosis("a").is_some());
            a.begin_diagnosis_attempt("a", 3);
            assert!(a.diagnosis("a").is_none());
        });
    }

    #[gpui_kit::test]
    fn diagnosis_dismiss_and_delete_discard_result(cx: &mut TestAppContext) {
        let activity = cx.new(|_| InstanceActivity::new());
        activity.update(cx, |a, cx| {
            a.begin_diagnosis_attempt("a", 1);
            a.clear_diagnosis("a");
            a.apply_diagnosis("a", 1, result(), cx);
            assert!(a.diagnosis("a").is_none());
            a.begin_diagnosis_attempt("a", 2);
            a.loaded = true;
            a.apply_diagnosis("a", 2, result(), cx);
            assert!(a.diagnosis("a").is_none(), "deleted member");
        });
    }

    #[gpui_kit::test]
    fn diagnosis_does_not_cross_instances(cx: &mut TestAppContext) {
        let activity = cx.new(|_| InstanceActivity::new());
        activity.update(cx, |a, cx| {
            a.begin_diagnosis_attempt("a", 1);
            a.begin_diagnosis_attempt("b", 2);
            a.apply_diagnosis("b", 1, result(), cx);
            assert!(a.diagnosis("b").is_none());
            a.apply_diagnosis("a", 1, result(), cx);
            assert!(a.diagnosis("a").is_some());
            assert!(a.diagnosis("b").is_none());
        });
    }
}
