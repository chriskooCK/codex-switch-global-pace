use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::app::{App, ResetCardExpiry, UsageStatus};
use super::keymap;
use super::meter::{percent_marker_offset, usage_meter_line};
use super::popup;
use crate::jwt::PlanKind;
use crate::output::{
    format_local_time, format_reset_short, format_reset_time, reset_credits_count,
};
use crate::safe_text;
use crate::usage::{
    GlobalWeeklySummary, QuotaPaceState, UsageAvailability, UsageInfo, quota_pace_state,
    usage_availability,
};

// ── RGB-only color palette ───────────────────────────────
// All colors are explicit RGB to avoid mixing ANSI-16 + 24-bit,
// which causes rendering glitches on Windows conhost (cmd.exe / PowerShell).

const BG: Color = Color::Rgb(24, 24, 24); // near-black background
const C_WHITE: Color = Color::Rgb(240, 240, 240); // primary text
const C_GRAY: Color = Color::Rgb(180, 180, 180); // secondary text
const DIM: Color = Color::Rgb(120, 120, 120); // dim labels / placeholders
const C_RED: Color = Color::Rgb(255, 90, 90); // errors and unavailable account states
const C_GREEN: Color = Color::Rgb(80, 220, 120); // OK, active
const C_YELLOW: Color = Color::Rgb(255, 220, 80); // keys, usage ahead of pace
const C_CYAN: Color = Color::Rgb(100, 210, 255); // headers, prompts
const C_MAGENTA: Color = Color::Rgb(220, 130, 255); // team plans
const C_BLUE: Color = Color::Rgb(80, 140, 220); // borders (inactive)
const C_HIGHLIGHT_BG: Color = Color::Rgb(55, 55, 65); // selected row bg

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

fn safe_display(value: &str) -> String {
    safe_text::terminal_text(value).into_owned()
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

fn fitted_segments(max_width: usize, segments: impl IntoIterator<Item = String>) -> String {
    let mut used_width: usize = 0;
    let mut text = String::new();

    for segment in segments {
        let decorated = if text.is_empty() {
            segment
        } else {
            format!(" · {segment}")
        };
        let segment_width = display_width(&decorated);
        let Some(next_width) = used_width.checked_add(segment_width) else {
            break;
        };
        if next_width > max_width {
            break;
        }
        text.push_str(&decorated);
        used_width = next_width;
    }

    text
}

fn status_message_color(is_error: bool) -> Color {
    if is_error { C_RED } else { C_CYAN }
}

fn editable_input_line(
    prefix: &'static str,
    input: &str,
    cursor: usize,
    hint: &'static str,
) -> Line<'static> {
    let byte_cursor = super::app::grapheme_to_byte(input, cursor);
    let (before_cursor, after_cursor) = input.split_at(byte_cursor);

    Line::from(vec![
        Span::styled(prefix, base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(
            before_cursor.to_string(),
            base().fg(C_WHITE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("#", base().fg(C_GRAY)),
        Span::styled(
            after_cursor.to_string(),
            base().fg(C_WHITE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint, base().fg(DIM)),
    ])
}

pub fn render(f: &mut Frame, app: &mut App, now: i64) {
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

    render_account_table(f, app, vertical[0], now);
    let global_weekly = app.global_weekly_summary(now);
    let reset_card_expiry = app.earliest_reset_card_expiry(now);
    render_global_weekly_pace(
        f,
        &global_weekly,
        reset_card_expiry.as_ref(),
        now,
        vertical[1],
    );
    if app.detail_visible && detail_height > 0 {
        render_detail_panel(f, app, vertical[2], now);
    }
    render_status_bar(f, app, vertical[3]);

    // Overlays (rendered last, on top of everything).
    // Help popup takes top priority since the user invoked it explicitly.
    if let Some(state) = app.help_popup.as_mut() {
        render_help_popup(f, state, area);
    } else if let Some(menu) = app.menu.as_mut() {
        menu.render(f, area, now);
    }

    // Ratatui colors are independent of the ANSI helpers used by the plain
    // CLI. Normalize the completed frame at this one rendering boundary so
    // `--color never` and NO_COLOR also cover every TUI view and overlay.
    // Keep text modifiers: bold is the non-color selection cue, so clearing
    // the entire style would make keyboard navigation invisible.
    apply_frame_color_policy(f.buffer_mut(), crate::color::enabled());
}

fn apply_frame_color_policy(buffer: &mut ratatui::buffer::Buffer, color_enabled: bool) {
    if !color_enabled {
        for cell in &mut buffer.content {
            cell.set_style(
                Style::default()
                    .fg(Color::Reset)
                    .bg(Color::Reset)
                    .underline_color(Color::Reset),
            );
        }
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

pub(super) fn quota_pace_color(used_percent: Option<f64>, pace_percent: Option<f64>) -> Color {
    match quota_pace_state(used_percent, pace_percent) {
        QuotaPaceState::UsageAhead => C_YELLOW,
        QuotaPaceState::PaceAheadOrEqual => C_GREEN,
        QuotaPaceState::Unavailable => DIM,
    }
}

fn render_global_weekly_pace(
    f: &mut Frame,
    summary: &GlobalWeeklySummary,
    reset_card_expiry: Option<&ResetCardExpiry>,
    now: i64,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if area.height == GLOBAL_WEEKLY_COMPACT_HEIGHT {
        f.render_widget(
            Paragraph::new(compact_global_weekly_line(
                summary,
                reset_card_expiry,
                now,
                area.width as usize,
            ))
            .style(base()),
            area,
        );
        return;
    }

    let block = Block::default()
        .title(" Global Weekly Pace ")
        .borders(Borders::ALL)
        .border_style(base().fg(C_BLUE))
        .style(base());
    let inner = block.inner(area);
    let content = Rect {
        x: inner.x.saturating_add(1),
        width: inner.width.saturating_sub(2),
        ..inner
    };
    let pace_color = quota_pace_color(
        summary.aggregate_used_percent,
        summary.aggregate_elapsed_percent,
    );

    f.render_widget(block, area);

    let gauge_area = Rect {
        height: content.height.min(1),
        ..content
    };
    let mut global_marker = None;
    match (
        summary.aggregate_used_percent,
        summary.aggregate_elapsed_percent,
    ) {
        (Some(used), Some(elapsed)) => {
            let areas = usage_bar_areas(gauge_area);
            let marker_pace = crate::usage::visible_pace_marker(Some(used), Some(elapsed));
            let marker_offset =
                marker_pace.and_then(|value| percent_marker_offset(value, areas.bar.width));
            let remaining = (100.0 - used).max(0.0);
            render_usage_bar_row(
                f,
                areas,
                Line::from(Span::styled("7d", base().fg(C_WHITE))),
                usage_meter_line(
                    Some(used),
                    marker_offset,
                    areas.bar.width,
                    base().fg(pace_color),
                    base().fg(DIM),
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

    let summary_text = fitted_segments(
        content.width.into(),
        global_weekly_summary_segments(summary, reset_card_expiry, now),
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
    reset_card_expiry: Option<&ResetCardExpiry>,
    now: i64,
    width: usize,
) -> Line<'static> {
    let prefix = if width >= 28 {
        " Global Weekly: "
    } else {
        " Global: "
    };
    let summary_text = fitted_segments(
        width.saturating_sub(display_width(prefix)),
        global_weekly_summary_segments(summary, reset_card_expiry, now),
    );

    Line::from(vec![
        Span::styled(prefix, base().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(summary_text, base().fg(C_GRAY)),
    ])
}

fn global_weekly_summary_segments(
    summary: &GlobalWeeklySummary,
    reset_card_expiry: Option<&ResetCardExpiry>,
    now: i64,
) -> Vec<String> {
    let mut segments = Vec::with_capacity(3);
    if let Some(accounts) = global_account_count_text(summary) {
        segments.push(accounts);
    }
    if let Some(next) = next_reset_text(summary, now) {
        segments.push(format!("Next reset: {next}"));
    }
    if let Some(expiry) = reset_card_expiry
        && let Some(next) = account_event_text(&expiry.alias, expiry.expires_at, now)
    {
        segments.push(format!("Card expiry: {next}"));
    }
    segments
}

fn global_account_count_text(summary: &GlobalWeeklySummary) -> Option<String> {
    if summary.excluded_accounts == 0 {
        let noun = if summary.included_accounts == 1 {
            "account"
        } else {
            "accounts"
        };
        return Some(format!("{} {noun}", summary.included_accounts));
    }
    let total = summary
        .included_accounts
        .checked_add(summary.excluded_accounts)?;
    Some(format!("{}/{} accounts", summary.included_accounts, total))
}

fn next_reset_text(summary: &GlobalWeeklySummary, now: i64) -> Option<String> {
    let alias = summary.next_reset_alias.as_deref()?;
    let resets_at = summary.next_reset_at?;
    account_event_text(alias, resets_at, now)
}

fn account_event_text(alias: &str, timestamp: i64, now: i64) -> Option<String> {
    let remaining = u64::try_from(timestamp.checked_sub(now)?).ok()?;
    (remaining > 0).then(|| format!("{alias} in {}", format_duration_compact(remaining)))
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

fn render_account_table(f: &mut Frame, app: &App, area: Rect, now: i64) {
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

            let alias = safe_display(&entry.alias);
            let email = safe_display(entry.info.email.as_deref().unwrap_or("--"));
            let api_plan = if let UsageStatus::Loaded(u) = &entry.usage {
                u.plan_type.as_deref()
            } else {
                None
            };
            let effective_plan = api_plan.or(entry.info.plan_type.as_deref());
            let plan_label = safe_display(&entry.info.plan_label_with(effective_plan));
            let plan_style = plan_color(effective_plan, is_selected);

            let (
                status_text,
                status_color,
                pct_5h,
                pct_5h_color,
                pct_7d,
                pct_7d_color,
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
                Color,
                String,
                Color,
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
                    DIM,
                    "--".into(),
                    DIM,
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
                    C_YELLOW,
                    "...".into(),
                    C_YELLOW,
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
                    C_RED,
                    "Err".into(),
                    C_RED,
                    "--".into(),
                    DIM,
                    "--".into(),
                    DIM,
                    "Err".into(),
                    C_RED,
                ),
                UsageStatus::Loaded(u) => {
                    let refreshing = app.is_refreshing(&entry.alias);
                    let (p5, p5c) =
                        quota_table_value(u.primary.as_ref(), crate::usage::WINDOW_5H_SECS, now);
                    let (p7, p7c) =
                        quota_table_value(u.secondary.as_ref(), crate::usage::WINDOW_7D_SECS, now);
                    let r5_ts = u.primary.as_ref().and_then(|w| w.resets_at);
                    let r5 = r5_ts
                        .map(|timestamp| format_reset_short(timestamp, now))
                        .unwrap_or_else(|| "--".into());
                    let r5c = r5_ts
                        .map(|timestamp| reset_timestamp_color(timestamp, now))
                        .unwrap_or(DIM);
                    let r7_ts = u.secondary.as_ref().and_then(|w| w.resets_at);
                    let r7 = r7_ts
                        .map(|timestamp| format_reset_short(timestamp, now))
                        .unwrap_or_else(|| "--".into());
                    let r7c = r7_ts
                        .map(|timestamp| reset_timestamp_color(timestamp, now))
                        .unwrap_or(DIM);
                    let cards = reset_cards_table_text(u);
                    let cards_color = reset_cards_color(u);
                    if refreshing {
                        (
                            "Refresh".into(),
                            C_YELLOW,
                            p5,
                            p5c,
                            p7,
                            p7c,
                            r5,
                            r5c,
                            r7,
                            r7c,
                            cards,
                            cards_color,
                        )
                    } else {
                        let (status, color) = match usage_availability(u, &entry.info) {
                            UsageAvailability::Available => ("OK", C_GREEN),
                            UsageAvailability::Limited => ("Used up", C_RED),
                            UsageAvailability::Unavailable => ("N/A", DIM),
                        };
                        (
                            status.into(),
                            color,
                            p5,
                            p5c,
                            p7,
                            p7c,
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
                Cell::from(alias).style(row_style),
                Cell::from(email).style(row_style),
                Cell::from(plan_label).style(plan_style),
                Cell::from(status_text).style(base().fg(status_color).add_modifier(
                    if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                )),
                Cell::from(pct_5h).style(usage_pct_style(pct_5h_color, is_selected)),
                Cell::from(pct_7d).style(usage_pct_style(pct_7d_color, is_selected)),
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
            s = safe_text::terminal_text(&s.query)
        )
    } else {
        format!(" Accounts ({})", app.accounts.len())
    };
    if loading_count > 0 {
        title.push_str(&format!(" -- fetching {loading_count}..."));
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

    let alias_labels: Vec<String> = app
        .view_indices
        .iter()
        .map(|&idx| safe_display(&app.accounts[idx].alias))
        .collect();
    let email_labels: Vec<String> = app
        .view_indices
        .iter()
        .map(|&idx| safe_display(app.accounts[idx].info.email.as_deref().unwrap_or("--")))
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
            safe_display(
                &entry
                    .info
                    .plan_label_with(api_plan.or(entry.info.plan_type.as_deref())),
            )
        })
        .collect();
    let aliases: Vec<&str> = alias_labels.iter().map(String::as_str).collect();
    let emails: Vec<&str> = email_labels.iter().map(String::as_str).collect();
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

fn render_detail_panel(f: &mut Frame, app: &App, area: Rect, now: i64) {
    let entry = match app
        .selected_account_idx()
        .and_then(|idx| app.accounts.get(idx))
    {
        Some(e) => e,
        None => return,
    };

    let title = if entry.is_current {
        format!(" * {} (active) ", safe_text::terminal_text(&entry.alias))
    } else {
        format!(" {} ", safe_text::terminal_text(&entry.alias))
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
            let p = Paragraph::new(format!("Error: {}", safe_text::terminal_text(&e.detail)))
                .style(base().fg(C_RED));
            f.render_widget(p, layout[0]);
        }
        UsageStatus::Loaded(u) => {
            render_usage_gauges(f, u, layout[0], now);
        }
    }
}

pub(super) fn render_usage_gauges(f: &mut Frame, u: &UsageInfo, area: Rect, now: i64) {
    if !u.parse_issues.is_empty() {
        let detail = u
            .parse_issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        f.render_widget(
            Paragraph::new(format!(
                "Usage response rejected: {}",
                safe_text::terminal_text(&detail)
            ))
            .style(base().fg(C_RED)),
            area,
        );
        return;
    }
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
                format!("{}  unavailable", safe_text::terminal_text(name))
            } else {
                safe_display(name)
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
                crate::usage::quota_window_spec(window, "5h", crate::usage::WINDOW_5H_SECS);
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
                crate::usage::quota_window_spec(window, "7d", crate::usage::WINDOW_7D_SECS);
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

struct StatusBarContent {
    lines: Vec<Line<'static>>,
}

fn wrapped_status_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    popup::wrap_line(&line, usize::from(width))
}

fn confirmation_status_line(confirm: &super::app::ConfirmAction) -> Line<'static> {
    let message = match confirm {
        super::app::ConfirmAction::Delete(alias) => {
            format!("Delete profile '{alias}'? (y/n)")
        }
        super::app::ConfirmAction::BatchDelete(aliases) => {
            format!("Delete {} marked profile(s)? (y/n)", aliases.len())
        }
        super::app::ConfirmAction::ConsumeResetCard {
            alias, expires_at, ..
        } => format!(
            "Confirm reset card for '{alias}' expiring {}: y to use, any other key cancels",
            safe_text::terminal_text(expires_at)
        ),
    };
    Line::from(Span::styled(
        message,
        base().fg(C_RED).add_modifier(Modifier::BOLD),
    ))
}

fn status_bar_content(app: &App, width: u16) -> StatusBarContent {
    if let Some(rename) = &app.rename {
        return StatusBarContent {
            lines: wrapped_status_line(
                editable_input_line(
                    " Rename: ",
                    &rename.input,
                    rename.cursor,
                    "  (Enter confirm / Esc cancel)",
                ),
                width,
            ),
        };
    }
    if let Some(confirm) = &app.confirm {
        return StatusBarContent {
            lines: wrapped_status_line(confirmation_status_line(confirm), width),
        };
    }
    if app.search_active
        && let Some(search) = &app.search
    {
        return StatusBarContent {
            lines: wrapped_status_line(
                editable_input_line(
                    " /",
                    &search.query,
                    search.cursor,
                    "  (Enter accept / Esc clear)",
                ),
                width,
            ),
        };
    }
    if let Some(progress) = app.profile_switch_progress() {
        return StatusBarContent {
            lines: wrapped_status_line(
                Line::from(Span::styled(progress, base().fg(C_CYAN))),
                width,
            ),
        };
    }
    if let Some(status) = &app.status_msg {
        return StatusBarContent {
            lines: wrapped_status_line(
                Line::from(Span::styled(
                    status.clone(),
                    base().fg(status_message_color(app.status_is_error)),
                )),
                width,
            ),
        };
    }
    if !app.marked.is_empty() {
        let lines = wrapped_status_line(
            Line::from(vec![
                Span::styled(" ", base()),
                Span::styled(
                    app.marked.len().to_string(),
                    base().fg(C_YELLOW).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" selected", base().fg(C_YELLOW)),
                Span::styled(" \u{2014} ", base().fg(DIM)),
                Span::styled("enter", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
                Span::styled(" for batch \u{2502} ", base().fg(DIM)),
                Span::styled("esc", base().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
                Span::styled(" to clear", base().fg(DIM)),
            ]),
            width,
        );
        return StatusBarContent {
            lines: append_status_bar_version(app, lines, width),
        };
    }
    let lines = build_shortcut_lines(app, width.into());
    StatusBarContent {
        lines: append_status_bar_version(app, lines, width),
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let content = status_bar_content(app, area.width);
    f.render_widget(Paragraph::new(content.lines).style(base()), area);
}

fn status_bar_version(app: &App) -> Line<'static> {
    let version = crate::update::current_version();
    let ver_spans: Vec<Span> = if let Some(latest) = &app.update_available {
        vec![
            Span::styled(format!("v{version}"), base().fg(DIM)),
            Span::styled(
                format!(" -> v{} ", safe_text::terminal_text(latest)),
                base().fg(C_YELLOW),
            ),
        ]
    } else {
        vec![Span::styled(format!("v{version} "), base().fg(DIM))]
    };
    Line::from(ver_spans)
}

fn append_status_bar_version(
    app: &App,
    mut lines: Vec<Line<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    let version = status_bar_version(app);
    let width = usize::from(width);
    let version_width = version.width();
    if width < version_width {
        return lines;
    }

    let separator = Span::styled(" \u{2502} ", base().fg(DIM));
    let inline_version_width = separator.width().saturating_add(version_width);
    let last = lines
        .last_mut()
        .expect("status bar builders always return at least one line");
    if last.width().saturating_add(inline_version_width) <= width {
        let padding = width.saturating_sub(last.width().saturating_add(inline_version_width));
        if padding > 0 {
            last.spans.push(Span::styled(" ".repeat(padding), base()));
        }
        last.spans.push(separator);
        last.spans.extend(version.spans);
        return lines;
    }

    let padding = width.saturating_sub(version_width);
    let mut version_line = Line::from(Span::styled(" ".repeat(padding), base()));
    version_line.spans.extend(version.spans);
    lines.push(version_line);
    lines
}

/// Render a single usage gauge (5h or 7d) with block chars and pace marker.
fn render_usage_gauge(
    f: &mut Frame,
    w: &crate::usage::WindowUsage,
    label: &str,
    window_secs: Option<i64>,
    now: i64,
    area: Rect,
) {
    let used_percent = crate::usage::normalized_quota_usage(w.used_percent);
    let quota_text = match used_percent {
        Some(used) => format!("  {used:>3.0}% used  {:>3.0}% left", 100.0 - used),
        None => "   --% used   --% left".to_string(),
    };
    let pace = window_secs.and_then(|duration| crate::usage::pace_percent_at(w, duration, now));
    let marker_pace = crate::usage::visible_pace_marker(used_percent, pace);
    let (reset_str, reset_style) = match w.resets_at {
        Some(resets_at) => {
            let reset_style = base().fg(reset_timestamp_color(resets_at, now));
            (format_reset_time(resets_at, now), reset_style)
        }
        None => ("--".to_string(), base().fg(DIM)),
    };

    // Row 1: fixed label and metrics columns around the shared-width meter.
    let gauge_area = Rect { height: 1, ..area };
    let areas = usage_bar_areas(gauge_area);
    let bar_width = areas.bar.width;

    let used_color = quota_pace_color(used_percent, pace);
    let used_style = base().fg(used_color);
    let remaining_style = base().fg(DIM);
    let pace_style = base().fg(C_WHITE).add_modifier(Modifier::BOLD);

    let pace_pos = marker_pace.and_then(|value| percent_marker_offset(value, bar_width));
    let bar_line = usage_meter_line(
        used_percent,
        pace_pos,
        bar_width,
        used_style,
        remaining_style,
        pace_style,
    );

    render_usage_bar_row(
        f,
        areas,
        Line::from(Span::styled(label.to_string(), base().fg(C_WHITE))),
        bar_line,
        Line::from(Span::styled(quota_text, base().fg(used_color))),
    );

    // Row 2: "started HH:MM" left, "↑ pace" at pace position, "resets in ..." right
    let reset_area = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let reset_text = format!("resets in {reset_str}");
    let started_text = window_secs
        .and_then(|duration| {
            w.resets_at
                .and_then(|timestamp| timestamp.checked_sub(duration))
        })
        .map(|timestamp| format!("started {}", format_local_time(timestamp, now)))
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

fn quota_table_value(
    window: Option<&crate::usage::WindowUsage>,
    default_secs: i64,
    now: i64,
) -> (String, Color) {
    let Some(window) = window else {
        return ("--".into(), DIM);
    };
    let Some(used) = crate::usage::normalized_quota_usage(window.used_percent) else {
        return ("--".into(), DIM);
    };
    let remaining = 100.0 - used;
    let pace = crate::usage::quota_window_duration_secs(window, default_secs)
        .and_then(|window_secs| crate::usage::pace_percent_at(window, window_secs, now));
    (
        format!("{remaining:.0}%"),
        quota_pace_color(Some(used), pace),
    )
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

fn reset_timestamp_color(timestamp: i64, now: i64) -> Color {
    timestamp.checked_sub(now).map(reset_color).unwrap_or(DIM)
}

fn usage_pct_style(color: Color, is_selected: bool) -> Style {
    let s = base().fg(color);
    if is_selected {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

fn status_bar_indicator_span(app: &App, indicator: keymap::StatusBarIndicator) -> Span<'static> {
    match indicator {
        keymap::StatusBarIndicator::AutoRefresh if app.auto_refresh_enabled => {
            Span::styled(" [ON]", base().fg(C_GREEN).add_modifier(Modifier::BOLD))
        }
        keymap::StatusBarIndicator::AutoRefresh => Span::styled(" [OFF]", base().fg(DIM)),
    }
}

fn build_shortcut_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let key_style = base().fg(C_YELLOW);
    let sep_style = base().fg(DIM);
    let label_style = base().fg(C_GRAY);
    let space_style = base();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" ", space_style)];
    let mut used = 1usize;

    let sep = " \u{2502} ";
    let sep_width = display_width(sep);
    for (keys, item) in keymap::status_bar_items() {
        let indicator = item
            .indicator
            .map(|indicator| status_bar_indicator_span(app, indicator));
        let item_len = display_width(keys)
            + 1
            + display_width(item.label)
            + indicator.as_ref().map(Span::width).unwrap_or(0);
        if used > 1 && used + sep_width + item_len > width {
            lines.push(Line::from(spans));
            spans = vec![Span::styled(" ", space_style)];
            used = 1;
        }
        if used > 1 {
            spans.push(Span::styled(sep, sep_style));
            used += sep_width;
        }
        spans.push(Span::styled(keys, key_style));
        spans.push(Span::styled(" ", space_style));
        spans.push(Span::styled(item.label, label_style));
        if let Some(indicator) = indicator {
            spans.push(indicator);
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
    status_bar_content(app, width).lines.len()
}

#[cfg(test)]
mod tests {
    use super::{
        C_BLUE, C_CYAN, C_GRAY, C_GREEN, C_MAGENTA, C_RED, C_YELLOW, DIM,
        GLOBAL_WEEKLY_COMPACT_HEIGHT, GLOBAL_WEEKLY_FULL_HEIGHT, MIN_ACCOUNT_TABLE_HEIGHT,
        PACE_LABEL, apply_frame_color_policy, display_width, editable_input_line, fitted_segments,
        global_weekly_panel_height, plan_color, quota_pace_color, quota_table_value, render,
        render_account_table, render_detail_panel, render_global_weekly_pace, render_usage_gauge,
        render_usage_gauges, reset_timestamp_color, status_bar_content, status_bar_height,
        status_message_color, table_text_widths, usage_gauges_height,
    };
    use crate::jwt::AccountInfo;
    use crate::tui::app::{
        AccountEntry, App, ConfirmAction, RenameState, ResetCardExpiry, SearchState, UsageStatus,
    };
    use crate::tui::meter::percent_marker_offset;
    use crate::usage::{
        AdditionalRateLimit, GlobalPaceWeighting, GlobalWeeklySummary, UsageAvailability,
        UsageError, UsageInfo, UsageParseIssue, WindowUsage, usage_availability,
    };
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::{Terminal, backend::TestBackend};

    type MeterBounds = Option<(u16, u16)>;
    const TEST_NOW: i64 = 1_000_000;

    #[test]
    fn tui_frame_color_policy_strips_colors_but_preserves_text_modifiers() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_style(
            Style::default()
                .fg(Color::Red)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
        buffer[(1, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Rgb(4, 5, 6))
                .add_modifier(Modifier::DIM),
        );

        apply_frame_color_policy(&mut buffer, true);
        assert_eq!(buffer[(0, 0)].fg, Color::Red);
        assert_eq!(buffer[(0, 0)].bg, Color::Blue);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));

        apply_frame_color_policy(&mut buffer, false);

        for cell in buffer.content() {
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
        }
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::UNDERLINED));
        assert!(buffer[(1, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn no_color_account_table_keeps_the_selected_row_visible() {
        let mut app = App::new();
        for alias in ["first", "second"] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loading,
                is_current: false,
            });
        }
        app.update_view();
        app.selected = 1;

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_account_table(frame, &app, area, TEST_NOW);
                apply_frame_color_policy(frame.buffer_mut(), false);
            })
            .unwrap();

        let find_alias = |alias: &str| {
            (0..12)
                .find_map(|y| {
                    row_text(terminal.backend(), y)
                        .find(alias)
                        .map(|x| (x as u16, y))
                })
                .unwrap_or_else(|| panic!("missing account row for {alias}"))
        };
        let first = find_alias("first");
        let second = find_alias("second");
        let buffer = terminal.backend().buffer();

        assert!(!buffer[first].modifier.contains(Modifier::BOLD));
        assert!(buffer[second].modifier.contains(Modifier::BOLD));
        for position in [first, second] {
            assert_eq!(buffer[position].fg, Color::Reset);
            assert_eq!(buffer[position].bg, Color::Reset);
        }
    }

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

    fn status_content_text(app: &App) -> String {
        status_bar_content(app, 80)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn ordinary_footer_surfaces_primary_actions_and_auto_refresh_state() {
        let mut app = App::new();

        let off = status_content_text(&app);
        assert!(off.contains("/ search"), "{off:?}");
        assert!(off.contains("enter menu"), "{off:?}");
        assert!(off.contains("a add new account"), "{off:?}");
        assert!(off.contains("r refresh"), "{off:?}");
        assert!(off.contains("t auto refresh [OFF]"), "{off:?}");
        assert!(off.contains("h help"), "{off:?}");
        assert!(!off.contains("nav"), "{off:?}");
        assert!(!off.contains("quota"), "{off:?}");
        assert!(!off.contains("q quit"), "{off:?}");

        app.auto_refresh_enabled = true;
        let on = status_content_text(&app);
        assert!(on.contains("t auto refresh [ON]"), "{on:?}");
        assert!(!on.contains("[OFF]"), "{on:?}");
    }

    #[test]
    fn auto_refresh_footer_state_has_distinct_visual_emphasis() {
        let mut app = App::new();

        let off = status_bar_content(&app, 120);
        let off_state = off
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == " [OFF]")
            .expect("disabled auto-refresh state");
        assert_eq!(off_state.style.fg, Some(DIM));
        assert!(!off_state.style.add_modifier.contains(Modifier::BOLD));

        app.auto_refresh_enabled = true;
        let on = status_bar_content(&app, 120);
        let on_state = on
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.as_ref() == " [ON]")
            .expect("enabled auto-refresh state");
        assert_eq!(on_state.style.fg, Some(C_GREEN));
        assert!(on_state.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn footer_and_version_share_the_measured_layout_without_overlap() {
        for width in [60, 80, 100] {
            let mut app = App::new();
            app.update_available = Some("9.9.9".to_string());
            let status_height = status_bar_height(&app, width);
            let backend = TestBackend::new(width, 20);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal
                .draw(|frame| render(frame, &mut app, TEST_NOW))
                .unwrap();

            let rendered = ((20 - status_height as u16)..20)
                .map(|y| row_text(terminal.backend(), y).trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            for expected in [
                "/ search",
                "enter menu",
                "a add new account",
                "r refresh",
                "t auto refresh [OFF]",
                "h help",
                "-> v9.9.9",
            ] {
                assert!(rendered.contains(expected), "width {width}: {rendered:?}");
            }
            let content = status_bar_content(&app, width);
            assert!(
                content
                    .lines
                    .iter()
                    .all(|line| line.width() <= usize::from(width)),
                "width {width}: a status line exceeds its measured width"
            );
            assert_eq!(
                content.lines.last().map(|line| line.width()),
                Some(usize::from(width)),
                "width {width}: version should remain right-aligned"
            );
            if width == 100 {
                let version_line = content.lines.last().expect("version line");
                let version_text = version_line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                assert!(version_text.contains("-> v9.9.9"), "{version_text:?}");
                assert!(!version_text.contains("h help"), "{version_text:?}");
            }
        }
    }

    #[test]
    fn status_message_color_distinguishes_errors_from_information() {
        assert_eq!(status_message_color(false), C_CYAN);
        assert_eq!(status_message_color(true), C_RED);
    }

    #[tokio::test]
    async fn tracked_profile_switch_progress_survives_transient_status_expiry_in_render() {
        let mut app = App::new();
        app.track_pending_profile_switch_for_render_test("slow-account");
        app.status_msg = Some("temporary background notice".to_string());
        app.status_is_error = true;
        app.status_expiry = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        app.tick();
        assert!(app.status_msg.is_none());

        let width = 80;
        let height = 18;
        let status_height = status_bar_height(&app, width);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, TEST_NOW))
            .unwrap();

        let rendered = ((height - status_height as u16)..height)
            .map(|y| row_text(terminal.backend(), y).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Preparing switch to slow-account..."),
            "{rendered:?}"
        );
    }

    #[tokio::test]
    async fn interactive_statuses_precede_switch_progress_which_precedes_transient_status() {
        let mut app = App::new();
        app.track_pending_profile_switch_for_render_test("account");
        app.status_msg = Some("temporary background notice".to_string());

        let progress = status_content_text(&app);
        assert!(progress.contains("Preparing switch to account..."));
        assert!(!progress.contains("temporary background notice"));

        app.search_active = true;
        app.search = Some(SearchState {
            query: "needle".to_string(),
            cursor: 6,
        });
        app.confirm = Some(ConfirmAction::Delete("delete-me".to_string()));
        app.rename = Some(RenameState {
            old_alias: "old".to_string(),
            input: "renamed".to_string(),
            cursor: 7,
        });
        let rename = status_content_text(&app);
        assert!(rename.contains("Rename:"));
        assert!(rename.contains("renamed"));
        assert!(!rename.contains("Preparing switch"));

        app.rename = None;
        let confirm = status_content_text(&app);
        assert!(confirm.contains("Delete profile 'delete-me'?"));
        assert!(!confirm.contains("Preparing switch"));

        app.confirm = None;
        let search = status_content_text(&app);
        assert!(search.contains("needle"));
        assert!(!search.contains("Preparing switch"));
    }

    #[test]
    fn unrepresentable_reset_distance_is_neutral() {
        assert_eq!(reset_timestamp_color(i64::MAX, i64::MIN), DIM);
    }

    #[test]
    fn safety_status_wraps_without_version_overdraw_at_supported_widths() {
        let message =
            "account: reset-card consumption may have occurred; verify before retry".to_string();
        for width in [60, 80] {
            let mut app = App::new();
            app.status_msg = Some(message.clone());
            app.status_is_error = true;
            let status_height = status_bar_height(&app, width);
            let backend = TestBackend::new(width, 18);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal
                .draw(|frame| render(frame, &mut app, TEST_NOW))
                .unwrap();

            let rendered = ((18 - status_height as u16)..18)
                .map(|y| row_text(terminal.backend(), y).trim_end().to_string())
                .collect::<String>();
            assert!(rendered.contains(&message), "width {width}: {rendered:?}");
            assert!(
                !rendered.contains(crate::update::current_version()),
                "width {width}: {rendered:?}"
            );
        }
    }

    #[test]
    fn status_height_uses_display_width_for_cjk() {
        let mut app = App::new();
        app.status_msg = Some("한".repeat(40));

        assert_eq!(status_bar_height(&app, 60), 2);
        assert_eq!(status_bar_height(&app, 80), 1);
    }

    #[test]
    fn confirmation_footer_height_and_render_share_the_same_wrapped_model() {
        let mut app = App::new();
        app.confirm = Some(ConfirmAction::Delete(
            "account-with-a-long-alias".to_string(),
        ));
        let width = 20;
        let height = status_bar_height(&app, width);
        assert!(height > 1);
        let backend = TestBackend::new(width, 18);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &mut app, TEST_NOW))
            .unwrap();

        let rendered = ((18 - height as u16)..18)
            .map(|y| row_text(terminal.backend(), y).trim_end().to_string())
            .collect::<String>();
        assert!(
            rendered.contains("Delete profile 'account-with-a-long-alias'? (y/n)"),
            "{rendered:?}"
        );
    }

    #[test]
    fn weekly_quota_is_plan_independent_while_explicit_limits_remain_limited() {
        let missing_windows = UsageInfo::default();
        let missing_percent = UsageInfo {
            primary: Some(WindowUsage::default()),
            ..UsageInfo::default()
        };
        let available = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(20.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };
        let exhausted = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(100.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };
        let broad_limited_incomplete = UsageInfo {
            account_limited: true,
            ..UsageInfo::default()
        };
        let explicit_spend_blocker = UsageInfo {
            account_limited: true,
            spend_control_reached: true,
            ..UsageInfo::default()
        };
        let malformed = UsageInfo {
            parse_issues: vec![UsageParseIssue::InvalidCodeReviewRateLimit {
                detail: "expected an object".to_string(),
            }],
            ..UsageInfo::default()
        };
        let plus = AccountInfo {
            plan_type: Some("plus".to_string()),
            ..AccountInfo::default()
        };

        assert_eq!(
            usage_availability(&missing_windows, &AccountInfo::default()),
            UsageAvailability::Unavailable
        );
        assert_eq!(
            usage_availability(&missing_percent, &AccountInfo::default()),
            UsageAvailability::Unavailable
        );
        assert_eq!(
            usage_availability(&available, &AccountInfo::default()),
            UsageAvailability::Available
        );
        assert_eq!(
            usage_availability(&available, &plus),
            UsageAvailability::Available
        );
        assert_eq!(
            usage_availability(&exhausted, &AccountInfo::default()),
            UsageAvailability::Limited
        );
        assert_eq!(
            usage_availability(&broad_limited_incomplete, &AccountInfo::default()),
            UsageAvailability::Unavailable
        );
        assert_eq!(
            usage_availability(&explicit_spend_blocker, &AccountInfo::default()),
            UsageAvailability::Limited
        );
        assert_eq!(
            usage_availability(&malformed, &AccountInfo::default()),
            UsageAvailability::Unavailable
        );
    }

    #[test]
    fn account_table_renders_loaded_missing_quota_as_neutral() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "missing".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                primary: Some(WindowUsage::default()),
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.update_view();
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_account_table(frame, &app, area, TEST_NOW);
            })
            .unwrap();

        let (x, y) = (0..12)
            .find_map(|y| {
                row_text(terminal.backend(), y)
                    .find("N/A")
                    .map(|x| (x as u16, y))
            })
            .expect("neutral missing-quota status");
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((x, y))
                .expect("status cell")
                .fg,
            DIM
        );
    }

    #[test]
    fn account_table_renders_exhausted_quota_as_used_up() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "exhausted".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                secondary: Some(WindowUsage {
                    used_percent: Some(100.0),
                    ..WindowUsage::default()
                }),
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.update_view();
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_account_table(frame, &app, area, TEST_NOW);
            })
            .unwrap();

        let (x, y) = (0..12)
            .find_map(|y| {
                row_text(terminal.backend(), y)
                    .find("Used up")
                    .map(|x| (x as u16, y))
            })
            .expect("used-up status");
        let rendered = (0..12)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<String>();
        assert!(rendered.contains("Used up"), "{rendered:?}");
        assert!(!rendered.contains("Limited"), "{rendered:?}");
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((x, y))
                .expect("status cell")
                .fg,
            C_RED
        );
    }

    #[test]
    fn dashboard_never_replays_controls_from_api_or_release_text() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                email: Some("user\u{1b}]52;mail\u{7}@example.com".into()),
                plan_type: Some("future\nplan".into()),
                ..AccountInfo::default()
            },
            usage: UsageStatus::Error(UsageError {
                summary: "failed".into(),
                detail: "server\u{1b}]52;detail\u{7}\nerror".into(),
            }),
            is_current: false,
        });
        app.update_available = Some("9.9\u{1b}]52;release\u{7}\nnext".into());
        app.update_view();
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &mut app, TEST_NOW))
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<String>();
        assert!(
            rendered.chars().all(|character| !character.is_control()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("user]52;mail@example.com"),
            "{rendered:?}"
        );
        assert!(rendered.contains("server]52;detailerror"), "{rendered:?}");
        assert!(rendered.contains("v9.9]52;releasenext"), "{rendered:?}");
    }

    #[test]
    fn additional_pool_title_is_sanitized_at_the_render_boundary() {
        let usage = UsageInfo {
            additional_limits: vec![AdditionalRateLimit {
                limit_name: Some("Pool\u{1b}]52;name\u{7}\nNext".into()),
                primary: Some(WindowUsage {
                    used_percent: Some(20.0),
                    resets_at: Some(TEST_NOW + 3600),
                    window_minutes: Some(300),
                }),
                ..AdditionalRateLimit::default()
            }],
            ..UsageInfo::default()
        };
        let backend = TestBackend::new(90, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_usage_gauges(frame, &usage, frame.area(), TEST_NOW))
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<String>();
        assert!(rendered.chars().all(|character| !character.is_control()));
        assert!(rendered.contains("Pool]52;nameNext"), "{rendered:?}");
    }

    #[test]
    fn malformed_infallible_usage_is_visible_in_account_details() {
        let usage = UsageInfo {
            parse_issues: vec![UsageParseIssue::InvalidAdditionalRateLimits {
                detail: "entry 0 has no rate_limit object".into(),
            }],
            ..UsageInfo::default()
        };
        let backend = TestBackend::new(90, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_usage_gauges(frame, &usage, frame.area(), TEST_NOW))
            .unwrap();

        let rendered = (0..terminal.backend().buffer().area.height)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<String>();
        assert!(rendered.contains("Usage response rejected"), "{rendered:?}");
        assert!(rendered.contains("additional_rate_limits"), "{rendered:?}");
    }

    #[test]
    fn editable_cursor_is_inserted_at_its_unicode_character_position() {
        let line = editable_input_line(" /", "a중b", 2, " hint");

        assert_eq!(line.to_string(), " /a중#b hint");
        assert_eq!(
            editable_input_line(" /", "a중b", 3, "").to_string(),
            " /a중b#"
        );
        assert_eq!(
            editable_input_line(" /", "👩‍💻a", 1, "").to_string(),
            " /👩‍💻#a"
        );
        assert_eq!(
            editable_input_line(" /", "e\u{301}x", 1, "").to_string(),
            " /e\u{301}#x"
        );
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

    fn reset_card_expiry(now: i64) -> ResetCardExpiry {
        ResetCardExpiry {
            alias: "work1".to_string(),
            expires_at: now + 2 * 86_400 + 6 * 3_600,
        }
    }

    fn dashboard_geometry(width: u16, is_current: bool) -> DashboardGeometry {
        let now = TEST_NOW;
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
                    None,
                    now,
                    Rect::new(0, 0, width, GLOBAL_WEEKLY_FULL_HEIGHT),
                );
                render_detail_panel(
                    frame,
                    &app,
                    Rect::new(0, GLOBAL_WEEKLY_FULL_HEIGHT, width, 6),
                    now,
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
    fn quota_pace_colors_use_relative_position_only() {
        assert_eq!(quota_pace_color(Some(1.0), Some(0.0)), C_YELLOW);
        assert_eq!(quota_pace_color(Some(50.0), Some(50.0)), C_GREEN);
        assert_eq!(quota_pace_color(Some(95.0), Some(99.0)), C_GREEN);
        assert_eq!(quota_pace_color(Some(100.0), Some(50.0)), C_YELLOW);
        assert_eq!(quota_pace_color(Some(20.0), None), DIM);
        assert_eq!(quota_pace_color(None, Some(20.0)), DIM);
    }

    #[test]
    fn quota_table_values_do_not_encode_state_in_the_text() {
        let now = TEST_NOW;
        let ahead = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + crate::usage::WINDOW_7D_SECS - 60),
            window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
        };
        let behind = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + crate::usage::WINDOW_7D_SECS / 2),
            window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
        };

        assert_eq!(
            quota_table_value(Some(&ahead), crate::usage::WINDOW_7D_SECS, now),
            ("80%".to_string(), C_YELLOW)
        );
        assert_eq!(
            quota_table_value(Some(&behind), crate::usage::WINDOW_7D_SECS, now),
            ("80%".to_string(), C_GREEN)
        );
        assert_eq!(
            quota_table_value(None, crate::usage::WINDOW_7D_SECS, now),
            ("--".to_string(), DIM)
        );
    }

    #[test]
    fn segmented_text_keeps_only_the_complete_priority_prefix() {
        assert_eq!(fitted_segments(2, ["ab".to_string()]), "ab");
        assert_eq!(fitted_segments(1, ["ab".to_string()]), "");
        assert_eq!(fitted_segments(2, ["한".to_string()]), "한");
        assert_eq!(
            fitted_segments(6, ["ab".to_string(), "c".to_string()]),
            "ab · c"
        );
        assert_eq!(
            fitted_segments(6, ["too long".to_string(), "x".to_string()]),
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
    fn full_global_panel_renders_usage_pace_marker_resets_and_card_expiry() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let reset_card_expiry = reset_card_expiry(now);
        let backend = TestBackend::new(110, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_global_weekly_pace(
                    frame,
                    &summary,
                    Some(&reset_card_expiry),
                    now,
                    frame.area(),
                )
            })
            .unwrap();

        let rendered = (0..GLOBAL_WEEKLY_FULL_HEIGHT)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Global Weekly Pace"));
        assert!(rendered.contains("7% used"));
        assert!(rendered.contains("93% left"));
        assert!(rendered.contains("↑ pace"));
        assert!(!rendered.contains("106.7%"));
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
        assert!(rendered.contains("3/4 accounts"));
        assert!(rendered.contains("Next reset: work2 in 3h18m"));
        assert!(rendered.contains("Card expiry: work1 in 2d6h"));
        for removed in [
            "Pace 106.7%",
            "100% normal",
            "equal weight",
            "+6.7%p",
            "Eff 320/300",
        ] {
            assert!(
                !rendered.contains(removed),
                "obsolete text remained: {removed}"
            );
        }
    }

    #[test]
    fn global_meter_uses_aggregate_usage_and_elapsed_pace() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.pace_percent = Some(75.6);
        summary.aggregate_used_percent = Some(3.0);
        summary.aggregate_elapsed_percent = Some(0.73);

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, None, now, frame.area()))
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
        assert!(!rendered.contains("75.6%"));
        assert!(rendered.contains("3/4 accounts"));
        assert!(!rendered.contains("-2.3%p"));
    }

    #[test]
    fn exhausted_global_meter_keeps_the_pace_marker() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.aggregate_used_percent = Some(100.0);
        summary.aggregate_elapsed_percent = Some(50.0);

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, None, now, frame.area()))
            .unwrap();

        let (meter_start, meter_end) = meter_bounds(terminal.backend(), 1).expect("global meter");
        let expected_offset = percent_marker_offset(50.0, meter_end - meter_start).unwrap();
        let marker_x = symbol_x(terminal.backend(), 1, "|").expect("pace marker");
        assert_eq!(marker_x, meter_start + expected_offset);
        assert_eq!(symbol_x(terminal.backend(), 2, "↑"), Some(marker_x));
    }

    #[test]
    fn global_pace_label_is_complete_at_every_meter_boundary() {
        let now = 1_000_000;
        for width in [40, 80, 110] {
            for elapsed in [0.0, 50.0, 97.73, 100.0] {
                let mut summary = global_summary(now);
                summary.aggregate_used_percent = Some(50.0);
                summary.aggregate_elapsed_percent = Some(elapsed);
                let backend = TestBackend::new(width, GLOBAL_WEEKLY_FULL_HEIGHT);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| {
                        render_global_weekly_pace(frame, &summary, None, now, frame.area())
                    })
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
        summary.aggregate_used_percent = None;
        summary.aggregate_elapsed_percent = None;
        summary.included_accounts = 0;
        summary.excluded_accounts = 2;
        summary.next_reset_at = None;
        summary.next_reset_alias = None;

        let backend = TestBackend::new(80, GLOBAL_WEEKLY_FULL_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_global_weekly_pace(frame, &summary, None, now, frame.area()))
            .unwrap();
        let full_text = (0..GLOBAL_WEEKLY_FULL_HEIGHT)
            .map(|y| row_text(terminal.backend(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full_text.contains("No valid current weekly quota data"));
        assert!(full_text.contains("0/2 accounts"));
        assert!(!full_text.contains("Pace unavailable"));
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
            .draw(|frame| render_global_weekly_pace(frame, &summary, None, now, frame.area()))
            .unwrap();
        let compact_text = row_text(terminal.backend(), 0);
        assert!(compact_text.contains("Global Weekly: 0/2 accounts"));
        assert!(!compact_text.contains("unavailable"));
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
                            Some(crate::usage::WINDOW_7D_SECS),
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
    fn compact_global_panel_keeps_account_count_and_reset_on_one_line() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let reset_card_expiry = reset_card_expiry(now);
        let backend = TestBackend::new(70, GLOBAL_WEEKLY_COMPACT_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_global_weekly_pace(
                    frame,
                    &summary,
                    Some(&reset_card_expiry),
                    now,
                    frame.area(),
                )
            })
            .unwrap();
        let text = row_text(terminal.backend(), 0);

        assert!(!text.contains("106.7%"));
        assert!(text.contains("3/4 accounts"));
        assert!(text.contains("Next reset: work2 in 3h18m"));
        assert!(!text.contains("Card expiry"));
        assert!(!text.contains("+6.7%p"));
        for symbol in ["█", "░", "|", "↑"] {
            assert_eq!(symbol_x(terminal.backend(), 0, symbol), None);
        }
    }

    #[test]
    fn compact_global_panel_reports_included_over_total_accounts() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let reset_card_expiry = reset_card_expiry(now);
        let backend = TestBackend::new(120, GLOBAL_WEEKLY_COMPACT_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_global_weekly_pace(
                    frame,
                    &summary,
                    Some(&reset_card_expiry),
                    now,
                    frame.area(),
                )
            })
            .unwrap();

        assert_eq!(
            row_text(terminal.backend(), 0).trim_end(),
            " Global Weekly: 3/4 accounts · Next reset: work2 in 3h18m · Card expiry: work1 in 2d6h"
        );
    }

    #[test]
    fn compact_global_panel_drops_the_complete_card_segment_at_its_width_boundary() {
        let now = 1_000_000;
        let summary = global_summary(now);
        let reset_card_expiry = reset_card_expiry(now);
        let full = " Global Weekly: 3/4 accounts · Next reset: work2 in 3h18m · Card expiry: work1 in 2d6h";
        let without_card = " Global Weekly: 3/4 accounts · Next reset: work2 in 3h18m";
        let full_width = display_width(full);

        assert_eq!(
            super::compact_global_weekly_line(&summary, Some(&reset_card_expiry), now, full_width,)
                .to_string(),
            full
        );
        assert_eq!(
            super::compact_global_weekly_line(
                &summary,
                Some(&reset_card_expiry),
                now,
                full_width - 1,
            )
            .to_string(),
            without_card
        );
    }

    #[test]
    fn card_expiry_is_independent_of_the_weekly_reset_segment() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.next_reset_at = None;
        summary.next_reset_alias = None;
        let reset_card_expiry = reset_card_expiry(now);

        let text = super::compact_global_weekly_line(&summary, Some(&reset_card_expiry), now, 120)
            .to_string();

        assert!(text.contains("3/4 accounts · Card expiry: work1 in 2d6h"));
        assert!(!text.contains("Next reset"));
    }

    #[test]
    fn global_panel_uses_a_plain_count_when_every_account_is_available() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.excluded_accounts = 0;

        let text = super::compact_global_weekly_line(&summary, None, now, 120).to_string();

        assert!(text.contains("Global Weekly: 3 accounts · "), "{text:?}");
        assert!(!text.contains("3/3 accounts"), "{text:?}");

        summary.included_accounts = 1;
        let text = super::compact_global_weekly_line(&summary, None, now, 120).to_string();
        assert!(text.contains("Global Weekly: 1 account · "), "{text:?}");
        assert!(!text.contains("1 accounts"), "{text:?}");
    }

    #[test]
    fn compact_global_panel_omits_an_unrepresentable_account_total() {
        let now = 1_000_000;
        let mut summary = global_summary(now);
        summary.included_accounts = usize::MAX;
        summary.excluded_accounts = 1;

        let text = super::compact_global_weekly_line(&summary, None, now, 120).to_string();

        assert!(!text.contains("accounts"), "{text:?}");
        assert!(text.contains("Next reset: work2 in 3h18m"), "{text:?}");
        assert!(!text.contains("equal"), "{text:?}");
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
        let now = TEST_NOW;
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
            fetched_at: Some(now),
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
        terminal.draw(|frame| render(frame, &mut app, now)).unwrap();
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
                    resets_at: Some(TEST_NOW + 6 * 24 * 60 * 60),
                    window_minutes: Some(7 * 24 * 60),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_usage_gauges(frame, &usage, frame.area(), TEST_NOW))
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
            &["oai001@example.com"],
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
