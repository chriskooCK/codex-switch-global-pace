use std::sync::OnceLock;

use owo_colors::OwoColorize;

use crate::cli::ColorMode;
use crate::jwt::PlanKind;
use crate::usage::{QuotaPaceState, quota_pace_state};

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Initialize color support. Call once at startup.
pub fn init(mode: ColorMode) {
    let enabled = match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            // Respect NO_COLOR convention (https://no-color.org)
            if std::env::var_os("NO_COLOR").is_some() {
                return ENABLED.set(false).unwrap_or(());
            }
            // Check if stdout is a terminal with color support
            supports_color::on(supports_color::Stream::Stdout).is_some()
        }
    };
    let _ = ENABLED.set(enabled);
}

/// Whether color output is enabled.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        supports_color::on(supports_color::Stream::Stdout).is_some()
    })
}

// ── Styled output helpers for CLI ─────────────────────────

/// Green text for success
pub fn success(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().green())
    } else {
        s.into_owned()
    }
}

/// Red text for errors
pub fn error(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().red())
    } else {
        s.into_owned()
    }
}

/// Yellow text for warnings
pub fn warn(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().yellow())
    } else {
        s.into_owned()
    }
}

/// Dim/gray text
pub fn dim(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().dimmed())
    } else {
        s.into_owned()
    }
}

/// Bold text
pub fn bold(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().bold())
    } else {
        s.into_owned()
    }
}

/// Green bold for active marker
pub fn active(s: &str) -> String {
    let s = crate::safe_text::terminal_text(s);
    if enabled() {
        format!("{}", s.as_ref().green().bold())
    } else {
        s.into_owned()
    }
}

/// Color quota usage by its position relative to pace.
pub fn usage_pace(s: &str, used_percent: Option<f64>, pace_percent: Option<f64>) -> String {
    let s = crate::safe_text::terminal_text(s);
    if !enabled() {
        return s.into_owned();
    }
    colored_usage_pace(s.as_ref(), used_percent, pace_percent)
}

fn colored_usage_pace(s: &str, used_percent: Option<f64>, pace_percent: Option<f64>) -> String {
    match quota_pace_state(used_percent, pace_percent) {
        QuotaPaceState::UsageAhead => format!("{}", s.yellow()),
        QuotaPaceState::PaceAheadOrEqual => format!("{}", s.green()),
        QuotaPaceState::Unavailable => format!("{}", s.dimmed()),
    }
}

/// Color a credits balance: green >= $10, yellow >= $2, red < $2
pub fn credits(s: &str, balance: f64, unlimited: bool) -> String {
    let s = crate::safe_text::terminal_text(s);
    if !enabled() {
        return s.into_owned();
    }
    if unlimited || balance >= 10.0 {
        format!("{}", s.as_ref().green())
    } else if balance >= 2.0 {
        format!("{}", s.as_ref().yellow())
    } else {
        format!("{}", s.as_ref().red())
    }
}

/// Color a status tag: OK = green, Limited = red, Error = red
pub fn status_tag(tag: &str) -> String {
    let tag = crate::safe_text::terminal_text(tag);
    if !enabled() {
        return format!("[{tag}]");
    }
    match tag.as_ref() {
        "OK" => format!("[{}]", tag.as_ref().green()),
        "Limited" | "Error" => format!("[{}]", tag.as_ref().red()),
        _ => format!("[{tag}]"),
    }
}

/// Color a plan label by type
pub fn plan(label: &str, plan_type: Option<&str>) -> String {
    let label = crate::safe_text::terminal_text(label);
    if !enabled() {
        return format!("[{label}]");
    }
    colored_plan(label.as_ref(), plan_type)
}

fn colored_plan(label: &str, plan_type: Option<&str>) -> String {
    match PlanKind::from_wire(plan_type) {
        PlanKind::Free | PlanKind::Unknown => format!("[{}]", label.bright_black()),
        PlanKind::Go => format!("[{}]", label.bright_blue()),
        PlanKind::Plus => format!("[{}]", label.cyan()),
        PlanKind::ProLite => format!("[{}]", label.yellow()),
        PlanKind::Pro => format!("[{}]", label.bright_yellow().bold()),
        PlanKind::Team | PlanKind::Business | PlanKind::Edu => {
            format!("[{}]", label.magenta())
        }
        PlanKind::Enterprise => format!("[{}]", label.bright_magenta().bold()),
    }
}

#[cfg(test)]
mod tests {
    use super::{colored_plan, colored_usage_pace, error};

    #[test]
    fn cli_styling_never_replays_untrusted_terminal_controls() {
        let rendered = error("bad\u{1b}]52;clipboard\u{7}\nnext");
        assert!(!rendered.contains("]52;clipboard\u{7}"));
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("bad]52;clipboardnext"));
    }

    #[test]
    fn plan_colors_distinguish_go_and_both_pro_tiers() {
        assert!(colored_plan("Go", Some("go")).contains("\u{1b}[94m"));
        assert!(colored_plan("Pro 5×", Some("prolite")).contains("\u{1b}[33m"));
        let pro = colored_plan("Pro 20×", Some("pro"));
        assert!(pro.contains("\u{1b}[93m"));
        assert!(pro.contains("\u{1b}[1m"));
    }

    #[test]
    fn quota_colors_use_only_relative_pace_states() {
        assert!(colored_usage_pace("x", Some(1.0), Some(0.0)).contains("\u{1b}[33m"));
        assert!(colored_usage_pace("x", Some(50.0), Some(50.0)).contains("\u{1b}[32m"));
        assert!(colored_usage_pace("x", Some(95.0), Some(99.0)).contains("\u{1b}[32m"));
        assert!(colored_usage_pace("x", Some(100.0), Some(50.0)).contains("\u{1b}[33m"));
        assert!(colored_usage_pace("x", Some(20.0), None).contains("\u{1b}[2m"));
    }
}
