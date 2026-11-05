//! Pure account-bound draft/lifecycle. The page owns tasks, image handles and notifications.
use patina_core::accounts::{
    Account, AccountKind, AccountSnapshot,
    appearance::{
        AppearanceChanges, AppearanceProfile, ApplyOutcome, CapeChange, CapeId, FieldOutcome,
        PreviewSide, SkinModel, SkinUpload,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeavePolicy {
    Allow,
    ConfirmDiscard,
    WaitForApply,
    ConfirmUncertain,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceState {
    #[default]
    Loading,
    Ready,
    Applying,
    Reconciling,
    Unavailable,
    Uncertain,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppearanceTicket {
    account_id: String,
    revision: u64,
}
#[derive(Debug, Default)]
pub struct AppearanceModel {
    accounts: Vec<Account>,
    selected: Option<Account>,
    selection_lost: bool,
    revision: u64,
    state: AppearanceState,
    baseline: Option<AppearanceProfile>,
    baseline_skin: Option<SkinUpload>,
    draft_skin: Option<SkinUpload>,
    filename: Option<String>,
    cape: Option<CapeId>,
    confirmed_cape: Option<CapeId>,
    pending_cape: bool,
    side: PreviewSide,
    outer: bool,
    error: Option<String>,
    outcome: Option<ApplyOutcome>,
}
impl AppearanceModel {
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }
    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_ref().map(|a| a.id.as_str())
    }
    pub fn selected_name(&self) -> Option<&str> {
        self.selected.as_ref().map(|a| a.username.as_str())
    }
    pub fn state(&self) -> AppearanceState {
        self.state
    }
    pub fn baseline(&self) -> Option<&AppearanceProfile> {
        self.baseline.as_ref()
    }
    pub fn skin(&self) -> Option<&SkinUpload> {
        self.draft_skin.as_ref().or(self.baseline_skin.as_ref())
    }
    pub fn draft_skin(&self) -> Option<&SkinUpload> {
        self.draft_skin.as_ref()
    }
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }
    pub fn selected_cape(&self) -> Option<&CapeId> {
        self.cape.as_ref()
    }
    pub fn side(&self) -> PreviewSide {
        self.side
    }
    pub fn outer(&self) -> bool {
        self.outer
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn outcome(&self) -> Option<&ApplyOutcome> {
        self.outcome.as_ref()
    }
    pub fn ticket(&self) -> Option<AppearanceTicket> {
        Some(AppearanceTicket {
            account_id: self.selected_id()?.to_owned(),
            revision: self.revision,
        })
    }
    pub fn is_current(&self, ticket: &AppearanceTicket) -> bool {
        self.ticket().as_ref() == Some(ticket)
    }
    fn advance(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("appearance revision overflow");
    }
    pub fn set_accounts(&mut self, snapshot: AccountSnapshot) {
        self.accounts = snapshot
            .accounts()
            .iter()
            .filter(|a| a.kind == AccountKind::Msa)
            .cloned()
            .collect();
        if let Some(selected) = &self.selected {
            let replacement = self.accounts.iter().find(|a| a.id == selected.id);
            if replacement
                .is_none_or(|a| a.uuid != selected.uuid || a.token_ref != selected.token_ref)
            {
                self.advance();
                self.selected = None;
                self.clear_draft();
                self.confirmed_cape = None;
                self.pending_cape = false;
                self.outcome = None;
                self.baseline = None;
                self.baseline_skin = None;
                self.state = AppearanceState::Unavailable;
                self.selection_lost = true;
                self.error = Some(rust_i18n::t!("appearance.identity_changed").to_string());
            }
        } else if !self.selection_lost {
            let selected = snapshot
                .current()
                .filter(|a| a.kind == AccountKind::Msa)
                .or_else(|| self.accounts.first())
                .cloned();
            if let Some(account) = selected {
                self.selected = Some(account);
                self.advance();
                self.state = AppearanceState::Loading;
                self.outer = true;
            }
        }
    }
    pub fn select_account(&mut self, id: &str) -> bool {
        if self.is_applying() || self.state == AppearanceState::Reconciling {
            return false;
        }
        let Some(account) = self.accounts.iter().find(|a| a.id == id).cloned() else {
            return false;
        };
        if self.selected_id() == Some(id) {
            return false;
        }
        self.selected = Some(account);
        self.selection_lost = false;
        self.advance();
        self.clear_draft();
        self.baseline = None;
        self.baseline_skin = None;
        self.error = None;
        self.outcome = None;
        self.side = PreviewSide::Front;
        self.outer = true;
        self.state = AppearanceState::Loading;
        true
    }
    pub fn reload(&mut self) -> bool {
        if self.is_applying() || self.selected.is_none() {
            return false;
        }
        self.advance();
        self.clear_draft();
        self.baseline = None;
        self.baseline_skin = None;
        self.error = None;
        self.outcome = None;
        self.state = AppearanceState::Loading;
        true
    }
    pub fn accept_profile(
        &mut self,
        ticket: &AppearanceTicket,
        result: Result<AppearanceProfile, String>,
    ) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        match result {
            Ok(profile) if Some(profile.account_id()) == self.selected_id() => {
                self.cape = profile.active_cape().map(|c| c.id().clone());
                self.confirmed_cape = self.cape.clone();
                self.baseline = Some(profile);
                self.state = AppearanceState::Ready;
                self.error = None;
            }
            Ok(_) => return false,
            Err(error) => {
                self.state = AppearanceState::Unavailable;
                self.error = Some(error);
            }
        }
        true
    }
    pub fn accept_skin_texture(&mut self, profile: &AppearanceProfile, skin: SkinUpload) -> bool {
        if self.baseline.as_ref() != Some(profile)
            || self
                .baseline
                .as_ref()
                .and_then(|p| p.skin())
                .is_none_or(|s| s.model() != skin.model())
        {
            return false;
        }
        self.baseline_skin = Some(skin);
        true
    }
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }
    pub fn is_editable(&self) -> bool {
        self.state == AppearanceState::Ready && self.baseline.is_some()
    }
    pub fn choose_skin(&mut self, skin: SkinUpload, filename: String) -> bool {
        if !self.is_editable() {
            return false;
        }
        self.advance();
        self.draft_skin = Some(skin);
        self.filename = Some(filename);
        self.error = None;
        true
    }
    pub fn set_model(&mut self, model: SkinModel) -> Result<(), String> {
        if !self.is_editable() {
            return Err(rust_i18n::t!("appearance.unavailable").to_string());
        }
        let Some(skin) = self.skin() else {
            return Err(rust_i18n::t!("appearance.texture_unavailable").to_string());
        };
        let skin = skin.with_model(model).map_err(|e| e.to_string())?;
        self.advance();
        self.draft_skin = if self.filename.is_none()
            && self
                .baseline
                .as_ref()
                .and_then(|p| p.skin())
                .is_some_and(|s| s.model() == model)
        {
            None
        } else {
            Some(skin)
        };
        self.error = None;
        Ok(())
    }
    pub fn set_cape(&mut self, cape: Option<CapeId>) -> bool {
        if !self.is_editable()
            || cape.as_ref().is_some_and(|id| {
                !self
                    .baseline
                    .as_ref()
                    .is_some_and(|p| p.capes().iter().any(|c| c.id() == id))
            })
        {
            return false;
        }
        self.cape = cape;
        self.advance();
        self.error = None;
        true
    }
    pub fn set_side(&mut self, side: PreviewSide) {
        self.side = side;
    }
    pub fn set_outer(&mut self, outer: bool) {
        self.outer = outer;
    }
    pub fn has_unapplied_changes(&self) -> bool {
        self.draft_skin.is_some() || (self.baseline.is_some() && self.confirmed_cape != self.cape)
    }
    pub fn is_applying(&self) -> bool {
        self.state == AppearanceState::Applying
    }
    pub fn leave_policy(&self) -> LeavePolicy {
        if self.is_applying() {
            LeavePolicy::WaitForApply
        } else if matches!(
            self.state,
            AppearanceState::Uncertain | AppearanceState::Reconciling
        ) {
            LeavePolicy::ConfirmUncertain
        } else if self.has_unapplied_changes() {
            LeavePolicy::ConfirmDiscard
        } else {
            LeavePolicy::Allow
        }
    }
    fn clear_draft(&mut self) {
        self.draft_skin = None;
        self.filename = None;
        self.cape = None;
    }
    pub fn discard(&mut self) {
        if self.is_applying() {
            return;
        }
        self.advance();
        self.clear_draft();
        self.cape = self.confirmed_cape.clone();
        self.error = None;
    }
    pub fn begin_apply(&mut self) -> Option<(AppearanceTicket, AppearanceChanges)> {
        if !self.is_editable() || !self.has_unapplied_changes() {
            return None;
        }
        let baseline = self.baseline.as_ref()?;
        let mut changes = AppearanceChanges::new(baseline);
        if let Some(skin) = &self.draft_skin {
            changes = changes.with_skin(skin.clone());
        }
        self.pending_cape = self.confirmed_cape != self.cape;
        if self.pending_cape {
            changes = changes.with_cape(
                self.cape
                    .clone()
                    .map_or(CapeChange::Disable, CapeChange::Select),
            );
        }
        self.advance();
        self.state = AppearanceState::Applying;
        self.error = None;
        Some((self.ticket()?, changes))
    }
    pub fn apply_wait_expired(&mut self) {
        if matches!(
            self.state,
            AppearanceState::Applying | AppearanceState::Reconciling
        ) {
            self.state = AppearanceState::Uncertain;
            self.error = Some(rust_i18n::t!("appearance.uncertain").to_string());
        }
    }
    pub fn finish_apply(
        &mut self,
        ticket: &AppearanceTicket,
        result: Result<ApplyOutcome, String>,
    ) -> bool {
        if !self.is_current(ticket) {
            return false;
        }
        match result {
            Err(error) => {
                if !matches!(
                    self.state,
                    AppearanceState::Uncertain | AppearanceState::Reconciling
                ) {
                    self.state = AppearanceState::Ready;
                } else {
                    self.state = AppearanceState::Uncertain;
                }
                self.error = Some(error);
            }
            Ok(outcome) => {
                if outcome.skin() == &FieldOutcome::Confirmed {
                    self.baseline_skin = self.draft_skin.take().or(self.baseline_skin.take());
                    self.filename = None;
                }
                if let Some(profile) = outcome.profile() {
                    self.confirmed_cape = profile.active_cape().map(|cape| cape.id().clone());
                    self.baseline = Some(profile.clone());
                    if outcome.cape() == &FieldOutcome::Confirmed || !self.pending_cape {
                        self.cape = self.confirmed_cape.clone();
                    }
                } else if outcome.cape() == &FieldOutcome::Confirmed {
                    self.confirmed_cape = self.cape.clone();
                }
                self.state = if outcome.is_uncertain() {
                    AppearanceState::Uncertain
                } else if outcome.refresh_error().is_some() {
                    AppearanceState::Unavailable
                } else {
                    AppearanceState::Ready
                };
                self.error = outcome
                    .refresh_error()
                    .map(ToString::to_string)
                    .or_else(|| {
                        [outcome.skin(), outcome.cape()]
                            .iter()
                            .find_map(|field| match field {
                                FieldOutcome::Failed(e) | FieldOutcome::Uncertain(e) => {
                                    Some(e.to_string())
                                }
                                _ => None,
                            })
                    });
                self.outcome = Some(outcome);
            }
        }
        true
    }
    pub fn begin_reconcile(&mut self) -> Option<(AppearanceTicket, ApplyOutcome)> {
        if !matches!(
            self.state,
            AppearanceState::Uncertain | AppearanceState::Unavailable
        ) {
            return None;
        }
        let outcome = self.outcome.clone()?;
        let ticket = self.ticket()?;
        self.state = AppearanceState::Reconciling;
        Some((ticket, outcome))
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod model_tests;
