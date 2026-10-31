//! Which page the main window shows ([`Nav`]) and its owner ([`ShellState`]).

use gpui_kit::Context;
use rust_i18n::t;

/// A destination of the left rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nav {
    #[default]
    Instances,
    Downloads,
    Accounts,
    Settings,
}

impl Nav {
    /// The destination's name, as the rail's tooltip and a page title.
    pub fn label(self) -> String {
        match self {
            Nav::Instances => t!("shell.nav.instances"),
            Nav::Downloads => t!("shell.nav.downloads"),
            Nav::Accounts => t!("shell.nav.accounts"),
            Nav::Settings => t!("shell.nav.settings"),
        }
        .to_string()
    }

    /// Element id of the destination's rail button.
    pub fn rail_id(self) -> &'static str {
        match self {
            Nav::Instances => "rail-instances",
            Nav::Downloads => "rail-downloads",
            Nav::Accounts => "rail-accounts",
            Nav::Settings => "rail-settings",
        }
    }

    /// Element id of the destination's page in the main area.
    pub fn page_id(self) -> &'static str {
        match self {
            Nav::Instances => "page-instances",
            Nav::Downloads => "page-downloads",
            Nav::Accounts => "page-accounts",
            Nav::Settings => "page-settings",
        }
    }
}

/// The main window's navigation state (an entity). The rail, the title bar
/// and the root view share it; observers are notified only when the page
/// actually changes.
#[derive(Debug, Default)]
pub struct ShellState {
    nav: Nav,
}

impl ShellState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The page on show.
    pub fn nav(&self) -> Nav {
        self.nav
    }

    /// Shows `nav`; notifies only if that changes the page.
    pub fn navigate(&mut self, nav: Nav, cx: &mut Context<Self>) {
        if self.nav == nav {
            return;
        }
        self.nav = nav;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{AppContext, TestAppContext};

    use super::*;

    #[gpui_kit::test]
    fn navigate_notifies_only_on_change(cx: &mut TestAppContext) {
        let shell = cx.new(|_| ShellState::new());
        let notified = Rc::new(Cell::new(0));
        let _observe = cx.update(|cx| {
            let notified = notified.clone();
            cx.observe(&shell, move |_, _| notified.set(notified.get() + 1))
        });
        assert_eq!(shell.read_with(cx, |s, _| s.nav()), Nav::Instances);

        shell.update(cx, |s, cx| s.navigate(Nav::Instances, cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 0, "already on that page");

        shell.update(cx, |s, cx| s.navigate(Nav::Accounts, cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 1);
        assert_eq!(shell.read_with(cx, |s, _| s.nav()), Nav::Accounts);

        shell.update(cx, |s, cx| s.navigate(Nav::Accounts, cx));
        shell.update(cx, |s, cx| s.navigate(Nav::Settings, cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 2);
    }
}
