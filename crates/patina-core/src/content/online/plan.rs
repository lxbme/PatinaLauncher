//! Concrete user choices and immutable, target-bound installation plans.
use super::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub(super) instance: String,
    pub(super) version: VersionRef,
    pub(super) target: TargetCompatibility,
}
impl InstallRequest {
    pub fn new(
        instance: impl Into<String>,
        version: VersionRef,
        target: TargetCompatibility,
    ) -> Self {
        Self {
            instance: instance.into(),
            version,
            target,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct InstallChoices {
    pub(super) optional: HashSet<ProjectRef>,
    pub(super) replace: HashSet<String>,
    pub(super) enable: HashSet<ProjectRef>,
    pub(super) manual: HashMap<VersionRef, PathBuf>,
}
impl InstallChoices {
    pub fn set_optional(&mut self, project: ProjectRef, value: bool) {
        if value {
            self.optional.insert(project);
        } else {
            self.optional.remove(&project);
        }
    }
    pub fn is_optional_selected(&self, project: &ProjectRef) -> bool {
        self.optional.contains(project)
    }
    pub fn set_replacement(&mut self, path: String, value: bool) {
        if value {
            self.replace.insert(path);
        } else {
            self.replace.remove(&path);
        }
    }
    pub fn is_replacement_allowed(&self, path: &str) -> bool {
        self.replace.contains(path)
    }
    pub fn set_enabled_dependency(&mut self, project: ProjectRef, value: bool) {
        if value {
            self.enable.insert(project);
        } else {
            self.enable.remove(&project);
        }
    }
    pub fn is_dependency_enabled(&self, project: &ProjectRef) -> bool {
        self.enable.contains(project)
    }
    pub fn manual_file(&self, version: &VersionRef) -> Option<&std::path::Path> {
        self.manual.get(version).map(PathBuf::as_path)
    }

    pub fn with_optional(mut self, project: ProjectRef) -> Self {
        self.optional.insert(project);
        self
    }
    pub fn with_replacement(mut self, path: impl Into<String>) -> Self {
        self.replace.insert(path.into());
        self
    }
    pub fn with_enabled_dependency(mut self, project: ProjectRef) -> Self {
        self.enable.insert(project);
        self
    }
    pub fn with_manual_file(mut self, version: VersionRef, path: PathBuf) -> Self {
        self.manual.insert(version, path);
        self
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictReason {
    Modified,
    ForeignFile,
    DisabledRequired,
}
#[derive(Debug, Clone)]
pub struct ContentConflict {
    pub(super) project: ProjectRef,
    pub(super) path: String,
    pub(super) reason: ConflictReason,
}
impl ContentConflict {
    pub fn project(&self) -> &ProjectRef {
        &self.project
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn reason(&self) -> &ConflictReason {
        &self.reason
    }
}
#[derive(Debug, Clone)]
pub struct PlannedContent {
    pub(super) version: ContentVersion,
    pub(super) title: String,
    pub(super) kind: ContentKind,
    pub(super) file: ContentFile,
    pub(super) destination: String,
    pub(super) old: Option<SourceRecord>,
    pub(super) reusable: bool,
    // Any satisfied path activates this item. Each path lists optional ancestors,
    // so a required dependency of an optional project is selected with its parent.
    pub(super) conditions: Vec<Vec<ProjectRef>>,
    pub(super) dependency: bool,
}
impl PlannedContent {
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn version(&self) -> &ContentVersion {
        &self.version
    }
    pub fn file(&self) -> &ContentFile {
        &self.file
    }
    pub fn destination(&self) -> &str {
        &self.destination
    }
    pub fn is_reused(&self) -> bool {
        self.reusable
    }
    pub fn is_optional(&self) -> bool {
        self.conditions.iter().all(|c| !c.is_empty())
    }
    pub fn is_selected(&self, choices: &InstallChoices) -> bool {
        self.conditions
            .iter()
            .any(|path| path.iter().all(|p| choices.optional.contains(p)))
    }
}
#[derive(Debug, Clone)]
pub(super) struct UnavailableDependency {
    pub(super) conditions: Vec<ProjectRef>,
    pub(super) project: String,
}
#[derive(Debug, Clone)]
pub(super) struct Incompatibility {
    pub(super) project: ProjectRef,
    pub(super) conditions: Vec<ProjectRef>,
    pub(super) installed: bool,
}
#[derive(Debug, Clone)]
pub struct ContentInstallPlan {
    pub(super) request: InstallRequest,
    pub(super) items: Vec<PlannedContent>,
    pub(super) sources: ContentSources,
    pub(super) observed: HashMap<String, Option<String>>,
    pub(super) conflicts: Vec<ContentConflict>,
    pub(super) incompatibilities: Vec<Incompatibility>,
    pub(super) unavailable: Vec<UnavailableDependency>,
    pub(super) loader: Option<crate::loader::LoaderKind>,
}
impl ContentInstallPlan {
    pub fn root(&self) -> &VersionRef {
        &self.request.version
    }
    pub fn instance_id(&self) -> &str {
        &self.request.instance
    }
    pub fn items(&self) -> &[PlannedContent] {
        &self.items
    }
    pub fn conflicts(&self) -> &[ContentConflict] {
        &self.conflicts
    }
    pub fn optional_projects(&self) -> Vec<ProjectRef> {
        let mut seen = HashSet::new();
        self.items
            .iter()
            .flat_map(|i| i.conditions.iter().flatten())
            .filter(|p| seen.insert((*p).clone()))
            .cloned()
            .collect()
    }
    pub fn restricted_files(&self) -> Vec<&PlannedContent> {
        self.items
            .iter()
            .filter(|item| !item.reusable && item.file.download_url().is_none())
            .collect()
    }
}
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub(super) applied: Vec<SourceRecord>,
}
impl InstallOutcome {
    pub fn applied(&self) -> &[SourceRecord] {
        &self.applied
    }
}
