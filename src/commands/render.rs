use crate::{color, usage};

fn read_confirmation(prompt: &str) -> Option<String> {
    use std::io::{self, Write as _};

    eprint!("{}", color::dim(prompt));
    io::stderr().flush().ok();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(input.trim().to_lowercase()),
    }
}

/// Prompt the user for Y/n confirmation. Returns false on EOF or explicit "n"/"no".
pub(crate) fn confirm(prompt: &str) -> bool {
    read_confirmation(prompt).is_some_and(|answer| !matches!(answer.as_str(), "n" | "no"))
}

/// Prompt the user for y/N confirmation. Only an explicit "y" or "yes" accepts.
pub(crate) fn confirm_default_no(prompt: &str) -> bool {
    read_confirmation(prompt).is_some_and(|answer| matches!(answer.as_str(), "y" | "yes"))
}

pub(crate) fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Render a progress bar without outer brackets.
/// `=` for used portion, `-` for remaining, `|` for pace marker.
pub(crate) fn render_progress_bar(
    used_pct: f64,
    pace_pct: Option<f64>,
    bar_width: usize,
) -> String {
    let used_pos = ((used_pct / 100.0) * bar_width as f64)
        .round()
        .clamp(0.0, bar_width as f64) as usize;
    let pace_pos = pace_pct.map(|p| {
        ((p / 100.0) * bar_width as f64)
            .round()
            .clamp(0.0, (bar_width.saturating_sub(1)) as f64) as usize
    });

    let mut bar = String::with_capacity(bar_width);
    for i in 0..bar_width {
        if pace_pos == Some(i) {
            bar.push('|');
        } else if i < used_pos {
            bar.push('=');
        } else {
            bar.push('-');
        }
    }
    bar
}

/// Format relative reset time: "~2h17m" or "~4d18h"
pub(crate) fn format_reset_short_relative(w: &usage::WindowUsage) -> String {
    let Some(resets_at) = w.resets_at else {
        return "--".into();
    };
    let remaining_secs = (resets_at - crate::auth::now_unix_secs()).max(0) as u64;
    if remaining_secs == 0 {
        return "expired".into();
    }
    if remaining_secs < 3600 {
        format!("~{}m", remaining_secs / 60)
    } else if remaining_secs < 86400 {
        format!(
            "~{}h{}m",
            remaining_secs / 3600,
            (remaining_secs % 3600) / 60
        )
    } else {
        format!(
            "~{}d{}h",
            remaining_secs / 86400,
            (remaining_secs % 86400) / 3600
        )
    }
}

struct QuotaWindowParts {
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    pace_percent: Option<f64>,
    bar: String,
}

fn quota_window_parts(
    window: &usage::WindowUsage,
    window_secs: i64,
    bar_width: usize,
) -> QuotaWindowParts {
    let used_percent = usage::normalized_quota_usage(window.used_percent);
    let pace_percent = usage::pace_percent(window, window_secs);
    let marker_pace = usage::visible_pace_marker(used_percent, pace_percent);
    let bar = match used_percent {
        Some(used) => render_progress_bar(used, marker_pace, bar_width),
        None => "-".repeat(bar_width),
    };
    QuotaWindowParts {
        used_percent,
        remaining_percent: used_percent.map(|used| 100.0 - used),
        pace_percent,
        bar,
    }
}

/// Render one additional-limit pool's window as a compact segment, e.g.
/// "5h [====------] 60% left".
fn pool_window_segment(default_label: &str, w: &usage::WindowUsage, default_secs: i64) -> String {
    let (label, window_secs) = usage::quota_window_spec(w, default_label, default_secs);
    let parts = quota_window_parts(w, window_secs, 10);
    let remaining = parts
        .remaining_percent
        .map(|value| format!("{value:.0}% left"))
        .unwrap_or_else(|| "--% left".to_string());
    format!(
        "{} [{}] {}",
        color::dim(&label),
        color::usage_pace(&parts.bar, parts.used_percent, parts.pace_percent),
        color::usage_pace(&remaining, parts.used_percent, parts.pace_percent),
    )
}

/// Print one indented sub-line per additional-limit pool (e.g. per-model
/// quota pools on Pro 20x accounts). No-op when there are no additional pools.
pub(crate) fn print_additional_pool_lines(limits: &[usage::AdditionalRateLimit]) {
    for row in usage::additional_pool_rows(limits) {
        let mut segments = vec![format!(
            "  {} {}",
            color::dim("\u{2514}"),
            color::dim(&row.limit_name)
        )];
        if let Some(w) = &row.primary {
            segments.push(pool_window_segment("5h", w, usage::WINDOW_5H_SECS));
        }
        if let Some(w) = &row.secondary {
            segments.push(pool_window_segment("7d", w, usage::WINDOW_7D_SECS));
        }
        if row.unavailable {
            segments.push(color::error("[exhausted]"));
        }
        println!("{}", segments.join("  "));
    }
}

fn usage_window_line(
    default_label: &str,
    window: &usage::WindowUsage,
    default_secs: i64,
    bar_width: usize,
) -> String {
    let (label, window_secs) = usage::quota_window_spec(window, default_label, default_secs);
    let parts = quota_window_parts(window, window_secs, bar_width);
    let remaining = parts
        .remaining_percent
        .map(|value| format!("{value:>3.0}% left"))
        .unwrap_or_else(|| " --% left".to_string());
    let reset = format_reset_short_relative(window);
    format!(
        "  {label}  {}  {}   {}",
        color::usage_pace(&parts.bar, parts.used_percent, parts.pace_percent),
        color::usage_pace(&remaining, parts.used_percent, parts.pace_percent),
        color::dim(&reset),
    )
}

pub(crate) fn print_usage_line(u: &usage::UsageInfo) {
    let width = term_width();
    // Each line: "  5h  bar  XXX% left  ~Xh" ≈ bar_width + 30
    let bar_width = if width >= 80 {
        16
    } else if width >= 60 {
        10
    } else {
        6
    };

    if let Some(w) = &u.primary {
        println!(
            "{}",
            usage_window_line("5h", w, usage::WINDOW_5H_SECS, bar_width)
        );
    }
    if let Some(w) = &u.secondary {
        println!(
            "{}",
            usage_window_line("7d", w, usage::WINDOW_7D_SECS, bar_width)
        );
    }
    print_additional_pool_lines(&u.additional_limits);
    if let Some(balance) = u.credits_balance {
        let unlimited = u.unlimited_credits == Some(true);
        let text = if unlimited {
            "credits: unlimited".to_string()
        } else {
            format!("credits: ${balance:.2}")
        };
        println!("  {}", color::credits(&text, balance, unlimited));
    }
    for line in crate::output::reset_credits_detail_lines(u, 4) {
        println!("  {}", color::dim(&line));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_reset_short_relative, pool_window_segment, render_progress_bar, usage_window_line,
    };
    use crate::usage::WindowUsage;

    #[test]
    fn progress_bar_clamps_used_and_pace_positions() {
        assert_eq!(render_progress_bar(0.0, None, 10), "----------");
        assert_eq!(render_progress_bar(100.0, None, 10), "==========");
        assert_eq!(render_progress_bar(0.0, Some(150.0), 10), "---------|");
        assert_eq!(render_progress_bar(50.0, Some(50.0), 10), "=====|----");
    }

    #[test]
    fn usage_lines_never_append_a_warning_suffix() {
        let now = crate::auth::now_unix_secs();
        let window = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + crate::usage::WINDOW_7D_SECS - 60),
            window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
        };
        let rendered = usage_window_line("7d", &window, crate::usage::WINDOW_7D_SECS, 10);

        assert!(!rendered.contains('!'));
        assert!(rendered.contains("80% left"));
    }

    #[test]
    fn usage_lines_do_not_invent_missing_quota() {
        let window = WindowUsage {
            used_percent: None,
            resets_at: Some(crate::auth::now_unix_secs() + 60),
            window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
        };
        let rendered = usage_window_line("7d", &window, crate::usage::WINDOW_7D_SECS, 10);

        assert!(rendered.contains("--% left"));
        assert!(!rendered.contains("100% left"));
        assert!(!rendered.contains('|'));
    }

    #[test]
    fn additional_pool_uses_its_reported_window_duration() {
        let window = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(crate::auth::now_unix_secs() + crate::usage::WINDOW_7D_SECS / 2),
            window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
        };
        let rendered = pool_window_segment("5h", &window, crate::usage::WINDOW_5H_SECS);

        assert!(rendered.contains("7d ["));
        assert!(!rendered.contains("5h ["));
    }

    fn reset_after(seconds: i64) -> WindowUsage {
        WindowUsage {
            resets_at: Some(crate::auth::now_unix_secs() + seconds),
            ..WindowUsage::default()
        }
    }

    #[test]
    fn short_relative_reset_uses_minute_hour_and_day_boundaries() {
        assert_eq!(
            format_reset_short_relative(&reset_after(59 * 60 + 30)),
            "~59m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(60 * 60 + 30)),
            "~1h0m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(23 * 60 * 60 + 59 * 60 + 30)),
            "~23h59m"
        );
        assert_eq!(
            format_reset_short_relative(&reset_after(24 * 60 * 60 + 30)),
            "~1d0h"
        );
    }
}
