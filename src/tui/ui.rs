use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::app::{App, UsageStatus};
use super::keymap;
use super::popup;
use crate::jwt::PlanKind;
use crate::output::{
    format_local_time, format_reset_short, format_reset_time, reset_credits_count,
};
use crate::usage::{GlobalWeeklySummary, UsageInfo, is_available};

// ── RGB-only color palette ───────────────────────────────
// All colors are explicit RGB to avoid mixing ANSI-16 + 24-bit,
// which causes rendering glitches on Windows conhost (cmd.exe / PowerShell).

const BG: Color = Color::Rgb(24, 24, 24); // near-black background
const C_WHITE: Color = Color::Rgb(240, 240, 240); // primary text
const C_GRAY: Color = Color::Rgb(180, 180, 180); // secondary text
const DIM: Color = Color::Rgb(120, 120, 120); // dim labels / placeholders
const C_RED: Color = Color::Rgb(255, 90, 90); // errors, warnings
const C_GREEN: Color = Color::Rgb(80, 220, 120); // OK, active
const C_YELLOW: Color = Color::Rgb(255, 220, 80); // keys, markers
const C_CYAN: Color = Color::Rgb(100, 210, 255); // headers, prompts
const C_MAGENTA: Color = Color::Rgb(220, 130, 255); // team plans
const C_BLUE: Color = Color::Rgb(80, 140, 220); // borders (inactive)
const C_HIGHLIGHT_BG: Color = Color::Rgb(55, 55, 65); // selected row bg

const GLOBAL_PACE_HEALTHY_MIN: f64 = 100.0;
const GLOBAL_PACE_WARNING_MIN: f64 = 90.0;
const GLOBAL_WEEKLY_FULL_HEIGHT: u16 = 5;
const GLOBAL_WEEKLY_COMPACT_HEIGHT: u16 = 1;
const MIN_ACCOUNT_TABLE_HEIGHT: u16 = 6;
const USAGE_BAR_LABEL_COLUMN_WIDTH: u16 = 6;
const USAGE_BAR_SUFFIX_WIDTH: u16 = 24;
const PACE_LABEL: &str = "\u{2191} pace";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UsageBarAreas {
    label: Rect,
    bar: Rect,
    suffix: Rect,
}

fn base() -> Style {
    Style::default().bg(BG)
}

fn usage_bar_areas(area: Rect) -> UsageBarAreas {
    let row = Rect {
        height: area.height.min(1),
        ..area
    };
    let label_column_width = row.width.min(USAGE_BAR_LABEL_COLUMN_WIDTH);
    let remaining_width = row.width.saturating_sub(label_column_width);
    let suffix_width = remaining_width.min(USAGE_BAR_SUFFIX_WIDTH);
    let bar_width = remaining_width.saturating_sub(suffix_width);
    let bar_x = row.x.saturating_add(label_column_width);
    let suffix_x = bar_x.saturating_add(bar_width);

    UsageBarAreas {
        label: Rect {
            // Keep the final column blank so dynamic window labels never run
            // directly into the first meter cell.
            width: label_column_width.saturating_sub(1),
            ..row
        },
        bar: Rect {
            x: bar_x,
            width: bar_width,
            ..row
        },
        suffix: Rect {
            x: suffix_x,
            width: suffix_width,
            ..row
        },
    }
}

fn render_usage_bar_row(
    f: &mut Frame,
    areas: UsageBarAreas,
    label: Line<'static>,
    bar: Line<'static>,
    suffix: Line<'static>,
) {
    f.render_widget(Paragraph::new(label).style(base()), areas.label);
    f.render_widget(Paragraph::new(bar).style(base()), areas.bar);
    f.render_widget(Paragraph::new(suffix).style(base()), areas.suffix);
}

/// Map a percentage on a 0..100 meter to its marker cell.
/// Zero is the first cell and 100 is the last cell.
fn percent_marker_offset(percent: f64, width: u16) -> Option<u16> {
    if width == 0 || !percent.is_finite() {
        return None;
    }

    let last_cell = width - 1;
    Some(
        ((percent.clamp(0.0, 100.0) / 100.0) * f64::from(last_cell))
            .round()
            .clamp(0.0, f64::from(last_cell)) as u16,
    )
}

fn meter_fill_width(percent: f64, width: u16) -> u16 {
    debug_assert!(percent.is_finite());
    // Keep a small nonzero usage segment visible instead of rounding it away.
    ((percent.clamp(0.0, 100.0) / 100.0) * f64::from(width))
        .ceil()
        .clamp(0.0, f64::from(width)) as u16
}

fn usage_meter_line(
    fill_percent: f64,
    marker_offset: Option<u16>,
    width: u16,
    fill_style: Style,
    remaining_style: Style,
    marker_style: Style,
) -> Line<'static> {
    let fill_width = meter_fill_width(fill_percent, width);
    let mut spans = Vec::new();

    if let Some(marker) = marker_offset {
        debug_assert!(marker < width);
        let before_fill = marker.min(fill_width);
        let before_remaining = marker.saturating_sub(fill_width);
        let after_fill = fill_width.saturating_sub(marker + 1);
        let after_remaining = width.saturating_sub(marker + 1 + after_fill);

        if before_fill > 0 {
            spans.push(Span::styled("█".repeat(before_fill.into()), fill_style));
        }
        if before_remaining > 0 {
            spans.push(Span::styled(
                "░".repeat(before_remaining.into()),
                remaining_style,
            ));
        }
        spans.push(Span::styled("|", marker_style));
        if after_fill > 0 {
            spans.push(Span::styled("█".repeat(after_fill.into()), fill_style));
        }
        if after_remaining > 0 {
            spans.push(Span::styled(
                "░".repeat(after_remaining.into()),
                remaining_style,
            ));
        }
    } else {
        if fill_width > 0 {
            spans.push(Span::styled("█".repeat(fill_width.into()), fill_style));
        }
        if width > fill_width {
            spans.push(Span::styled(
                "░".repeat((width - fill_width).into()),
                remaining_style,
            ));
        }
    }

    Line::from(spans)
}

fn render_pace_label(f: &mut Frame, row: Rect, bar: Rect, marker_offset: Option<u16>) {
    let Some(marker_offset) = marker_offset else {
        return;
    };
    let x = bar.x.saturating_add(marker_offset);
    let label_width = u16::try_from(display_width(PACE_LABEL)).unwrap_or(u16::MAX);
    if x < row.x || x.saturating_add(label_width) > row.right() {
        return;
    }
    let marker_area = Rect {
        x,
        width: label_width,
        height: row.height.min(1),
        ..row
    };
    f.render_widget(
        Paragraph::new(PACE_LABEL).style(base().fg(DIM)),
        marker_area,
    );
}

fn fitted_segment_suffix(
    base_width: usize,
    max_width: usize,
    segments: impl IntoIterator<Item = String>,
) -> String {
    let mut used_width = base_width;
    let mut suffix = String::new();

    for segment in segments {
        let decorated = format!(" · {segment}");
        let segment_width = display_width(&decorated);
        let Some(next_width) = used_width.checked_add(segment_width) else {
            break;
        };
        if next_width > max_width {
            break;
        }
        suffix.push_str(&decorated);
        used_width = next_width;
    }

    suffix
}

fn status_message_color(is_error: bool) -> Color {
    if is_error { C_RED } else { C_CYAN }
}

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Paint the entire area with a solid background first
    f.render_widget(Block::default().style(base()), area);

    let status_height = status_bar_height(app, area.width);
    let global_height = global_weekly_panel_height(area.height, status_height as u16);

    let detail_height = if app.detail_visible {
        let available = area
            .height
            .saturating_sub(status_height as u16)
            .saturating_sub(global_height)
            .saturating_sub(MIN_ACCOUNT_TABLE_HEIGHT);
        let constrained = detail_panel_height(app).min(available);
        // A bordered panel below three rows has no usable content. Give those
        // rows back to the account list instead of drawing an empty border.
        if constrained >= 3 { constrained } else { 0 }
    } else {
        0
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(MIN_ACCOUNT_TABLE_HEIGHT), // account list
            Constraint::Length(global_height),         // global weekly pace
            Constraint::Length(detail_height),         // detail panel
            Constraint::Length(status_height as u16),  // status bar
        ])
        .split(area);

    render_account_table(f, app, vertical[0]);
    let now = crate::auth::now_unix_secs();
    let global_weekly = app.global_weekly_summary(now);
    render_global_weekly_pace(f, &global_weekly, now, vertical[1]);
    if app.detail_visible && detail_height > 0 {
        render_detail_panel(f, app, vertical[2]);
    }
    render_status_bar(f, app, vertical[3]);

    // Overlays (rendered last, on top of everything).
    // Help popup takes top priority since the user invoked it explicitly.
    if let Some(state) = app.help_popup.as_mut() {
        render_help_popup(f, state, area);
    } else if let Some(menu) = app.menu.as_mut() {
        menu.render(f, area);
    }
}

fn global_weekly_panel_height(total_height: u16, status_height: u16) -> u16 {
    let body_height = total_height.saturating_sub(status_height);
    if body_height >= MIN_ACCOUNT_TABLE_HEIGHT + GLOBAL_WEEKLY_FULL_HEIGHT {
        GLOBAL_WEEKLY_FULL_HEIGHT
    } else if body_height >= 2 {
        GLOBAL_WEEKLY_COMPACT_HEIGHT
    } else {
        0
    }
}

fn global_pace_color(pace_percent: Option<f64>) -> Color {
    match pace_percent {
        Some(pace) if pace >= GLOBAL_PACE_HEALTHY_MIN => C_GREEN,
        Some(pace) if pace >= GLOBAL_PACE_WARNING_MIN => C_YELLOW,
        Some(_) => C_RED,
        None => DIM,
    }
}

fn render_global_weekly_pace(f: &mut Frame, summary: &GlobalWeeklySummary, now: i64, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if area.height == GLOBAL_WEEKLY_COMPACT_HEIGHT {
        f.render_widget(
            Paragraph::new(compact_global_weekly_line(
                summary,
                now,
                area.width as usize,
            ))
            .style(base()),
            area,
        );
        return;
    }

    let block = Block::default()
        .title(format!(
            " Global Weekly Pace · 100% normal · {} weight ",
            summary.weighting.as_str()
        ))
        .borders(Borders::ALL)
        .border_style(base().fg(C_BLUE))
        .style(base());
    let inner = block.inner(area);
    let content = Rect {
        x: inner.x.saturating_add(1),
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let pace_color = global_pace_color(summary.pace_percent);

    f.render_widget(block, area);

    let gauge_area = Rect {
        height: content.height.min(1),
        ..content
    };
    let mut global_marker = None;
    match (
        summary.pace_percent,
        summary.aggregate_used_percent,
        summary.aggregate_elapsed_percent,
    ) {
        (Some(_), Some(used), Some(elapsed)) => {
            let areas = usage_bar_areas(gauge_area);
            let marker_offset = percent_marker_offset(elapsed, areas.bar.width);
            let remaining = (100.0 - used).max(0.0);
            render_usage_bar_row(
                f,
                areas,
                Line::from(Span::styled("7d", base().fg(C_WHITE))),
                usage_meter_line(
                    used,
                    marker_offset,
                    areas.bar.width,
                    base().fg(pace_color),
                    base().fg(remaining_color(remaining)),
                    base().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Line::from(Span::styled(
                    format!("  {used:>3.0}% used  {remaining:>3.0}% left"),
                    base().fg(pace_color).add_modifier(Modifier::BOLD),
                )),
            );
            global_marker = Some((areas.bar, marker_offset));
        }
        _ => f.render_widget(
            Paragraph::new("No valid current weekly quota data").style(base().fg(DIM)),
            gauge_area,
        ),
    }

    if content.height > 1 {
        let marker_row = Rect {
            y: content.y.saturating_add(1),
            height: 1,
            ..content
        };
        if let Some((bar, marker_offset)) = global_marker {
            render_pace_label(f, marker_row, bar, marker_offset);
        }
    }

    let (summary_prefix, summary_segments) =
        match (summary.pace_percent, summary.reserve_percent_points) {
            (Some(pace), Some(reserve)) => (
                format!("Pace {pace:.1}%"),
                vec![
                    format_global_pace_delta(reserve),
                    format!(
                        "Eff {}/{}",
                        format_capacity(summary.effective_capacity),
                        format_capacity(summary.normal_capacity)
                    ),
                    format!(
                        "{} included / {} unavailable",
                        summary.included_accounts, summary.excluded_accounts
                    ),
                ],
            ),
            _ => (
                format!("{} included", summary.included_accounts),
                vec![format!("{} unavailable", summary.excluded_accounts)],
            ),
        };
    let mut summary_segments = summary_segments;
    if let Some(next) = next_reset_text(summary, now) {
        summary_segments.push(format!("Next {next}"));
    }
    let summary_text = format!(
        "{}{}",
        summary_prefix,
        fitted_segment_suffix(
            display_width(&summary_prefix),
            content.width.into(),
            summary_segments
        )
    );

    if content.height > 2 {
        f.render_widget(
            Paragraph::new(summary_text).style(base().fg(C_GRAY)),
            Rect {
                y: content.y.saturating_add(2),
                height: 1,
                ..content
            },
        );
    }
}

fn compact_global_weekly_line(
    summary: &GlobalWeeklySummary,
    now: i64,
    width: usize,
) -> Line<'static> {
    let prefix = if width >= 28 {
        " Global Weekly: "
    } else {
        " Global: "
    };
    let (Some(pace), Some(reserve)) = (summary.pace_percent, summary.reserve_percent_points) else {
        let unavailable = "unavailable";
        let suffix = fitted_segment_suffix(
            display_width(prefix) + display_width(unavailable),
            width,
            [format!(
                "{}/{} accounts",
                summary.included_accounts, summary.excluded_accounts
            )],
        );
        return Line::from(vec![
            Span::styled(prefix, base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{unavailable}{suffix}"), base().fg(DIM)),
        ]);
    };
    let pace_color = global_pace_color(Some(pace));
    let pace_text = format!("{pace:.1}%");
    let mut segments = vec![format_pace_delta_value(reserve)];
    if let Some(next) = next_reset_text(summary, now) {
        segments.push(format!("reset {next}"));
    }
    segments.push(format!(
        "{}/{} accounts",
        summary.included_accounts, summary.excluded_accounts
    ));
    segments.push(summary.weighting.as_str().to_string());
    let suffix = fitted_segment_suffix(
        display_width(prefix) + display_width(&pace_text),
        width,
        segments,
    );

    Line::from(vec![
        Span::styled(prefix, base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(
            pace_text,
            base().fg(pace_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, base().fg(pace_color)),
    ])
}

fn next_reset_text(summary: &GlobalWeeklySummary, now: i64) -> Option<String> {
    let alias = summary.next_reset_alias.as_deref()?;
    let resets_at = summary.next_reset_at?;
    (resets_at > now).then(|| {
        format!(
            "{alias} in {}",
            format_duration_compact((resets_at - now) as u64)
        )
    })
}

fn format_duration_compact(seconds: u64) -> String {
    if seconds < 60 {
        "<1m".to_string()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h{}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d{}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

fn format_global_pace_delta(reserve: f64) -> String {
    let reserve = if reserve.abs() < 0.05 { 0.0 } else { reserve };
    if reserve > 0.0 {
        format!("{} reserve", format_pace_delta_value(reserve))
    } else if reserve < 0.0 {
        format!("{} deficit", format_pace_delta_value(reserve))
    } else {
        "0.0%p normal".to_string()
    }
}

fn format_pace_delta_value(reserve: f64) -> String {
    let reserve = if reserve.abs() < 0.05 { 0.0 } else { reserve };
    if reserve == 0.0 {
        "0.0%p".to_string()
    } else {
        format!("{reserve:+.1}%p")
    }
}

fn format_capacity(capacity: f64) -> String {
    if (capacity - capacity.round()).abs() < 0.05 {
        format!("{capacity:.0}")
    } else {
        format!("{capacity:.1}")
    }
}

fn render_help_popup(f: &mut Frame, state: &mut popup::PopupState, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let key_style = Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(C_WHITE);
    let heading_style = Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(DIM);

    // Compute key column width for alignment within section
    let groups = keymap::help_sections();
    let key_col = groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(8);

    for (i, (heading, items)) in groups.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            (*heading).to_string(),
            heading_style,
        )));
        for (k, label) in items {
            let pad = key_col.saturating_sub(display_width(k));
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::raw("  "));
            spans.push(Span::styled((*k).to_string(), key_style));
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            spans.push(Span::raw("  "));
            spans.push(Span::styled((*label).to_string(), label_style));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  esc / q / h to close \u{2022} j k arrows / PgUp PgDn to scroll",
        dim_style,
    )));

    popup::render_popup(f, "Help", &lines, state, area);
}

fn display_width(s: &str) -> usize {
    Line::from(s).width()
}

#[derive(Debug, PartialEq, Eq)]
struct TableTextWidths {
    alias: u16,
    email: u16,
    plan: u16,
}

fn table_text_widths(
    total_width: u16,
    aliases: &[&str],
    emails: &[&str],
    plans: &[&str],
) -> TableTextWidths {
    let desired = |header: &str, values: &[&str]| {
        values
            .iter()
            .map(|value| u16::try_from(display_width(value)).unwrap_or(u16::MAX))
            .chain(std::iter::once(
                u16::try_from(display_width(header)).unwrap_or(u16::MAX),
            ))
            .max()
            .unwrap_or(0)
    };
    let mut widths = TableTextWidths {
        alias: desired("Alias", aliases).max(5),
        email: desired("Email", emails).max(5),
        plan: desired("Plan", plans).max(4),
    };

    // Borders, column spacing, marker and fixed quota columns consume 64 cells.
    let budget = total_width.saturating_sub(64).max(14);
    let total = u32::from(widths.alias) + u32::from(widths.email) + u32::from(widths.plan);
    let mut excess = total.saturating_sub(u32::from(budget));
    for (width, minimum) in [
        (&mut widths.email, 5_u16),
        (&mut widths.plan, 4_u16),
        (&mut widths.alias, 5_u16),
    ] {
        let shrink = excess.min(u32::from(width.saturating_sub(minimum)));
        *width -= shrink as u16;
        excess -= shrink;
    }
    widths
}

fn render_account_table(f: &mut Frame, app: &App, area: Rect) {
    if app.accounts.is_empty() {
        let block = Block::default()
            .title(" codex-switch-global-pace ")
            .borders(Borders::ALL)
            .border_style(base().fg(C_BLUE))
            .style(base());
        let hint = Paragraph::new(Line::from(vec![
            Span::styled("No accounts yet. Press ", Style::default().fg(DIM)),
            Span::styled(
                "a",
                Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to add one, or ", Style::default().fg(DIM)),
            Span::styled(
                "q",
                Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to quit.", Style::default().fg(DIM)),
        ]))
        .block(block)
        .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(hint, area);
        return;
    }

    let hdr = base().fg(C_CYAN).add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from(" ").style(base().fg(DIM)),
        Cell::from("Alias").style(hdr),
        Cell::from("Email").style(hdr),
        Cell::from("Plan").style(hdr),
        Cell::from("Status").style(hdr),
        Cell::from("5h").style(hdr),
        Cell::from("7d").style(hdr),
        Cell::from("5h Reset").style(hdr),
        Cell::from("7d Reset").style(hdr),
        Cell::from("Cards").style(hdr),
    ])
    .height(1);

    let mut rows: Vec<Row> = Vec::new();
    let mut render_selected: usize = 0;
    for (view_i, &acc_i) in app.view_indices.iter().enumerate() {
        let entry = &app.accounts[acc_i];
        let main_row = {
            let is_marked = app.marked.contains(&entry.alias);
            let marker = if is_marked {
                ">"
            } else if entry.is_current {
                "*"
            } else {
                " "
            };
            let marker_style = if is_marked {
                base().fg(C_YELLOW).add_modifier(Modifier::BOLD)
            } else if entry.is_current {
                base().fg(C_GREEN).add_modifier(Modifier::BOLD)
            } else {
                base()
            };

            let is_selected = view_i == app.selected;
            let row_style = if is_selected {
                base().fg(C_WHITE).add_modifier(Modifier::BOLD)
            } else {
                base().fg(C_GRAY)
            };

            let email = entry.info.email.as_deref().unwrap_or("--").to_string();
            let api_plan = if let UsageStatus::Loaded(u) = &entry.usage {
                u.plan_type.as_deref()
            } else {
                None
            };
            let effective_plan = api_plan.or(entry.info.plan_type.as_deref());
            let plan_label = entry.info.plan_label_with(effective_plan);
            let plan_style = plan_color(effective_plan, is_selected);

            let now = crate::auth::now_unix_secs();

            let (
                status_text,
                status_color,
                pct_5h,
                pct_7d,
                reset_5h,
                reset_5h_color,
                reset_7d,
                reset_7d_color,
                reset_cards,
                reset_cards_color,
            ): (
                String,
                Color,
                String,
                String,
                String,
                Color,
                String,
                Color,
                String,
                Color,
            ) = match &entry.usage {
                UsageStatus::Idle => (
                    "--".into(),
                    DIM,
                    "--".into(),
                    "--".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                ),
                UsageStatus::Loading => (
                    "...".into(),
                    C_YELLOW,
                    "...".into(),
                    "...".into(),
                    "loading".into(),
                    DIM,
                    "loading".into(),
                    DIM,
                    "...".into(),
                    C_YELLOW,
                ),
                UsageStatus::Error(_) => (
                    "Error".into(),
                    C_RED,
                    "Err".into(),
                    "Err".into(),
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                    "Err".into(),
                    C_RED,
                ),
                UsageStatus::Loaded(u) => {
                    let refreshing = app.is_refreshing(&entry.alias);
                    let over_5h = u.primary.as_ref().is_some_and(|w| {
                        let used = w.used_percent.unwrap_or(0.0);
                        // Suppress pace warning when usage is negligible — a fresh window
                        // always shows used > pace near t=0, which is noise not a real warning.
                        used >= 10.0
                            && crate::usage::visible_pace_percent(w, crate::usage::WINDOW_5H_SECS)
                                .is_some_and(|pace| used > pace)
                    });
                    let over_7d = u.secondary.as_ref().is_some_and(|w| {
                        let used = w.used_percent.unwrap_or(0.0);
                        used >= 10.0
                            && crate::usage::visible_pace_percent(w, crate::usage::WINDOW_7D_SECS)
                                .is_some_and(|pace| used > pace)
                    });
                    let p5 = u
                        .primary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| {
                            let s = format!("{:.0}%", (100.0 - p).max(0.0));
                            if over_5h { format!("{s}!") } else { s }
                        })
                        .unwrap_or_else(|| "--".into());
                    let p7 = u
                        .secondary
                        .as_ref()
                        .and_then(|w| w.used_percent)
                        .map(|p| {
                            let s = format!("{:.0}%", (100.0 - p).max(0.0));
                            if over_7d { format!("{s}!") } else { s }
                        })
                        .unwrap_or_else(|| "--".into());
                    let r5_ts = u.primary.as_ref().and_then(|w| w.resets_at);
                    let r5 = r5_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r5c = r5_ts.map(|ts| reset_color(ts - now)).unwrap_or(DIM);
                    let r7_ts = u.secondary.as_ref().and_then(|w| w.resets_at);
                    let r7 = r7_ts.map(format_reset_short).unwrap_or_else(|| "--".into());
                    let r7c = r7_ts.map(|ts| reset_color(ts - now)).unwrap_or(DIM);
                    let cards = reset_cards_table_text(u);
                    let cards_color = reset_cards_color(u);
                    if refreshing {
                        (
                            "Refresh".into(),
                            C_YELLOW,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    } else if is_available(u) {
                        (
                            "OK".into(),
                            C_GREEN,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    } else {
                        (
                            "Limited".into(),
                            C_RED,
                            p5,
                            p7,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    }
                }
            };

            Row::new(vec![
                Cell::from(Span::styled(marker, marker_style)),
                Cell::from(entry.alias.clone()).style(row_style),
                Cell::from(email).style(row_style),
                Cell::from(plan_label).style(plan_style),
                Cell::from(status_text).style(base().fg(status_color).add_modifier(
                    if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                )),
                Cell::from(pct_5h.clone()).style(usage_pct_style(&pct_5h, is_selected)),
                Cell::from(pct_7d.clone()).style(usage_pct_style(&pct_7d, is_selected)),
                Cell::from(reset_5h).style(base().fg(reset_5h_color)),
                Cell::from(reset_7d).style(base().fg(reset_7d_color)),
                Cell::from(reset_cards).style(base().fg(reset_cards_color)),
            ])
            .height(1)
        };

        if view_i == app.selected {
            render_selected = rows.len();
        }
        rows.push(main_row);
    }

    let loading_count = app.loading_count();
    let mut title = if let Some(s) = &app.search {
        format!(
            " Accounts ({}/{}) [/{s}]",
            app.view_indices.len(),
            app.accounts.len(),
            s = s.query
        )
    } else {
        format!(" Accounts ({})", app.accounts.len())
    };
    if loading_count > 0 {
        title.push_str(&format!(" -- fetching {}...", loading_count));
    }
    if !app.marked.is_empty() {
        title.push_str(&format!(" [{} marked]", app.marked.len()));
    }
    if let Some(secs) = app.auto_refresh_remaining_secs() {
        title.push_str(&format!(" auto:{}", format_auto_refresh_remaining(secs)));
        if app.auto_warmup_enabled {
            title.push_str("+warm");
        }
    }
    title.push_str(&format!(" sort:{} ", app.sort_mode.as_str()));

    let mut table_state = TableState::default().with_selected(render_selected);

    let aliases: Vec<&str> = app
        .view_indices
        .iter()
        .map(|&idx| app.accounts[idx].alias.as_str())
        .collect();
    let emails: Vec<&str> = app
        .view_indices
        .iter()
        .map(|&idx| app.accounts[idx].info.email.as_deref().unwrap_or("--"))
        .collect();
    let plan_labels: Vec<String> = app
        .view_indices
        .iter()
        .map(|&idx| {
            let entry = &app.accounts[idx];
            let api_plan = match &entry.usage {
                UsageStatus::Loaded(u) => u.plan_type.as_deref(),
                _ => None,
            };
            entry
                .info
                .plan_label_with(api_plan.or(entry.info.plan_type.as_deref()))
        })
        .collect();
    let plans: Vec<&str> = plan_labels.iter().map(String::as_str).collect();
    let text_widths = table_text_widths(area.width, &aliases, &emails, &plans);

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),                 // marker
            Constraint::Length(text_widths.alias), // alias
            Constraint::Length(text_widths.email), // email
            Constraint::Length(text_widths.plan),  // plan
            Constraint::Length(8),                 // status
            Constraint::Length(6),                 // 5h %
            Constraint::Length(6),                 // 7d %
            Constraint::Length(12),                // 5h reset
            Constraint::Length(12),                // 7d reset
            Constraint::Length(7),                 // reset cards
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(base().fg(C_BLUE))
            .style(base()),
    )
    .row_highlight_style(
        Style::default()
            .bg(C_HIGHLIGHT_BG)
            .add_modifier(Modifier::BOLD),
    )
    .style(base());

    f.render_stateful_widget(table, area, &mut table_state);
}

fn usage_gauges_height(usage: &UsageInfo) -> u16 {
    let multi_pool = !usage.additional_limits.is_empty();
    let mut height = 0u16;
    let mut pool_count = 0u16;
    let mut add_pool = |primary: bool, secondary: bool| {
        if pool_count > 0 {
            height = height.saturating_add(1);
        }
        if multi_pool {
            height = height.saturating_add(1);
        }
        height = height.saturating_add(u16::from(primary) * 2);
        height = height.saturating_add(u16::from(secondary) * 2);
        if !primary && !secondary {
            height = height.saturating_add(1);
        }
        pool_count = pool_count.saturating_add(1);
    };
    add_pool(usage.primary.is_some(), usage.secondary.is_some());
    for pool in &usage.additional_limits {
        add_pool(pool.primary.is_some(), pool.secondary.is_some());
    }
    height.max(1)
}

fn detail_panel_height(app: &App) -> u16 {
    let gauges = app
        .selected_account_idx()
        .and_then(|idx| app.accounts.get(idx))
        .and_then(|entry| match &entry.usage {
            UsageStatus::Loaded(usage) => Some(usage_gauges_height(usage)),
            _ => None,
        })
        .unwrap_or(4);
    gauges.saturating_add(2)
}

fn render_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    let entry = match app
        .selected_account_idx()
        .and_then(|idx| app.accounts.get(idx))
    {
        Some(e) => e,
        None => return,
    };

    let title = if entry.is_current {
        format!(" * {} (active) ", entry.alias)
    } else {
        format!(" {} ", entry.alias)
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(base().fg(if entry.is_current { C_GREEN } else { C_BLUE }))
        .style(base());

    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1)])
        .horizontal_margin(1)
        .split(inner);

    // Usage area
    match &entry.usage {
        UsageStatus::Idle => {
            let p = Paragraph::new("Press r to refresh usage").style(base().fg(DIM));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Loading => {
            let p = Paragraph::new("Fetching usage...").style(base().fg(C_YELLOW));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Error(e) => {
            let p = Paragraph::new(format!("Error: {}", e.detail)).style(base().fg(C_RED));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Loaded(u) => {
            render_usage_gauges(f, u, layout[0]);
        }
    }
}

pub(super) fn render_usage_gauges(f: &mut Frame, u: &UsageInfo, area: Rect) {
    let now = crate::auth::now_unix_secs();
    let multi_pool = !u.additional_limits.is_empty();
    let mut y = area.y;
    let mut render_pool = |f: &mut Frame,
                           name: &str,
                           primary: Option<&crate::usage::WindowUsage>,
                           secondary: Option<&crate::usage::WindowUsage>,
                           unavailable: bool| {
        if y > area.y {
            y = y.saturating_add(1);
        }
        if multi_pool && y < area.bottom() {
            let title = if unavailable {
                format!("{name}  unavailable")
            } else {
                name.to_string()
            };
            f.render_widget(
                Paragraph::new(title).style(base().fg(if unavailable { C_RED } else { C_CYAN })),
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
            );
            y = y.saturating_add(1);
        }
        if let Some(window) = primary
            && y < area.bottom()
        {
            let (label, window_secs) =
                quota_window_display(window, "5h", crate::usage::WINDOW_5H_SECS);
            render_usage_gauge(
                f,
                window,
                &label,
                window_secs,
                now,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 2,
                },
            );
            y = y.saturating_add(2);
        }
        if let Some(window) = secondary
            && y < area.bottom()
        {
            let (label, window_secs) =
                quota_window_display(window, "7d", crate::usage::WINDOW_7D_SECS);
            render_usage_gauge(
                f,
                window,
                &label,
                window_secs,
                now,
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 2,
                },
            );
            y = y.saturating_add(2);
        }
        if primary.is_none() && secondary.is_none() && y < area.bottom() {
            f.render_widget(
                Paragraph::new("No active window").style(base().fg(DIM)),
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
            );
            y = y.saturating_add(1);
        }
    };

    render_pool(f, "Main", u.primary.as_ref(), u.secondary.as_ref(), false);
    for pool in &u.additional_limits {
        render_pool(
            f,
            pool.limit_name.as_deref().unwrap_or("Additional"),
            pool.primary.as_ref(),
            pool.secondary.as_ref(),
            pool.allowed == Some(false) || pool.limit_reached == Some(true),
        );
    }
}

fn quota_window_display(
    window: &crate::usage::WindowUsage,
    fallback_label: &str,
    fallback_secs: i64,
) -> (String, i64) {
    match window.window_minutes {
        Some(minutes) if minutes % 1_440 == 0 => {
            (format!("{}d", minutes / 1_440), minutes.saturating_mul(60))
        }
        Some(minutes) if minutes % 60 == 0 => {
            (format!("{}h", minutes / 60), minutes.saturating_mul(60))
        }
        Some(minutes) => (format!("{minutes}m"), minutes.saturating_mul(60)),
        None => (fallback_label.to_string(), fallback_secs),
    }
}

fn reset_cards_table_text(u: &UsageInfo) -> String {
    reset_credits_count(u)
        .map(|count| count.to_string())
        .or_else(|| u.reset_credits_error.as_ref().map(|_| "err".to_string()))
        .unwrap_or_else(|| "--".to_string())
}

fn reset_cards_color(u: &UsageInfo) -> Color {
    match reset_credits_count(u) {
        Some(0) => DIM,
        Some(_) => C_GREEN,
        None if u.reset_credits_error.is_some() => C_YELLOW,
        None => DIM,
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Rename input takes top priority
    if let Some(rs) = &app.rename {
        let line = Line::from(vec![
            Span::styled(" Rename: ", base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(&rs.input, base().fg(C_WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("#", base().fg(C_GRAY)),
            Span::styled("  (Enter confirm / Esc cancel)", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    // Confirmation prompt
    if let Some(confirm) = &app.confirm {
        let msg = match confirm {
            super::app::ConfirmAction::Delete(alias) => {
                format!("Delete profile '{alias}'? (y/n)")
            }
            super::app::ConfirmAction::BatchDelete(aliases) => {
                format!("Delete {} marked profile(s)? (y/n)", aliases.len())
            }
            super::app::ConfirmAction::ConsumeResetCard { alias, expires_at } => {
                format!(
                    "Confirm reset card for '{alias}' expiring {expires_at}: y to use, any other key cancels"
                )
            }
        };
        let line = Line::from(Span::styled(
            msg,
            base().fg(C_RED).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if app.search_active
        && let Some(s) = &app.search
    {
        let line = Line::from(vec![
            Span::styled(" /", base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(&s.query, base().fg(C_WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("#", base().fg(C_GRAY)),
            Span::styled("  (Enter accept / Esc clear)", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
        return;
    }

    if let Some(s) = &app.status_msg {
        let msg = Line::from(Span::styled(
            s.as_str(),
            base().fg(status_message_color(app.status_is_error)),
        ));
        f.render_widget(Paragraph::new(msg).style(base()), area);
    } else if !app.marked.is_empty() {
        let line = Line::from(vec![
            Span::styled(" ", base()),
            Span::styled(
                format!("{}", app.marked.len()),
                base().fg(C_YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" selected", base().fg(C_YELLOW)),
            Span::styled(" \u{2014} ", base().fg(DIM)),
            Span::styled("enter", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(" for batch \u{2502} ", base().fg(DIM)),
            Span::styled("esc", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(" to clear", base().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(base()), area);
    } else {
        let lines = build_help_lines(area.width as usize);
        f.render_widget(Paragraph::new(lines).style(base()), area);
    }

    // Version indicator — always rendered at bottom-right corner
    let version = crate::update::current_version();
    let ver_spans: Vec<Span> = if let Some(latest) = &app.update_available {
        vec![
            Span::styled(" \u{2502} ", base().fg(DIM)),
            Span::styled(format!("v{version}"), base().fg(DIM)),
            Span::styled(format!(" -> v{latest} "), base().fg(C_YELLOW)),
        ]
    } else {
        vec![
            Span::styled(" \u{2502} ", base().fg(DIM)),
            Span::styled(format!("v{version} "), base().fg(DIM)),
        ]
    };
    let ver_width: usize = ver_spans.iter().map(|s| s.width()).sum();
    if (area.width as usize) > ver_width {
        let ver_area = Rect {
            x: area.x + area.width - ver_width as u16,
            y: area.y + area.height.saturating_sub(1),
            width: ver_width as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(ver_spans)).style(base()),
            ver_area,
        );
    }
}

/// Render a single usage gauge (5h or 7d) with block chars and pace marker.
fn render_usage_gauge(
    f: &mut Frame,
    w: &crate::usage::WindowUsage,
    label: &str,
    window_secs: i64,
    now: i64,
    area: Rect,
) {
    let used = w.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let remaining_pct = (100.0 - used).max(0.0);
    let pace = crate::usage::visible_pace_percent(w, window_secs);
    let over = used >= 10.0 && pace.is_some_and(|p| used > p);
    let reset_str = w
        .resets_at
        .map(format_reset_time)
        .unwrap_or_else(|| "--".into());
    let remaining_secs = w.resets_at.map(|ts| ts - now).unwrap_or(0);

    // Row 1: fixed label and metrics columns around the shared-width meter.
    let gauge_area = Rect { height: 1, ..area };
    let areas = usage_bar_areas(gauge_area);
    let bar_width = areas.bar.width;

    let used_color = if used >= 90.0 {
        C_RED
    } else if over || used >= 70.0 {
        C_YELLOW
    } else {
        C_GREEN
    };
    let used_style = base().fg(used_color);
    let remaining_style = base().fg(remaining_color(remaining_pct));
    let pace_style = base().fg(C_WHITE).add_modifier(Modifier::BOLD);

    let pace_pos = pace.and_then(|value| percent_marker_offset(value, bar_width));
    let bar_line = usage_meter_line(
        used,
        pace_pos,
        bar_width,
        used_style,
        remaining_style,
        pace_style,
    );

    let suffix_color = if over { C_YELLOW } else { DIM };
    render_usage_bar_row(
        f,
        areas,
        Line::from(Span::styled(label.to_string(), base().fg(C_WHITE))),
        bar_line,
        Line::from(Span::styled(
            format!("  {used:>3.0}% used  {remaining_pct:>3.0}% left"),
            base().fg(suffix_color),
        )),
    );

    // Row 2: "started HH:MM" left, "↑ pace" at pace position, "resets in ..." right
    let reset_area = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let reset_text = format!("resets in {reset_str}");
    let reset_style = base().fg(reset_color(remaining_secs));
    let started_text = w
        .resets_at
        .map(|ts| format!("started {}", format_local_time(ts - window_secs)))
        .unwrap_or_default();
    let reset_text_width = u16::try_from(display_width(&reset_text)).unwrap_or(u16::MAX);
    let reset_width = if reset_text_width <= reset_area.width {
        reset_text_width
    } else {
        0
    };
    let reset_rect = Rect {
        x: reset_area.right().saturating_sub(reset_width),
        width: reset_width,
        ..reset_area
    };
    let pace_right = reset_rect.x.saturating_sub(2).max(reset_area.x);
    let marker_x = pace_pos.map(|offset| areas.bar.x.saturating_add(offset));
    let started_right = marker_x
        .map(|x| x.saturating_sub(2))
        .unwrap_or(pace_right)
        .min(pace_right)
        .max(reset_area.x);
    let started_rect = Rect {
        width: started_right.saturating_sub(reset_area.x),
        ..reset_area
    };
    let pace_bounds = Rect {
        width: pace_right.saturating_sub(reset_area.x),
        ..reset_area
    };

    if display_width(&started_text) <= usize::from(started_rect.width) {
        f.render_widget(
            Paragraph::new(started_text).style(base().fg(DIM)),
            started_rect,
        );
    }
    render_pace_label(f, pace_bounds, areas.bar, pace_pos);
    if reset_width > 0 {
        f.render_widget(Paragraph::new(reset_text).style(reset_style), reset_rect);
    }
}

// ── Style helpers ─────────────────────────────────────────

/// Color for remaining percentage: green > 30%, yellow > 10%, red <= 10%
fn remaining_color(remaining_pct: f64) -> Color {
    if remaining_pct > 30.0 {
        C_GREEN
    } else if remaining_pct > 10.0 {
        C_YELLOW
    } else {
        C_RED
    }
}

fn plan_color(plan: Option<&str>, is_selected: bool) -> Style {
    let kind = PlanKind::from_wire(plan);
    let fg = match kind {
        PlanKind::Free | PlanKind::Unknown => C_GRAY,
        PlanKind::Go => C_BLUE,
        PlanKind::Plus => C_CYAN,
        PlanKind::ProLite | PlanKind::Pro => C_YELLOW,
        PlanKind::Team | PlanKind::Business | PlanKind::Enterprise | PlanKind::Edu => C_MAGENTA,
    };
    let s = base().fg(fg);
    if is_selected || matches!(kind, PlanKind::Pro | PlanKind::Enterprise) {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Color for reset countdown: green = soon (< 1h), yellow = medium (< 4h), red = far (>= 4h)
fn reset_color(remaining_secs: i64) -> Color {
    if remaining_secs < 3600 {
        C_GREEN
    } else if remaining_secs < 14400 {
        C_YELLOW
    } else {
        C_RED
    }
}

fn usage_pct_style(remaining_pct_str: &str, is_selected: bool) -> Style {
    let over_pace = remaining_pct_str.ends_with('!');
    let clean = remaining_pct_str.trim_end_matches('!');
    let fg = if over_pace {
        C_RED
    } else {
        match clean.trim_end_matches('%').parse::<f64>() {
            Ok(n) => remaining_color(n),
            Err(_) => DIM,
        }
    };
    let s = base().fg(fg);
    if is_selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

fn build_help_lines(width: usize) -> Vec<Line<'static>> {
    let key_style = base().fg(C_YELLOW);
    let sep_style = base().fg(DIM);
    let label_style = base().fg(C_GRAY);
    let space_style = base();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ", space_style)];
    let mut used = 1usize;

    let items = keymap::status_bar_items();
    for (i, (k, label)) in items.iter().enumerate() {
        let key_disp = (*k).to_string();
        let label_short = short_label(label);
        let sep = " \u{2502} ";
        let item_len = key_disp.chars().count()
            + 1
            + label_short.chars().count()
            + if i + 1 < items.len() {
                sep.chars().count()
            } else {
                0
            };
        if used + item_len > width && used > 1 {
            lines.push(Line::from(spans));
            spans = vec![Span::styled(" ", space_style)];
            used = 1;
        }
        spans.push(Span::styled(key_disp, key_style));
        spans.push(Span::styled(" ", space_style));
        spans.push(Span::styled(label_short.to_string(), label_style));
        if i + 1 < items.len() {
            spans.push(Span::styled(sep, sep_style));
        }
        used += item_len;
    }
    if spans.len() > 1 {
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("", space_style)));
    }
    lines
}

/// Compress verbose keymap labels for status bar.
fn short_label(label: &str) -> &str {
    match label {
        "move selection" => "nav",
        "search" => "search",
        "open menu (account or batch)" => "menu",
        "refresh visible accounts" => "refresh",
        "show / hide account detail panel" => "quota",
        "show this help" => "help",
        "quit" => "quit",
        other => other,
    }
}

fn format_auto_refresh_remaining(secs: u64) -> String {
    if secs == 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem = secs % 60;
    if rem == 0 {
        format!("{mins}m")
    } else {
        format!("{mins}m{rem}s")
    }
}

fn status_bar_height(app: &App, width: u16) -> usize {
    if app.status_msg.is_some()
        || app.rename.is_some()
        || app.confirm.is_some()
        || app.search_active
        || !app.marked.is_empty()
    {
        return 1;
    }
    build_help_lines(width as usize).len()
}

#[cfg(test)]
mod tests {
    use super::{
        C_BLUE, C_CYAN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, GLOBAL_WEEKLY_COMPACT_HEIGHT,
        GLOBAL_WEEKLY_FULL_HEIGHT, MIN_ACCOUNT_TABLE_HEIGHT, PACE_LABEL, fitted_segment_suffix,
        global_pace_color, global_weekly_panel_height, meter_fill_width, percent_marker_offset,
        plan_color, render, render_detail_panel, render_global_weekly_pace, render_usage_gauge,
        render_usage_gauges, status_message_color, table_text_widths, usage_gauges_height,
    };
    use crate::jwt::AccountInfo;
    use crate::tui::app::{AccountEntry, App, UsageStatus};
    use crate::usage::{
        AdditionalRateLimit, GlobalPaceWeighting, GlobalWeeklySummary, UsageInfo, WindowUsage,
    };
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use ratatui::{Terminal, backend::TestBackend};

    type MeterBounds = Option<(u16, u16)>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct DashboardGeometry {
        global_meter: MeterBounds,
        account_meter: MeterBounds,
        global_marker: Option<u16>,
        account_marker: Option<u16>,
        global_pace_label: Option<u16>,
        account_pace_label: Option<u16>,
    }

    fn row_text(backend: &TestBackend, y: u16) -> String {
        let area = backend.buffer().area;
        (0..area.width)
            .map(|x| {
                backend
                    .buffer()
                    .cell((x, y))
                    .expect("cell inside test buffer")
                    .symbol()
            })
            .collect()
    }

    fn meter_bounds(backend: &TestBackend, y: u16) -> MeterBounds {
        let width = backend.buffer().area.width;
        let mut best = None;
        let mut x = 0;

        while x < width {
            let symbol = backend
                .buffer()
                .cell((x, y))
                .expect("cell inside test buffer")
                .symbol();
            if !matches!(symbol, "█" | "░" | "|" | "│") {
                x += 1;
                continue;
            }

            let start = x;
            let mut has_fill_cell = false;
            while x < width {
                let symbol = backend
                    .buffer()
                    .cell((x, y))
                    .expect("cell inside test buffer")
                    .symbol();
                if !matches!(symbol, "█" | "░" | "|" | "│") {
                    break;
                }
                has_fill_cell |= matches!(symbol, "█" | "░");
                x += 1;
            }

            if has_fill_cell
                && best.is_none_or(|(best_start, best_end)| x - start > best_end - best_start)
            {
                best = Some((start, x));
            }
        }

        best
    }

    fn symbol_x(backend: &TestBackend, y: u16, symbol: &str) -> Option<u16> {
        (0..backend.buffer().area.width).find(|x| {
            backend
                .buffer()
                .cell((*x, y))
                .expect("cell inside test buffer")
                .symbol()
                == symbol
        })
    }

    fn assert_text_at(backend: &TestBackend, x: u16, y: u16, expected: &str) {
        for (offset, expected_symbol) in expected.chars().enumerate() {
            let actual = backend
                .buffer()
                .cell((x + u16::try_from(offset).unwrap(), y))
                .expect("cell inside test buffer")
                .symbol();
            assert_eq!(actual, expected_symbol.to_string());
        }
    }

    #[test]
    fn status_message_color_distinguishes_errors_from_information() {
        assert_eq!(status_message_color(false), C_CYAN);
        assert_eq!(status_message_color(true), C_RED);
    }

    fn global_summary(now: i64) -> GlobalWeeklySummary {
        GlobalWeeklySummary {
            pace_percent: Some(106.6666666667),
            reserve_percent_points: Some(6.6666666667),
            aggregate_used_percent: Some(6.6666666667),
            aggregate_elapsed_percent: Some(13.3333333333),
            effective_capacity: 320.0,
            normal_capacity: 300.0,
            included_accounts: 3,
            excluded_accounts: 1,
            weighting: GlobalPaceWeighting::Equal,
            next_reset_at: Some(now + 3 * 3_600 + 18 * 60),
            next_reset_alias: Some("work2".to_string()),
        }
    }

    fn dashboard_geometry(width: u16, is_current: bool) -> DashboardGeometry {
        let now = crate::auth::now_unix_secs();
        let usage = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(35.0),
                resets_at: Some(now + crate::usage::WINDOW_5H_SECS / 2),
                window_minutes: Some(crate::usage::WINDOW_5H_SECS / 60),
            }),
            ..UsageInfo::default()
        };
        let mut app = App::new();
        app.accounts = vec![AccountEntry {
            alias: "work".to_string(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(usage)),
            is_current,
        }];
        app.update_view();

        let backend = TestBackend::new(width, 11);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut summary = global_summary(now);
        summary.pace_percent = Some(115.0);
        summary.reserve_percent_points = Some(15.0);
        summary.aggregate_used_percent = Some(35.0);
        summary.aggregate_elapsed_percent = Some(50.0);
        summary.effective_capacity = 345.0;
        summary.normal_capacity = 300.0;
        terminal
            .draw(|frame| {
                render_global_weekly_pace(
                    frame,
                    &summary,
                    now,
                    Rect::new(0, 0, width, GLOBAL_WEEKLY_FULL_HEIGHT),
                );
                render_detail_panel(
                    frame,
                    &app,
                    Rect::new(0, GLOBAL_WEEKLY_FULL_HEIGHT, width, 6),
                );
            })
            .unwrap();

        let account_meter_y = GLOBAL_WEEKLY_FULL_HEIGHT + 1;
        DashboardGeometry {
            global_meter: meter_bounds(terminal.backend(), 1),
            account_meter: meter_bounds(terminal.backend(), account_meter_y),
            global_marker: symbol_x(terminal.backend(), 1, "|"),
            account_marker: symbol_x(terminal.backend(), account_meter_y, "|"),
            global_pace_label: symbol_x(terminal.backend(), 2, "↑"),
            account_pace_label: symbol_x(terminal.backend(), account_meter_y + 1, "↑"),
        }
    }

    fn dashboard_meter_bounds(width: u16, is_current: bool) -> (MeterBounds, MeterBounds) {
        let geometry = dashboard_geometry(width, is_current);
        (geometry.global_meter, geometry.account_meter)
    }

    #[test]
    fn global_pace_color_uses_normal_baseline_thresholds() {
        assert_eq!(global_pace_color(Some(100.0)), C_GREEN);
        assert_eq!(global_pace_color(Some(99.99)), C_YELLOW);
        assert_eq!(global_pace_color(Some(90.0)), C_YELLOW);
        assert_eq!(global_pace_color(Some(89.99)), C_RED);
    }

    #[test]
    fn percentage_markers_use_the_full_zero_to_hundred_axis() {
        assert_eq!(percent_marker_offset(50.0, 0), None);
        assert_eq!(percent_marker_offset(f64::NAN, 5), None);
        assert_eq!(percent_marker_offset(0.0, 1), Some(0));
        assert_eq!(percent_marker_offset(100.0, 1), Some(0));
        assert_eq!(percent_marker_offset(-10.0, 5), Some(0));
        assert_eq!(percent_marker_offset(0.0, 5), Some(0));
        assert_eq!(percent_marker_offset(50.0, 5), Some(2));
        assert_eq!(percent_marker_offset(100.0, 5), Some(4));
        assert_eq!(percent_marker_offset(125.0, 5), Some(4));
    }

    #[test]
    fn nonzero_meter_fill_remains_visible_and_stays_within_width() {
        assert_eq!(meter_fill_width(0.0, 5), 0);
        assert_eq!(meter_fill_width(0.01, 5), 1);
        assert_eq!(meter_fill_width(50.0, 5), 3);
        assert_eq!(meter_fill_width(99.99, 5), 5);
        assert_eq!(meter_fill_width(100.0, 5), 5);
        assert_eq!(meter_fill_width(125.0, 5), 5);
    }

    #[test]
    fn segmented_text_keeps_only_the_complete_priority_prefix() {
        assert_eq!(fitted_segment_suffix(5, 10, ["ab".to_string()]), " · ab");
        assert_eq!(fitted_segment_suffix(5, 9, ["ab".to_string()]), "");
        assert_eq!(fitted_segment_suffix(0, 5, ["한".to_string()]), " · 한");
        assert_eq!(
            fitted_segment_suffix(0, 6, ["too long".to_string(), "x".to_string()]),
            ""
        );
        assert_eq!(
            fitted_segment_suffix(usize::MAX, usize::MAX, ["x".to_string()]),
            ""
        );
    }

    #[test]
    fn global_panel_collapses_before_account_table_minimum_is_consumed() {
        let status_height = 1;
        let full_height = status_height + MIN_ACCOUNT_TABLE_HEIGHT + GLOBAL_WEEKLY_FULL_HEIGHT;
        assert_eq!(
            global_weekly_panel_height(full_height, status_height),
            GLOBAL_WEEKLY_FULL_HEIGHT
        );
        assert_eq!(
            global_weekly_panel_height(full_height - 1, status_height),
            GLOBAL_WEEKLY_COMPACT_HEIGHT
        );
        assert_eq!(
            global_weekly_panel_height(status_height + 1, status_height),
            0
        );
        assert_eq!(
            global_weekly_panel_height(status_height + 2, status_height),
            GLOBAL_WEEKLY_COMPACT_HEIGHT
        );
    }

    #[test]
    fn full_global_panel_renders_core_capacity_counts_and_reset() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let backend = TestBackend::new(110, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
            .unwrap();

        let rendered = (0..GLOBAL_WEEKLY_FULL_HEIGHT)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Global Weekly Pace · 100% normal · equal weight"));
        assert!(rendered.contains("7% used"));
        assert!(rendered.contains("93% left"));
        assert!(rendered.contains("↑ pace"));
        assert!(rendered.contains("106.7%"));
        assert!(rendered.contains("+6.7%p reserve"));
        let (meter_start, meter_end) = meter_bounds(terminal.backend(), 1).expect("global meter");
        assert!((meter_start..meter_end).all(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, 1))
                .expect("meter cell")
                .symbol()
                != "│"
        }));
        assert!(rendered.contains("Eff 320/300"));
        assert!(rendered.contains("3 included"));
        assert!(rendered.contains("1 unavailable"));
        assert!(rendered.contains("Next work2 in 3h18m"));
    }

    #[test]
    fn global_meter_uses_aggregate_usage_and_elapsed_pace() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.pace_percent = Some(97.73);
        summary.reserve_percent_points = Some(-2.27);
        summary.aggregate_used_percent = Some(3.0);
        summary.aggregate_elapsed_percent = Some(0.73);
        summary.effective_capacity = 293.2;

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
            .unwrap();

        let (meter_start, meter_end) = meter_bounds(terminal.backend(), 1).expect("global meter");
        let marker_x = symbol_x(terminal.backend(), 1, "|").expect("meter pace marker");
        let expected_offset = percent_marker_offset(0.73, meter_end - meter_start).unwrap();
        assert_eq!(marker_x, meter_start + expected_offset);
        assert_eq!(symbol_x(terminal.backend(), 2, "↑"), Some(marker_x));
        assert_text_at(terminal.backend(), marker_x, 2, PACE_LABEL);

        let meter_symbols = (meter_start..meter_end)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 1))
                    .expect("meter cell")
                    .symbol()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            meter_symbols
                .iter()
                .filter(|symbol| **symbol == "█")
                .count(),
            1
        );
        assert_eq!(
            meter_symbols
                .iter()
                .filter(|symbol| **symbol == "░")
                .count(),
            meter_symbols.len() - 2
        );

        let rendered = (0..GLOBAL_WEEKLY_FULL_HEIGHT)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("3% used"));
        assert!(rendered.contains("97% left"));
        assert!(rendered.contains("Pace 97.7%"));
        assert!(rendered.contains("-2.3%p deficit"));
        assert!(!rendered.contains("· N"));
    }

    #[test]
    fn global_pace_label_is_complete_at_every_meter_boundary() {
        let now = 1_000_000;
        for width in [40, 80, 110] {
            for elapsed in [0.0, 50.0, 97.73, 100.0, 125.0] {
                let mut summary = global_summary(now);
                summary.aggregate_used_percent = Some(50.0);
                summary.aggregate_elapsed_percent = Some(elapsed);
                let backend = TestBackend::new(width, GLOBAL_WEEKLY_FULL_HEIGHT);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
                    .unwrap();

                let marker_x = symbol_x(terminal.backend(), 1, "|").expect("pace marker");
                assert_eq!(symbol_x(terminal.backend(), 2, "↑"), Some(marker_x));
                assert_text_at(terminal.backend(), marker_x, 2, PACE_LABEL);
            }
        }
    }

    #[test]
    fn no_data_global_panels_do_not_render_a_meter_or_marker() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.pace_percent = None;
        summary.reserve_percent_points = None;
        summary.aggregate_used_percent = None;
        summary.aggregate_elapsed_percent = None;
        summary.effective_capacity = 0.0;
        summary.normal_capacity = 0.0;
        summary.included_accounts = 0;
        summary.excluded_accounts = 2;
        summary.next_reset_at = None;
        summary.next_reset_alias = None;

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
            .unwrap();
        let full_text = (0..GLOBAL_WEEKLY_FULL_HEIGHT)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full_text.contains("No valid current weekly quota data"));
        assert!(full_text.contains("0 included · 2 unavailable"));
        assert!((1..79).all(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, 2))
                .expect("marker-row cell")
                .symbol()
                == " "
        }));
        for symbol in ["█", "░", "|", "↑"] {
            assert_eq!(symbol_x(terminal.backend(), 1, symbol), None);
            assert_eq!(symbol_x(terminal.backend(), 2, symbol), None);
        }

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_COMPACT_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
            .unwrap();
        let compact_text = row_text(terminal.backend(), 0);
        assert!(compact_text.contains("unavailable · 0/2 accounts"));
        for symbol in ["█", "░", "|", "↑"] {
            assert_eq!(symbol_x(terminal.backend(), 0, symbol), None);
        }
    }

    #[test]
    fn account_meter_bounds_do_not_depend_on_percentage_or_label_length() {
        for width in [40, 70, 110] {
            let mut expected = None;
            for (used, label) in [
                (0.0, "5h"),
                (8.0, "7d"),
                (9.0, "14d"),
                (10.0, "30m"),
                (89.0, "5h"),
                (90.0, "7d"),
                (99.0, "14d"),
                (100.0, "1000m"),
            ] {
                let backend = TestBackend::new(width, 2);
                let mut terminal = Terminal::new(backend).unwrap();
                let window = WindowUsage {
                    used_percent: Some(used),
                    resets_at: None,
                    window_minutes: None,
                };

                terminal
                    .draw(|frame| {
                        render_usage_gauge(
                            frame,
                            &window,
                            label,
                            crate::usage::WINDOW_7D_SECS,
                            1_000_000,
                            frame.area(),
                        )
                    })
                    .unwrap();

                let bounds = meter_bounds(terminal.backend(), 0);
                assert!(bounds.is_some(), "meter missing at width {width}");
                if let Some(expected) = expected {
                    assert_eq!(bounds, Some(expected), "used={used}, label={label}");
                } else {
                    expected = bounds;
                }
            }
        }
    }

    #[test]
    fn global_and_account_meters_share_the_same_columns() {
        for width in [32, 40, 70, 110] {
            for is_current in [false, true] {
                let (global, account) = dashboard_meter_bounds(width, is_current);
                assert_eq!(global, account, "width={width}, active={is_current}");
            }
        }
    }

    #[test]
    fn global_and_account_pace_markers_share_the_same_coordinate_rule() {
        for width in [40, 70, 110] {
            for is_current in [false, true] {
                let geometry = dashboard_geometry(width, is_current);
                assert_eq!(
                    geometry.global_marker, geometry.account_marker,
                    "bar marker: width={width}, active={is_current}"
                );
                assert_eq!(geometry.global_pace_label, geometry.global_marker);
                if width == 40 {
                    assert_eq!(geometry.account_pace_label, None);
                } else {
                    assert_eq!(geometry.account_pace_label, geometry.account_marker);
                }
            }
        }
    }

    #[test]
    fn narrow_layout_does_not_invent_an_alternate_meter() {
        for width in [20, 32] {
            assert_eq!(dashboard_meter_bounds(width, true), (None, None));
            let geometry = dashboard_geometry(width, true);
            assert_eq!(geometry.global_marker, None);
            assert_eq!(geometry.account_marker, None);
            assert_eq!(geometry.global_pace_label, None);
            assert_eq!(geometry.account_pace_label, None);
        }
    }

    #[test]
    fn compact_global_panel_keeps_pace_reserve_and_reset_on_one_line() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let backend = TestBackend::new(70, GLOBAL_WEEKLY_COMPACT_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, now, frame.area()))
            .unwrap();
        let text = row_text(terminal.backend(), 0);

        assert!(text.contains("Global Weekly: 106.7%"));
        assert!(text.contains("+6.7%p"));
        assert!(text.contains("reset work2 in 3h18m"));
        for symbol in ["█", "░", "|", "↑"] {
            assert_eq!(symbol_x(terminal.backend(), 0, symbol), None);
        }
    }

    #[test]
    fn additional_quota_pool_expands_the_main_detail_panel() {
        let window = WindowUsage {
            used_percent: Some(25.0),
            resets_at: Some(1_000_000),
            window_minutes: Some(300),
        };
        let usage = UsageInfo {
            primary: Some(window.clone()),
            secondary: Some(window.clone()),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-6-Codex-Burst".to_string()),
                metered_feature: Some("codex_futureburst".to_string()),
                primary: Some(window.clone()),
                secondary: Some(window),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert_eq!(usage_gauges_height(&usage), 11);
    }

    #[test]
    fn eighty_by_twenty_four_keeps_every_additional_quota_meter_visible() {
        let now = crate::auth::now_unix_secs();
        let main_weekly = WindowUsage {
            used_percent: Some(25.0),
            resets_at: Some(now + 6 * 24 * 60 * 60),
            window_minutes: Some(7 * 24 * 60),
        };
        let spark_five_hour = WindowUsage {
            used_percent: Some(25.0),
            resets_at: Some(now + 2 * 60 * 60),
            window_minutes: Some(5 * 60),
        };
        let spark_weekly = WindowUsage {
            used_percent: Some(25.0),
            resets_at: Some(now + 5 * 24 * 60 * 60),
            window_minutes: Some(7 * 24 * 60),
        };
        let usage = UsageInfo {
            secondary: Some(main_weekly),
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                primary: Some(spark_five_hour),
                secondary: Some(spark_weekly),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(usage_gauges_height(&usage), 9);

        let mut app = App::new();
        app.accounts = (1..=3)
            .map(|number| AccountEntry {
                alias: format!("work{number}"),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(usage.clone())),
                is_current: number == 1,
            })
            .collect();
        app.update_view();

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rows = (0..24)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>();
        let spark_title_y = rows
            .iter()
            .position(|row| row.contains("GPT-5.3-Codex-Spark"))
            .expect("Spark quota title");

        assert!(rows[spark_title_y + 1].contains("5h"));
        assert!(rows[spark_title_y + 1].contains('|'));
        assert!(rows[spark_title_y + 2].contains(PACE_LABEL));
        assert!(rows[spark_title_y + 3].contains("7d"));
        assert!(rows[spark_title_y + 3].contains('|'));
        assert!(rows[spark_title_y + 4].contains(PACE_LABEL));
        assert_eq!(
            meter_bounds(terminal.backend(), MIN_ACCOUNT_TABLE_HEIGHT + 1),
            meter_bounds(
                terminal.backend(),
                u16::try_from(spark_title_y + 3).unwrap()
            )
        );
    }

    #[test]
    fn additional_primary_slot_uses_its_real_seven_day_window_for_label_and_pace() {
        let usage = UsageInfo {
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                primary: Some(WindowUsage {
                    used_percent: Some(8.0),
                    resets_at: Some(crate::auth::now_unix_secs() + 6 * 24 * 60 * 60),
                    window_minutes: Some(7 * 24 * 60),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_usage_gauges(frame, &usage, frame.area()))
            .unwrap();

        let row = (0..10)
            .map(|y| row_text(terminal.backend(), y))
            .find(|line| line.contains("7d"))
            .expect("the real seven-day window label must be rendered");
        assert!(!row.contains("5h"));
        let label_x = row.find("7d").unwrap();
        let pace_x = row.find('|').expect("pace marker");
        assert!(
            pace_x > label_x + 6,
            "seven-day pace must not be clamped to the start of the bar: {row}"
        );
    }

    #[test]
    fn plan_color_uses_semantic_plan_families() {
        assert_eq!(plan_color(Some("go"), false).fg, Some(C_BLUE));
        assert_eq!(plan_color(Some("plus"), false).fg, Some(C_CYAN));
        assert_eq!(plan_color(Some("prolite"), false).fg, Some(C_YELLOW));
        let pro = plan_color(Some("pro"), false);
        assert_eq!(pro.fg, Some(C_YELLOW));
        assert!(pro.add_modifier.contains(Modifier::BOLD));
        assert_eq!(plan_color(Some("team"), false).fg, Some(C_MAGENTA));
        assert_eq!(plan_color(Some("business"), false).fg, Some(C_MAGENTA));
        assert_eq!(plan_color(Some("future_plan"), false).fg, Some(C_GRAY));
    }

    #[test]
    fn account_table_columns_expand_to_fit_names_when_space_is_available() {
        let widths = table_text_widths(
            180,
            &["oai001_20x", "a-very-long-account-alias"],
            &["oai001@ozi.xyz"],
            &["Pro 20×", "Team - NightCity Workspace"],
        );

        assert!(widths.alias >= "a-very-long-account-alias".chars().count() as u16);
        assert!(widths.plan >= "Team - NightCity Workspace".chars().count() as u16);
    }

    #[test]
    fn account_table_columns_fit_an_eighty_column_terminal() {
        let widths = table_text_widths(
            80,
            &["a-very-long-account-alias"],
            &["a-very-long-address@example.com"],
            &["Team - NightCity Workspace"],
        );

        assert!(widths.alias + widths.email + widths.plan <= 16);
    }

    #[test]
    fn account_table_columns_use_extra_space_beyond_the_old_caps() {
        let alias = "a".repeat(45);
        let email = format!("{}@example.com", "e".repeat(40));
        let plan = format!("Team - {}", "Workspace".repeat(5));
        let widths = table_text_widths(260, &[&alias], &[&email], &[&plan]);

        assert_eq!(widths.alias, alias.len() as u16);
        assert_eq!(widths.email, email.len() as u16);
        assert_eq!(widths.plan, plan.len() as u16);
    }
}
