/// Single source of truth for TUI keybindings.
///
/// Status bar and Help popup both render from this list.
/// Adding/changing a key here updates every surface.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Navigation,
    Selection,
    Account,
    Batch,
    Global,
}

impl Section {
    pub const fn label(self) -> &'static str {
        match self {
            Section::Navigation => "Navigation",
            Section::Selection => "Selection",
            Section::Account => "Account actions  (open via Enter)",
            Section::Batch => "Batch actions  (open via Enter when accounts marked)",
            Section::Global => "Global",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusBarIndicator {
    AutoRefresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusBarItem {
    pub label: &'static str,
    pub indicator: Option<StatusBarIndicator>,
}

impl StatusBarItem {
    const fn plain(label: &'static str) -> Self {
        Self {
            label,
            indicator: None,
        }
    }

    const fn with_indicator(label: &'static str, indicator: StatusBarIndicator) -> Self {
        Self {
            label,
            indicator: Some(indicator),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub keys: &'static str,
    pub section: Section,
    pub label: &'static str,
    pub status_bar: Option<StatusBarItem>,
}

/// Master keymap. Order matters: status bar renders top entries first;
/// Help popup groups by section in the order encountered.
pub const KEYMAP: &[Binding] = &[
    // Navigation
    Binding {
        keys: "j / k / ↑ ↓",
        section: Section::Navigation,
        label: "move selection",
        status_bar: None,
    },
    Binding {
        keys: "/",
        section: Section::Navigation,
        label: "search",
        status_bar: Some(StatusBarItem::plain("search")),
    },
    Binding {
        keys: "s",
        section: Section::Navigation,
        label: "cycle sort (name / quota / status)",
        status_bar: None,
    },
    // Selection
    Binding {
        keys: "space",
        section: Section::Selection,
        label: "toggle mark",
        status_bar: None,
    },
    Binding {
        keys: "esc",
        section: Section::Selection,
        label: "clear marks / search / popup",
        status_bar: None,
    },
    // Account actions (via Enter menu)
    Binding {
        keys: "r",
        section: Section::Account,
        label: "refresh account details",
        status_bar: None,
    },
    Binding {
        keys: "u",
        section: Section::Account,
        label: "use (switch to)",
        status_bar: None,
    },
    Binding {
        keys: "l",
        section: Section::Account,
        label: "re-login",
        status_bar: None,
    },
    Binding {
        keys: "n",
        section: Section::Account,
        label: "rename",
        status_bar: None,
    },
    Binding {
        keys: "w",
        section: Section::Account,
        label: "warmup",
        status_bar: None,
    },
    Binding {
        keys: "c",
        section: Section::Account,
        label: "confirm earliest reset card",
        status_bar: None,
    },
    Binding {
        keys: "d",
        section: Section::Account,
        label: "delete",
        status_bar: None,
    },
    // Batch actions
    Binding {
        keys: "r",
        section: Section::Batch,
        label: "refresh selected",
        status_bar: None,
    },
    Binding {
        keys: "w",
        section: Section::Batch,
        label: "warmup selected",
        status_bar: None,
    },
    Binding {
        keys: "l",
        section: Section::Batch,
        label: "re-login selected (sequential)",
        status_bar: None,
    },
    Binding {
        keys: "d",
        section: Section::Batch,
        label: "delete selected",
        status_bar: None,
    },
    // Global
    Binding {
        keys: "enter",
        section: Section::Global,
        label: "open menu (account or batch)",
        status_bar: Some(StatusBarItem::plain("menu")),
    },
    Binding {
        keys: "a",
        section: Section::Global,
        label: "add new account",
        status_bar: Some(StatusBarItem::plain("add new account")),
    },
    Binding {
        keys: "r",
        section: Section::Global,
        label: "refresh visible accounts",
        status_bar: Some(StatusBarItem::plain("refresh")),
    },
    Binding {
        keys: "t",
        section: Section::Global,
        label: "toggle auto-refresh",
        status_bar: Some(StatusBarItem::with_indicator(
            "auto refresh",
            StatusBarIndicator::AutoRefresh,
        )),
    },
    Binding {
        keys: "W",
        section: Section::Global,
        label: "toggle auto-warmup (short / weekly-only)",
        status_bar: None,
    },
    Binding {
        keys: "i",
        section: Section::Global,
        label: "show / hide account detail panel",
        status_bar: None,
    },
    Binding {
        keys: "h",
        section: Section::Global,
        label: "show this help",
        status_bar: Some(StatusBarItem::plain("help")),
    },
    Binding {
        keys: "q",
        section: Section::Global,
        label: "quit",
        status_bar: None,
    },
];

/// Build help text grouped by section. Returns a list of (heading, lines).
pub fn help_sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut result: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for binding in KEYMAP {
        let heading = binding.section.label();
        if let Some((_, items)) = result.iter_mut().find(|(h, _)| *h == heading) {
            items.push((binding.keys, binding.label));
        } else {
            result.push((heading, vec![(binding.keys, binding.label)]));
        }
    }
    result
}

/// Status bar items in display order.
pub fn status_bar_items() -> impl Iterator<Item = (&'static str, StatusBarItem)> {
    KEYMAP
        .iter()
        .filter_map(|binding| binding.status_bar.map(|item| (binding.keys, item)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_bar_contains_only_primary_actions() {
        let keys: Vec<_> = super::status_bar_items().map(|(keys, _)| keys).collect();
        assert_eq!(keys, ["/", "enter", "a", "r", "t", "h"]);
    }

    #[test]
    fn auto_refresh_status_bar_item_declares_its_indicator() {
        let (_, item) = super::status_bar_items()
            .find(|(keys, _)| *keys == "t")
            .expect("auto-refresh should be visible in the status bar");
        assert_eq!(item.label, "auto refresh");
        assert_eq!(item.indicator, Some(super::StatusBarIndicator::AutoRefresh));
    }

    #[test]
    fn help_retains_actions_hidden_from_the_status_bar() {
        let help_keys: Vec<_> = super::help_sections()
            .into_iter()
            .flat_map(|(_, items)| items.into_iter().map(|(keys, _)| keys))
            .collect();
        for key in ["j / k / ↑ ↓", "i", "q"] {
            assert!(help_keys.contains(&key), "missing {key:?} from Help");
        }
    }
}
