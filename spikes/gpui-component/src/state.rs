//! Shared UI state: the instances, which one is selected and which global
//! section the rail shows. Views `observe` it and re-render on `notify`.

use gpui_kit::Context;

use crate::data::{self, Instance};

/// Global sections of the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Instances,
    Downloads,
    Accounts,
    Settings,
}

pub struct AppState {
    pub instances: Vec<Instance>,
    pub selected: usize,
    pub nav: Nav,
}

impl AppState {
    pub fn new() -> Self {
        let instances = data::instances();
        // `PATINA_SELECT=<index>` starts on another instance (screenshots).
        let selected = std::env::var("PATINA_SELECT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&i: &usize| i < instances.len())
            .unwrap_or(0);
        Self {
            instances,
            selected,
            nav: Nav::Instances,
        }
    }

    pub fn selected(&self) -> &Instance {
        &self.instances[self.selected]
    }

    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix != self.selected && ix < self.instances.len() {
            self.selected = ix;
            cx.notify();
        }
    }

    pub fn navigate(&mut self, nav: Nav, cx: &mut Context<Self>) {
        if nav != self.nav {
            self.nav = nav;
            cx.notify();
        }
    }
}
