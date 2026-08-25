/// Generic popup rendering with screen-size adaptation.
///
/// Centers a bordered box on screen, clamps to terminal bounds,
/// and supports vertical scrolling when content exceeds available height.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::safe_text;

const BG: Color = Color::Rgb(24, 24, 24);
const C_WHITE: Color = Color::Rgb(240, 240, 240);
const DIM: Color = Color::Rgb(120, 120, 120);
const C_CYAN: Color = Color::Rgb(100, 210, 255);

/// Minimum terminal size below which we abort popup rendering.
const MIN_TERM_W: u16 = 20;
const MIN_TERM_H: u16 = 6;

fn display_width(value: &str) -> u16 {
    u16::try_from(Span::raw(value).width()).unwrap_or(u16::MAX)
}

pub struct PopupState {
    pub scroll: u16,
}

impl PopupState {
    pub const fn new() -> Self {
        Self { scroll: 0 }
    }

    pub fn scroll_down(&mut self, max: u16) {
        if self.scroll < max {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn page_down(&mut self, page: u16, max: u16) {
        self.scroll = self.scroll.saturating_add(page).min(max);
    }

    pub fn page_up(&mut self, page: u16) {
        self.scroll = self.scroll.saturating_sub(page);
    }

    pub fn reset(&mut self) {
        self.scroll = 0;
    }
}

/// Render a popup with `lines` content, centered on screen.
///
/// - `title` shown in border
/// - `lines` plain text lines (already styled if needed via Line)
/// - `state` for scroll offset (use a fresh state for non-scrolling popups)
///
/// If terminal is too small, renders a single-line fallback at the bottom
/// of `screen` instead of the popup.
///
/// Returns the inner content area width (so callers can do their own
/// truncation if needed); caller may ignore.
pub fn render_popup(
    f: &mut Frame,
    title: &str,
    lines: &[Line<'_>],
    state: &mut PopupState,
    screen: Rect,
) {
    if screen.width < MIN_TERM_W || screen.height < MIN_TERM_H {
        render_too_small_fallback(f, screen);
        return;
    }

    let title = safe_text::terminal_text(title);
    let lines: Vec<Line<'static>> = lines.iter().map(sanitized_line).collect();

    // Measure width first; height is calculated after wrapping to the popup's
    // actual inner display width.
    let content_w = lines
        .iter()
        .map(Line::width)
        .max()
        .map(|width| u16::try_from(width).unwrap_or(u16::MAX))
        .unwrap_or(0);

    let title_w = display_width(title.as_ref()).saturating_add(4); // "─ title ─" + corners
    let needed_w = content_w.saturating_add(4).max(title_w); // 2 border + 2 padding
    // Clamp to screen, leaving 2 cols / 1 row margin where possible
    let max_w = screen.width.saturating_sub(2).max(MIN_TERM_W);
    let max_h = screen.height.saturating_sub(2).max(MIN_TERM_H);
    let w = needed_w.min(max_w);
    // 2 border cells and 2 explicit padding cells.
    let usable_w = w.saturating_sub(4) as usize;
    let wrapped: Vec<Line<'static>> = lines
        .iter()
        .flat_map(|line| wrap_line(line, usable_w))
        .collect();
    let content_h = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);
    let needed_h = content_h.saturating_add(2); // 2 border
    let h = needed_h.min(max_h);

    let x = screen.x + screen.width.saturating_sub(w) / 2;
    let y = screen.y + screen.height.saturating_sub(h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_CYAN).bg(BG))
        .style(Style::default().bg(BG).fg(C_WHITE));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Inner usable area accounting for 1-col left/right padding
    let pad_left = 1u16;
    let pad_right = 1u16;
    let usable_w = inner.width.saturating_sub(pad_left + pad_right);
    let visible_h = inner.height;

    let total_lines = wrapped.len() as u16;
    let scrollable = total_lines > visible_h;
    let max_scroll = total_lines.saturating_sub(visible_h);
    let scroll = state.scroll.min(max_scroll);
    state.scroll = scroll; // clamp persisted scroll to actual content bounds

    let visible_slice: &[Line<'static>] = if scrollable {
        let start = scroll as usize;
        let end = (start + visible_h as usize).min(wrapped.len());
        &wrapped[start..end]
    } else {
        &wrapped[..]
    };

    let content_area = Rect {
        x: inner.x + pad_left,
        y: inner.y,
        width: usable_w,
        height: visible_h,
    };
    f.render_widget(
        Paragraph::new(visible_slice.to_vec()).style(Style::default().bg(BG)),
        content_area,
    );

    // Scrollbar on right edge inside border
    if scrollable && inner.width >= 1 && visible_h > 0 {
        render_scrollbar(f, inner, scroll, max_scroll, visible_h, total_lines);
    }
}

fn sanitized_line(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| {
                Span::styled(
                    safe_text::terminal_text(span.content.as_ref()).into_owned(),
                    span.style,
                )
            })
            .collect(),
    }
}

fn render_scrollbar(
    f: &mut Frame,
    inner: Rect,
    scroll: u16,
    max_scroll: u16,
    visible_h: u16,
    total_lines: u16,
) {
    let bar_x = inner.x + inner.width.saturating_sub(1);
    let bar_h = visible_h;
    if bar_h == 0 || total_lines == 0 {
        return;
    }

    // Thumb height proportional to visible/total
    let thumb_h = ((bar_h as f64 * visible_h as f64 / total_lines as f64).round() as u16)
        .max(1)
        .min(bar_h);
    let thumb_pos = if max_scroll == 0 {
        0
    } else {
        ((bar_h.saturating_sub(thumb_h)) as f64 * scroll as f64 / max_scroll as f64).round() as u16
    };

    // Track
    for i in 0..bar_h {
        let cell_y = inner.y + i;
        let in_thumb = i >= thumb_pos && i < thumb_pos + thumb_h;
        let (ch, style) = if in_thumb {
            ("\u{2588}", Style::default().fg(C_CYAN).bg(BG)) // █
        } else {
            ("\u{258C}", Style::default().fg(DIM).bg(BG)) // ▌ (subtle track)
        };
        let area = Rect {
            x: bar_x,
            y: cell_y,
            width: 1,
            height: 1,
        };
        f.render_widget(Paragraph::new(Span::styled(ch, style)), area);
    }
}

fn render_too_small_fallback(f: &mut Frame, screen: Rect) {
    let msg = "Screen too small — resize terminal";
    let h = 1u16;
    let y = screen.y + screen.height.saturating_sub(h);
    let area = Rect {
        x: screen.x,
        y,
        width: screen.width,
        height: h,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(msg).style(
            Style::default()
                .fg(Color::Rgb(255, 90, 90))
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

/// Wrap a styled line by terminal display cells without splitting Unicode
/// grapheme clusters or dropping content.
pub(crate) fn wrap_line(line: &Line<'_>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![Line::from(Span::raw(""))];
    }

    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        for grapheme in span.styled_graphemes(Style::default()) {
            let grapheme_width = Span::raw(grapheme.symbol).width();
            if used > 0 && used + grapheme_width > max_width {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            current.push(Span::styled(grapheme.symbol.to_string(), grapheme.style));
            used += grapheme_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn wrap_line_returns_one_line_when_shorter_or_exact_width() {
        let short = Line::from("abc");
        let exact = Line::from("abcd");

        let short = wrap_line(&short, 4);
        let exact = wrap_line(&exact, 4);
        assert_eq!(short.len(), 1);
        assert_eq!(exact.len(), 1);
        assert_eq!(content(&short[0]), "abc");
        assert_eq!(content(&exact[0]), "abcd");
    }

    #[test]
    fn wrap_line_handles_cjk_and_emoji_by_display_width_without_loss() {
        let wrapped = wrap_line(&Line::from("中🙂abc"), 4);

        assert!(wrapped.iter().all(|line| line.width() <= 4), "{wrapped:?}");
        assert_eq!(wrapped.iter().map(content).collect::<String>(), "中🙂abc");
    }

    #[test]
    fn wrap_line_keeps_unicode_grapheme_clusters_intact() {
        let heart = wrap_line(&Line::from("❤️a"), 2);
        let developer = wrap_line(&Line::from("👩‍💻ab"), 3);

        assert!(heart.iter().all(|line| line.width() <= 2), "{heart:?}");
        assert_eq!(heart.iter().map(content).collect::<String>(), "❤️a");
        assert_eq!(developer.iter().map(content).collect::<String>(), "👩‍💻ab");
        assert!(developer.iter().all(|line| line.width() <= 3));
    }

    #[test]
    fn wrap_line_preserves_content_and_style_across_spans() {
        let line = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::styled("cdef", Style::default().fg(Color::Blue)),
        ]);

        let wrapped = wrap_line(&line, 4);

        assert_eq!(wrapped.iter().map(content).collect::<String>(), "abcdef");
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrapped[0].spans[2].style.fg, Some(Color::Blue));
    }

    #[test]
    fn popup_sanitization_preserves_styles_and_unicode_display_width() {
        let line = Line::from(vec![
            Span::styled(
                "계정\u{1b}]52;clipboard\u{7}",
                Style::default().fg(Color::Red),
            ),
            Span::styled("🙂", Style::default().fg(Color::Blue)),
        ]);

        let sanitized = sanitized_line(&line);

        assert_eq!(content(&sanitized), "계정]52;clipboard🙂");
        assert_eq!(sanitized.spans[0].style.fg, Some(Color::Red));
        assert_eq!(sanitized.spans[1].style.fg, Some(Color::Blue));
        assert_eq!(sanitized.width(), Line::from("계정]52;clipboard🙂").width());
    }

    #[test]
    fn popup_title_width_uses_terminal_cells_instead_of_utf8_bytes() {
        assert_eq!(display_width("계정🙂"), 6);
        assert_eq!(display_width("ASCII"), 5);
    }

    #[test]
    fn long_cjk_account_detail_wraps_instead_of_being_ellipsized() {
        let title = "한".repeat(40);
        let line = Line::from(vec![
            Span::styled("organization  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{title} · Member"),
                Style::default().fg(Color::White),
            ),
        ]);

        // An 80-column screen leaves 74 display cells after popup margin,
        // borders, and padding.
        let wrapped = wrap_line(&line, 74);

        assert!(wrapped.len() >= 2);
        assert!(wrapped.iter().all(|line| line.width() <= 74));
        let combined = wrapped.iter().map(content).collect::<String>();
        assert!(combined.contains(&title));
        assert!(combined.ends_with(" · Member"));
    }
}
