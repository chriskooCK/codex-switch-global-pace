use ratatui::{
    style::Style,
    text::{Line, Span},
};

/// Map a percentage on a 0..100 meter to its marker cell.
/// Zero is the first cell and 100 is the last cell.
pub(super) fn percent_marker_offset(percent: f64, width: u16) -> Option<u16> {
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

pub(super) fn meter_fill_width(percent: f64, width: u16) -> u16 {
    debug_assert!(percent.is_finite());
    // Keep a small nonzero usage segment visible instead of rounding it away.
    ((percent.clamp(0.0, 100.0) / 100.0) * f64::from(width))
        .ceil()
        .clamp(0.0, f64::from(width)) as u16
}

pub(super) fn usage_meter_line(
    fill_percent: Option<f64>,
    marker_offset: Option<u16>,
    width: u16,
    fill_style: Style,
    remaining_style: Style,
    marker_style: Style,
) -> Line<'static> {
    let fill_width = match fill_percent {
        Some(percent) => meter_fill_width(percent, width),
        None => 0,
    };
    let mut spans = Vec::new();

    if let Some(marker) = marker_offset.filter(|marker| *marker < width) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_geometry_uses_the_full_meter_domain() {
        assert_eq!(percent_marker_offset(50.0, 0), None);
        assert_eq!(percent_marker_offset(f64::NAN, 5), None);
        assert_eq!(percent_marker_offset(0.0, 1), Some(0));
        assert_eq!(percent_marker_offset(100.0, 1), Some(0));
        assert_eq!(percent_marker_offset(-10.0, 5), Some(0));
        assert_eq!(percent_marker_offset(50.0, 5), Some(2));
        assert_eq!(percent_marker_offset(125.0, 5), Some(4));
    }

    #[test]
    fn nonzero_usage_keeps_at_least_one_visible_cell() {
        assert_eq!(meter_fill_width(0.0, 5), 0);
        assert_eq!(meter_fill_width(0.01, 5), 1);
        assert_eq!(meter_fill_width(50.0, 5), 3);
        assert_eq!(meter_fill_width(99.99, 5), 5);
        assert_eq!(meter_fill_width(125.0, 5), 5);
    }

    #[test]
    fn missing_usage_renders_an_empty_meter_without_inventing_a_percentage() {
        let line = usage_meter_line(
            None,
            None,
            5,
            Style::default(),
            Style::default(),
            Style::default(),
        );
        assert_eq!(line.to_string(), "░░░░░");
    }
}
