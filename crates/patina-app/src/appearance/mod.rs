//! Selected-account Java appearance page. No credential, grant or URL enters this view.
mod leave_dialog;
mod model;
mod view;
pub(crate) use leave_dialog::confirm_leave;
pub use model::{AppearanceModel, AppearanceState, AppearanceTicket, LeavePolicy};

use crate::core_bridge::{AccountsChanged, Core, run, run_blocking};
use gpui_kit::component::{
    ActiveTheme, Theme,
    select::{SelectEvent, SelectItem, SelectState},
};
use gpui_kit::{
    App, AppContext, Context, Entity, RenderImage, ScrollHandle, SharedString, Subscription, Task,
    Window,
};
use patina_core::accounts::appearance::{
    AppearanceError, CapeId, CapeTexture, PreviewInput, SkinModel, SkinUpload, compose_preview,
};
use rust_i18n::t;
use std::{collections::HashMap, io::Read, rc::Rc, sync::Arc, time::Duration};

#[derive(Clone)]
struct AccountChoice {
    id: String,
    name: SharedString,
}
impl SelectItem for AccountChoice {
    type Value = String;
    fn title(&self) -> SharedString {
        self.name.clone()
    }
    fn value(&self) -> &String {
        &self.id
    }
}
pub struct AppearancePage {
    model: AppearanceModel,
    selector: Entity<SelectState<Vec<AccountChoice>>>,
    shown: bool,
    generation: u64,
    preview_revision: u64,
    factor: u32,
    preview_jobs: u64,
    #[cfg(any(test, feature = "test-support"))]
    render_count: u64,
    image: Option<Arc<RenderImage>>,
    image_size: (u32, u32),
    cape: Option<(CapeId, CapeTexture)>,
    thumbs: HashMap<CapeId, Arc<RenderImage>>,
    loading_cape: Option<CapeId>,
    skin_error: Option<String>,
    cape_error: Option<String>,
    reauth_required: bool,
    thumb_task: Task<()>,
    accounts_task: Task<()>,
    profile_task: Task<()>,
    skin_task: Task<()>,
    cape_task: Task<()>,
    preview_task: Task<()>,
    apply_task: Task<()>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}
impl AppearancePage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selector = cx.new(|cx| SelectState::new(Vec::<AccountChoice>::new(), None, window, cx));
        let mut this = Self {
            model: AppearanceModel::default(),
            selector,
            shown: false,
            generation: 0,
            preview_revision: 0,
            factor: 0,
            preview_jobs: 0,
            #[cfg(any(test, feature = "test-support"))]
            render_count: 0,
            image: None,
            image_size: (0, 0),
            cape: None,
            thumbs: HashMap::new(),
            loading_cape: None,
            skin_error: None,
            cape_error: None,
            reauth_required: false,
            thumb_task: Task::ready(()),
            accounts_task: Task::ready(()),
            profile_task: Task::ready(()),
            skin_task: Task::ready(()),
            cape_task: Task::ready(()),
            preview_task: Task::ready(()),
            apply_task: Task::ready(()),
            scroll: ScrollHandle::new(),
            _subscriptions: vec![],
        };
        this._subscriptions = vec![
            cx.observe_in(
                &AccountsChanged::epoch(cx),
                window,
                |this, _, window, cx| {
                    if this.shown {
                        this.refresh_accounts(false, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &this.selector,
                window,
                |this, _, event: &SelectEvent<_>, window, cx| {
                    let SelectEvent::Confirm(Some(id)) = event else {
                        return;
                    };
                    let id = id.clone();
                    this.sync_selector(window, cx);
                    this.request_account(&id, window, cx);
                },
            ),
            cx.observe_global_in::<Theme>(window, |this, window, cx| {
                this.update_scale(window, cx);
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.update_scale(window, cx);
            }),
        ];
        cx.on_release(|this, cx| this.retire_images(cx)).detach();
        this
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn renders_for_test(&self) -> u64 {
        self.render_count
    }
    pub fn model(&self) -> &AppearanceModel {
        &self.model
    }
    pub fn preview_jobs_started(&self) -> u64 {
        self.preview_jobs
    }
    pub fn preview_raster_size(&self) -> (u32, u32) {
        self.image_size
    }
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shown {
            return;
        }
        self.shown = true;
        self.update_scale(window, cx);
        self.refresh_accounts(true, window, cx);
    }
    pub fn hidden(&mut self, cx: &mut Context<Self>) {
        self.shown = false;
        self.thumb_task = Task::ready(());
        self.generation += 1;
        self.model.discard();
        self.model.reload();
        self.profile_task = Task::ready(());
        self.skin_task = Task::ready(());
        self.cape_task = Task::ready(());
        self.preview_task = Task::ready(());
        self.apply_task = Task::ready(());
        self.cape = None;
        self.loading_cape = None;
        self.retire_images(cx);
    }
    fn refresh_accounts(&mut self, reload: bool, window: &mut Window, cx: &mut Context<Self>) {
        let read = Core::read_accounts(cx);
        self.accounts_task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(snapshot) => {
                        let before = this.model.ticket();
                        this.model.set_accounts(snapshot);
                        if reload {
                            this.model.reload();
                        }
                        this.sync_selector(window, cx);
                        if before != this.model.ticket() {
                            this.clear_assets(cx);
                        }
                        if this.shown
                            && this.model.selected_id().is_some()
                            && this.model.state() == AppearanceState::Loading
                        {
                            this.load_profile(window, cx);
                        }
                    }
                    Err(error) => this.model.set_error(error),
                }
                cx.notify();
            })
            .ok();
        });
    }
    fn sync_selector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self
            .model
            .accounts()
            .iter()
            .map(|a| AccountChoice {
                id: a.id.clone(),
                name: a.username.clone().into(),
            })
            .collect();
        let selected = self.model.selected_id().map(str::to_owned);
        self.selector.update(cx, |select, cx| {
            select.set_items(items, window, cx);
            if let Some(id) = selected {
                select.set_selected_value(&id, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }
    fn request_account(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.selected_id() == Some(id) {
            return;
        }
        match self.model.leave_policy() {
            LeavePolicy::Allow => self.select_account(id, window, cx),
            policy => {
                let weak = cx.weak_entity();
                let id = id.to_owned();
                let confirm = Rc::new(move |window: &mut Window, cx: &mut App| {
                    weak.update(cx, |this, cx| {
                        this.model.discard();
                        this.select_account(&id, window, cx);
                    })
                    .ok();
                });
                confirm_leave(
                    policy,
                    self.model.selected_name().unwrap_or_default(),
                    confirm,
                    Rc::new(|_, _| {}),
                    window,
                    cx,
                );
            }
        }
    }
    fn select_account(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.select_account(id) {
            self.clear_assets(cx);
            self.sync_selector(window, cx);
            self.load_profile(window, cx);
            cx.notify();
        }
    }
    fn clear_assets(&mut self, cx: &mut App) {
        self.generation += 1;
        self.preview_revision += 1;
        self.cape = None;
        self.loading_cape = None;
        self.skin_error = None;
        self.cape_error = None;
        self.reauth_required = false;
        self.thumb_task = Task::ready(());
        self.skin_task = Task::ready(());
        self.cape_task = Task::ready(());
        self.preview_task = Task::ready(());
        self.retire_images(cx);
    }
    fn retire_images(&mut self, cx: &mut App) {
        if let Some(image) = self.image.take() {
            cx.defer(move |cx| cx.drop_image(image, None));
        }
        for (_, image) in self.thumbs.drain() {
            cx.defer(move |cx| cx.drop_image(image, None));
        }
        self.image_size = (0, 0);
    }
    fn load_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ticket) = self.model.ticket() else {
            return;
        };
        let id = self.model.selected_id().expect("ticket target").to_owned();
        let service = Core::global(cx).accounts();
        let read = run(cx, async move {
            let (_tx, cancel) = tokio::sync::watch::channel(false);
            service.appearance(&id, cancel).await
        });
        self.profile_task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                if !this.model.is_current(&ticket) {
                    return;
                }
                let result = match result {
                    Ok(result) => {
                        this.reauth_required = result.as_ref().err().is_some_and(needs_sign_in);
                        result.map_err(|e| e.to_string())
                    }
                    Err(error) => Err(error.to_string()),
                };
                if this.model.accept_profile(&ticket, result) {
                    this.load_thumbnails(cx);
                    this.load_skin(window, cx);
                    this.load_cape(window, cx);
                    cx.notify();
                }
            })
            .ok();
        });
    }
    fn load_skin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skin) = self.model.baseline().and_then(|p| p.skin()).cloned() else {
            return;
        };
        let Some(profile) = self.model.baseline().cloned() else {
            return;
        };
        let generation = self.generation;
        let id = self.model.selected_id().unwrap().to_owned();
        let service = Core::global(cx).accounts();
        let read = run(cx, async move {
            let (_tx, cancel) = tokio::sync::watch::channel(false);
            let bytes = service
                .appearance_texture(&id, skin.texture(), cancel)
                .await
                .map_err(|e| e.to_string())?;
            tokio::task::spawn_blocking(move || {
                SkinUpload::from_png(bytes, skin.model()).map_err(|e| e.to_string())
            })
            .await
            .map_err(|_| t!("appearance.texture_unavailable").to_string())?
        });
        self.skin_task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(skin) => {
                        this.model.accept_skin_texture(&profile, skin);
                        this.skin_error = None;
                    }
                    Err(error) if this.model.draft_skin().is_none() => {
                        this.skin_error = Some(error)
                    }
                    Err(_) => {}
                }
                this.rebuild_preview(window, cx);
                cx.notify();
            })
            .ok();
        });
    }
    fn load_cape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.model.selected_cape().cloned() else {
            self.cape = None;
            self.loading_cape = None;
            self.cape_task = Task::ready(());
            self.rebuild_preview(window, cx);
            return;
        };
        if self
            .cape
            .as_ref()
            .is_some_and(|(current, _)| current == &id)
            || self.loading_cape.as_ref() == Some(&id)
        {
            return;
        }
        let Some(cape) = self
            .model
            .baseline()
            .and_then(|p| p.capes().iter().find(|c| c.id() == &id))
            .cloned()
        else {
            return;
        };
        self.cape = None;
        self.loading_cape = Some(id.clone());
        self.cape_error = None;
        let generation = self.generation;
        let account = self.model.selected_id().unwrap().to_owned();
        let service = Core::global(cx).accounts();
        let read = run(cx, async move {
            let (_tx, cancel) = tokio::sync::watch::channel(false);
            let bytes = service
                .appearance_texture(&account, cape.texture(), cancel)
                .await
                .map_err(|e| e.to_string())?;
            CapeTexture::from_png(&bytes).map_err(|e| e.to_string())
        });
        self.cape_task = cx.spawn_in(window, async move |this, cx| {
            let result = read.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation || this.model.selected_cape() != Some(&id) {
                    return;
                }
                this.loading_cape = None;
                match result {
                    Ok(cape) => {
                        let mut raster = image::imageops::resize(
                            &cape.thumbnail(),
                            20,
                            32,
                            image::imageops::FilterType::Nearest,
                        );
                        for pixel in raster.pixels_mut() {
                            pixel.0.swap(0, 2);
                        }
                        let image = Arc::new(RenderImage::new(vec![image::Frame::new(raster)]));
                        if let Some(old) = this.thumbs.insert(id.clone(), image) {
                            cx.defer(move |cx| cx.drop_image(old, None));
                        }
                        this.cape = Some((id, cape));
                    }
                    Err(error) => this.cape_error = Some(error),
                }
                this.rebuild_preview(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.rebuild_preview(window, cx);
    }
    fn update_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Physical raster boundary: integer physical pixels, capped at384×640.
        let factor = (f32::from(cx.theme().font_size) * 0.5 * window.scale_factor())
            .floor()
            .clamp(1., 16.) as u32;
        if self.factor != factor {
            self.factor = factor;
            if self.shown {
                self.rebuild_preview(window, cx);
            }
        }
    }
    fn rebuild_preview(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.preview_revision += 1;
        let revision = self.preview_revision;
        let Some(skin) = self.model.skin() else {
            return;
        };
        let mut input = PreviewInput::new(skin)
            .with_side(self.model.side())
            .with_outer(self.model.outer());
        if self.model.side() == patina_core::accounts::appearance::PreviewSide::Back
            && self.model.selected_cape().is_some()
        {
            let Some((id, cape)) = &self.cape else {
                if let Some(image) = self.image.take() {
                    cx.defer(move |cx| cx.drop_image(image, None));
                }
                return;
            };
            if self.model.selected_cape() != Some(id) {
                return;
            }
            input = input.with_cape(cape.clone());
        }
        let factor = self.factor.max(1);
        self.preview_jobs += 1;
        let work = cx.background_spawn(async move {
            let logical = compose_preview(&input).ok()?;
            let mut raster = image::imageops::resize(
                &logical,
                24 * factor,
                40 * factor,
                image::imageops::FilterType::Nearest,
            );
            for pixel in raster.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            Some(raster)
        });
        self.preview_task = cx.spawn(async move |this, cx| {
            let raster = work.await;
            this.update(cx, |this, cx| {
                if revision != this.preview_revision || !this.shown {
                    return;
                }
                if let Some(raster) = raster {
                    this.image_size = raster.dimensions();
                    let image = Arc::new(RenderImage::new(vec![image::Frame::new(raster)]));
                    if let Some(old) = this.image.replace(image) {
                        cx.defer(move |cx| cx.drop_image(old, None));
                    }
                    cx.notify();
                }
            })
            .ok();
        });
    }
    fn choose_skin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.is_editable() {
            return;
        }
        let Some(ticket) = self.model.ticket() else {
            return;
        };
        let chosen = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.load_skin_file(ticket, path, window, cx);
            })
            .ok();
        })
        .detach();
    }
    pub(crate) fn load_skin_file(
        &mut self,
        ticket: AppearanceTicket,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.is_current(&ticket) || !self.model.is_editable() {
            return;
        }
        let read = run_blocking(cx, move || {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let file =
                std::fs::File::open(path).map_err(|_| t!("appearance.file_failed").to_string())?;
            let mut bytes = Vec::new();
            file.take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| t!("appearance.file_failed").to_string())?;
            SkinUpload::from_png(bytes, SkinModel::Classic)
                .map(|skin| (skin, filename))
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = read.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update_in(cx, |this, window, cx| {
                if !this.model.is_current(&ticket) {
                    return;
                }
                match result {
                    Ok((skin, name)) => {
                        this.model.choose_skin(skin, name);
                        this.skin_error = None;
                        this.rebuild_preview(window, cx);
                    }
                    Err(error) => this.model.set_error(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ticket, changes)) = self.model.begin_apply() else {
            return;
        };
        let id = self.model.selected_id().unwrap().to_owned();
        let service = Core::global(cx).accounts();
        let work = run(cx, async move {
            let (_tx, cancel) = tokio::sync::watch::channel(false);
            service.apply_appearance(&id, changes, cancel).await
        });
        self.wait_apply(ticket, work, window, cx);
        cx.notify();
    }
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ticket, outcome)) = self.model.begin_reconcile() else {
            return;
        };
        let service = Core::global(cx).accounts();
        let work = run(cx, async move {
            let (_tx, cancel) = tokio::sync::watch::channel(false);
            service.reconcile_appearance(&outcome, cancel).await
        });
        self.wait_apply(ticket, work, window, cx);
        cx.notify();
    }
    fn wait_apply(
        &mut self,
        ticket: AppearanceTicket,
        work: Task<
            Result<
                Result<patina_core::accounts::appearance::ApplyOutcome, AppearanceError>,
                crate::core_bridge::BridgeError,
            >,
        >,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_task = cx.spawn_in(window, async move |this, cx| {
            let deadline = cx.background_executor().timer(Duration::from_secs(90));
            let result = match futures::future::select(work, deadline).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right((_, work)) => {
                    this.update_in(cx, |this, _, cx| {
                        if this.model.is_current(&ticket) {
                            this.model.apply_wait_expired();
                            cx.notify();
                        }
                    })
                    .ok();
                    work.await
                }
            };
            this.update_in(cx, |this, window, cx| {
                if !this.model.is_current(&ticket) { return; }
                let result = match result {
                    Ok(result) => {
                        this.reauth_required = result.as_ref().err().is_some_and(needs_sign_in)
                            || result.as_ref().ok().is_some_and(|outcome| {
                                use patina_core::accounts::appearance::FieldOutcome;
                                [outcome.skin(),outcome.cape()].iter().any(|field|matches!(field,FieldOutcome::Failed(error)|FieldOutcome::Uncertain(error) if needs_sign_in(error)))
                            });
                        result.map_err(|e| e.to_string())
                    }
                    Err(error) => {
                        this.model.apply_wait_expired();
                        Err(error.to_string())
                    }
                };
                if this.model.finish_apply(&ticket, result) {
                    this.rebuild_preview(window, cx);
                    cx.notify();
                }
            })
            .ok();
        });
    }
}

fn needs_sign_in(error: &AppearanceError) -> bool {
    use patina_core::accounts::error::AuthError;
    matches!(
        error,
        AppearanceError::Auth(
            AuthError::ReauthenticationRequired
                | AuthError::CredentialMissing
                | AuthError::CredentialCorrupt
                | AuthError::SessionExpired
                | AuthError::Http { status: 401, .. }
        )
    )
}
impl AppearancePage {
    fn load_thumbnails(&mut self, cx: &mut Context<Self>) {
        use futures::{StreamExt, stream};
        let Some(profile) = self.model.baseline() else {
            return;
        };
        let capes = profile.capes().to_vec();
        let account = profile.account_id().to_owned();
        let generation = self.generation;
        let service = Core::global(cx).accounts();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let work = run(cx, async move {
            stream::iter(capes)
                .map(|cape| {
                    let service = service.clone();
                    let account = account.clone();
                    async move {
                        let (_tx, cancel) = tokio::sync::watch::channel(false);
                        let bytes = service
                            .appearance_texture(&account, cape.texture(), cancel)
                            .await
                            .ok()?;
                        let texture = CapeTexture::from_png(&bytes).ok()?;
                        let mut raster = image::imageops::resize(
                            &texture.thumbnail(),
                            20,
                            32,
                            image::imageops::FilterType::Nearest,
                        );
                        for pixel in raster.pixels_mut() {
                            pixel.0.swap(0, 2);
                        }
                        Some((cape.id().clone(), raster))
                    }
                })
                .buffer_unordered(4)
                .for_each(|result| {
                    let tx = tx.clone();
                    async move {
                        if let Some(result) = result {
                            let _ = tx.send(result).await;
                        }
                    }
                })
                .await;
        });
        self.thumb_task = cx.spawn(async move |this, cx| {
            let _work = work;
            while let Some((id, raster)) = rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        if this.generation != generation || !this.shown {
                            return false;
                        }
                        let image = Arc::new(RenderImage::new(vec![image::Frame::new(raster)]));
                        if let Some(old) = this.thumbs.insert(id, image) {
                            cx.defer(move |cx| cx.drop_image(old, None));
                        }
                        cx.notify();
                        true
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        });
    }
}
