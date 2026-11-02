//! Vendor download dialog; transient selection belongs here. All disk/network
//! work crosses Core's runtime bridge, and a selection revision rejects late results.
use super::SettingsPage;
use crate::core_bridge::{ConfigChanged, Core, forward_progress, run};
use crate::instances::redetect_java;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    dialog::DialogFooter,
    progress::Progress,
    select::{Select, SelectEvent, SelectState},
};
use gpui_kit::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, WeakEntity, Window,
    div,
};
use patina_core::java::{
    catalog::{JavaRelease, JavaVendor},
    distributions::InstalledDistribution,
};
use rust_i18n::t;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

type Choice = Entity<SelectState<Vec<String>>>;
struct JavaDownload {
    owner: WeakEntity<SettingsPage>,
    vendor: Choice,
    major: Choice,
    build: Choice,
    vendor_value: JavaVendor,
    major_value: Option<u32>,
    releases: Vec<JavaRelease>,
    revision: u64,
    loading: bool,
    running: bool,
    error: Option<String>,
    status: Option<String>,
    installed: Option<InstalledDistribution>,
    update_from: Option<JavaRelease>,
    cancel: Arc<AtomicBool>,
    progress: Option<patina_core::progress::TaskProgress>,
    request: Task<()>,
    download: Task<()>,
    forward: Task<()>,
    _subscriptions: Vec<Subscription>,
}
impl SettingsPage {
    pub(super) fn open_java_download(
        &mut self,
        update_from: Option<JavaRelease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = cx.weak_entity();
        let view = cx.new(|cx| JavaDownload::new(owner, update_from, window, cx));
        let close = view.downgrade();
        let body = view.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(t!("java_download.title").to_string())
                .child(body.clone())
                .on_ok({
                    let body = body.clone();
                    move |_, _, cx| {
                        body.update(cx, |view, cx| view.install(cx));
                        false
                    }
                })
                .on_close({
                    let close = close.clone();
                    move |_, _, cx| {
                        close
                            .update(cx, |v, _| {
                                v.cancel.store(true, Ordering::Relaxed);
                            })
                            .ok();
                    }
                })
        });
        view.update(cx, |v, cx| v.load_majors(window, cx));
    }
    pub(super) fn confirm_delete_distribution(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.removing.contains(&id) {
            return;
        }
        let this = cx.weak_entity();
        let title = t!("prefs.runtime_delete_title", name = id).to_string();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            let id = id.clone();
            dialog
                .title(title.clone())
                .description(t!("java_download.delete_body").to_string())
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("java-delete-cancel")
                                .label(t!("prefs.cancel").to_string())
                                .on_click(|_, w, cx| w.close_dialog(cx)),
                        )
                        .child(
                            Button::new("java-delete-confirm")
                                .danger()
                                .label(t!("prefs.runtime_delete_confirm").to_string())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    this.update(cx, |this, cx| {
                                        this.delete_distribution(id.clone(), window, cx)
                                    })
                                    .ok();
                                }),
                        ),
                )
                .on_ok(|_, _, cx| {
                    cx.propagate();
                    false
                })
        });
    }
    fn delete_distribution(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.removing.insert(id.clone()) {
            return;
        }
        let job = Core::remove_java_distribution(cx, id.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                this.removing.remove(&id);
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => Some(e.to_string()),
                };
                if let Some(error) = error {
                    window.open_alert_dialog(cx, move |dialog, _, _| {
                        dialog
                            .title(t!("java_download.delete_blocked").to_string())
                            .description(
                                div()
                                    .id("java-delete-reason")
                                    .test_support()
                                    .aria_label(error.clone())
                                    .child(error.clone()),
                            )
                    });
                }
                this.reload_runtimes(cx);
                redetect_java(&this.java_list, cx);
                ConfigChanged::bump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
impl JavaDownload {
    fn new(
        owner: WeakEntity<SettingsPage>,
        update_from: Option<JavaRelease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let vendor_value = update_from
            .as_ref()
            .map_or(JavaVendor::Temurin, |r| r.vendor());
        let vendor = cx.new(|cx| {
            SelectState::new(
                JavaVendor::ALL.map(|v| v.name().to_string()).to_vec(),
                None,
                window,
                cx,
            )
        });
        vendor.update(cx, |s, cx| {
            s.set_selected_value(&vendor_value.name().to_string(), window, cx)
        });
        let major = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let build = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &vendor,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event
                        && let Some(vendor) =
                            JavaVendor::ALL.into_iter().find(|v| v.name() == value)
                        && this.vendor_value != vendor
                    {
                        this.vendor_value = vendor;
                        this.load_majors(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &major,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event
                        && let Ok(major) = value.parse()
                    {
                        this.major_value = Some(major);
                        this.load_builds(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &build,
                window,
                |this, _, _: &SelectEvent<Vec<String>>, _, cx| {
                    this.installed = None;
                    this.status = None;
                    cx.notify();
                },
            ),
        ];
        Self {
            owner,
            vendor,
            major,
            build,
            vendor_value,
            major_value: None,
            releases: Vec::new(),
            revision: 0,
            loading: false,
            running: false,
            error: None,
            status: None,
            installed: None,
            update_from,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: None,
            request: Task::ready(()),
            download: Task::ready(()),
            forward: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }
    fn clear_builds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revision += 1;
        self.releases.clear();
        self.build.update(cx, |s, cx| {
            s.set_items(Vec::new(), window, cx);
            s.set_selected_index(None, window, cx);
        });
        self.error = None;
        self.status = None;
        self.installed = None;
        self.loading = true;
        cx.notify();
    }
    fn load_majors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_builds(window, cx);
        self.major_value = None;
        self.major.update(cx, |s, cx| {
            s.set_items(Vec::new(), window, cx);
            s.set_selected_index(None, window, cx);
        });
        let rev = self.revision;
        let vendor = self.vendor_value;
        let catalog = Core::global(cx).java_catalog.clone();
        let job = run(cx, async move { catalog.majors(vendor).await });
        self.request = cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                if this.revision != rev {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(majors)) => {
                        let majors = if let Some(old) = &this.update_from {
                            majors
                                .into_iter()
                                .filter(|m| *m == old.major())
                                .collect::<Vec<_>>()
                        } else {
                            majors
                        };
                        this.major.update(cx, |s, cx| {
                            s.set_items(majors.iter().map(u32::to_string).collect(), window, cx)
                        });
                        if let Some(major) = this
                            .update_from
                            .as_ref()
                            .map(|r| r.major())
                            .filter(|m| majors.contains(m))
                        {
                            this.major_value = Some(major);
                            this.major.update(cx, |s, cx| {
                                s.set_selected_value(&major.to_string(), window, cx)
                            });
                            this.load_builds(window, cx);
                        } else if majors.is_empty() {
                            this.status = Some(t!("java_download.unavailable").to_string());
                        }
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        });
    }
    fn load_builds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_builds(window, cx);
        let Some(major) = self.major_value else {
            return;
        };
        let rev = self.revision;
        let vendor = self.vendor_value;
        let catalog = Core::global(cx).java_catalog.clone();
        let job = run(cx, async move { catalog.releases(vendor, major).await });
        self.request = cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            this.update_in(cx, |this, window, cx| {
                if this.revision != rev {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(mut releases)) => {
                        let unavailable = releases.is_empty();
                        if let Some(old) = &this.update_from {
                            releases.retain(|r| r.is_newer_than(old));
                        }
                        if releases.is_empty() {
                            this.status = Some(
                                if unavailable {
                                    t!("java_download.unavailable")
                                } else {
                                    t!("java_download.up_to_date")
                                }
                                .to_string(),
                            );
                        }
                        this.build.update(cx, |s, cx| {
                            s.set_items(
                                releases.iter().map(|r| r.version().to_string()).collect(),
                                window,
                                cx,
                            );
                            if !releases.is_empty() {
                                s.set_selected_index(Some(Default::default()), window, cx);
                            }
                        });
                        this.releases = releases;
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        });
    }
    fn selected(&self, cx: &App) -> Option<JavaRelease> {
        let version = self.build.read(cx).selected_value()?;
        self.releases
            .iter()
            .find(|r| {
                r.version() == version
                    && r.vendor() == self.vendor_value
                    && Some(r.major()) == self.major_value
            })
            .cloned()
    }
    fn install(&mut self, cx: &mut Context<Self>) {
        if self.running || self.loading || self.installed.is_some() {
            return;
        }
        let Some(release) = self.selected(cx) else {
            return;
        };
        self.running = true;
        self.error = None;
        self.status = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.progress = None;
        let cancel = self.cancel.clone();
        let core = Core::global(cx).clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let weak = cx.weak_entity();
        self.forward = forward_progress(cx, weak, rx, |this, p, cx| {
            this.progress = Some(p);
            cx.notify();
        });
        let job = run(cx, async move {
            core.java_distributions
                .install(&core.java_downloader, &release, &Some(tx), cancel)
                .await
        });
        self.download = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.running = false;
                match result {
                    Ok(Ok(installed)) => {
                        this.installed = Some(installed);
                        this.status = Some(t!("java_download.complete").to_string());
                        this.owner
                            .update(cx, |owner, cx| {
                                owner.reload_runtimes(cx);
                                redetect_java(&owner.java_list, cx);
                                ConfigChanged::bump(cx);
                            })
                            .ok();
                    }
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }
}
impl Drop for JavaDownload {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Render for JavaDownload {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected(cx);
        let fixed = self.update_from.is_some();
        let disabled = self.running;
        let size = selected
            .as_ref()
            .and_then(|r| r.size())
            .map(|n| format!("{:.1} MiB", n as f64 / 1048576.))
            .unwrap_or_else(|| t!("java_download.size_unknown").to_string());
        let field = |label: String, control: gpui_kit::AnyElement| {
            v_flex().gap_1().child(div().child(label)).child(control)
        };
        v_flex()
            .id("java-download-dialog")
            .test_support()
            .gap_4()
            .child(field(
                t!("java_download.vendor").to_string(),
                Select::new(&self.vendor)
                    .id("java-download-vendor")
                    .w_full()
                    .disabled(disabled || fixed)
                    .accessibility_label(t!("java_download.vendor").to_string())
                    .into_any_element(),
            ))
            .child(field(
                t!("java_download.major").to_string(),
                Select::new(&self.major)
                    .id("java-download-major")
                    .w_full()
                    .disabled(disabled || fixed || self.loading)
                    .accessibility_label(t!("java_download.major").to_string())
                    .into_any_element(),
            ))
            .child(field(
                t!("java_download.build").to_string(),
                Select::new(&self.build)
                    .id("java-download-build")
                    .w_full()
                    .disabled(disabled || self.loading || self.releases.is_empty())
                    .accessibility_label(t!("java_download.build").to_string())
                    .into_any_element(),
            ))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Linux x86_64 · JDK · {size}")),
            )
            .children(self.loading.then(|| {
                div()
                    .id("java-download-loading")
                    .test_support()
                    .child(t!("java_download.loading").to_string())
            }))
            .children(self.status.as_ref().map(|s| {
                div()
                    .id("java-download-status")
                    .test_support()
                    .aria_label(s.clone())
                    .child(s.clone())
            }))
            .children(self.error.as_ref().map(|e| {
                div()
                    .id("java-download-error")
                    .test_support()
                    .aria_label(e.clone())
                    .text_color(cx.theme().danger)
                    .child(e.clone())
            }))
            .children(self.running.then(|| {
                let p = self.progress;
                v_flex()
                    .gap_2()
                    .child(div().child(p.map_or_else(
                        || t!("java_download.loading").to_string(),
                        |p| p.phase.display_name(),
                    )))
                    .child(
                        Progress::new("java-download-progress")
                            .loading(p.is_none_or(|p| p.total == 0))
                            .value(
                                p.filter(|p| p.total > 0)
                                    .map_or(0., |p| p.done as f32 / p.total as f32 * 100.),
                            ),
                    )
                    .children(
                        p.filter(|p| p.unit == patina_core::progress::Unit::Bytes)
                            .map(|p| {
                                div().id("java-download-bytes").test_support().child(
                                    t!(
                                        "java_download.bytes",
                                        amount = format!("{:.1}", p.done as f64 / 1048576.)
                                    )
                                    .to_string(),
                                )
                            }),
                    )
                    .child(
                        Button::new("java-download-cancel")
                            .label(t!("prefs.cancel").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel.store(true, Ordering::Relaxed);
                                cx.notify();
                            })),
                    )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("java-download-close")
                            .label(t!("java_download.close").to_string())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .children((self.installed.is_none() && !self.running).then(|| {
                        Button::new("java-download-install")
                            .primary()
                            .label(t!("java_download.install").to_string())
                            .disabled(selected.is_none() || self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.install(cx)))
                    }))
                    .children((self.error.is_some() && !self.running).then(|| {
                        Button::new("java-download-retry")
                            .label(t!("java_download.retry").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.major_value.is_some() {
                                    this.load_builds(window, cx);
                                } else {
                                    this.load_majors(window, cx);
                                }
                            }))
                    }))
                    .children(self.installed.as_ref().map(|i| {
                        let path = i.java().path.clone();
                        let owner = self.owner.clone();
                        Button::new("java-download-default")
                            .label(t!("java_download.set_default").to_string())
                            .on_click(move |_, window, cx| {
                                owner
                                    .update(cx, |owner, cx| {
                                        owner.set_java(Some(path.clone()), window, cx)
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            })
                    })),
            )
    }
}
