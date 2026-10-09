use ratatui::{Frame, Terminal, backend::Backend};

/// Capture wheel gestures on macOS while the alternate screen is active.
/// Otherwise Terminal.app can scroll the dashboard into its main-screen history.
pub(super) fn init() -> ratatui::DefaultTerminal {
    let terminal = ratatui::init();
    #[cfg(target_os = "macos")]
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
        .expect("enabling dashboard mouse input");
    terminal
}

pub(super) fn restore() {
    #[cfg(target_os = "macos")]
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
}

/// Draw without cursor queries or display erasure during IME recovery.
pub(super) fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    repaint: bool,
    render: impl FnOnce(&mut Frame),
) -> Result<(), B::Error> {
    terminal.draw(|frame| {
        // A hidden cursor can prevent macOS IMEs from tracking the composition
        // anchor. Keep a real cursor visible, away from the bottom edge. An
        // editor may override this default while rendering its input field.
        if cfg!(target_os = "macos") && !frame.area().is_empty() {
            frame.set_cursor_position(frame.area().as_position());
        }
        render(frame);
        if repaint {
            // terminal.clear() queries the physical cursor and erases the
            // display. Neither is needed to recover an IME-damaged frame.
            // Force cell output instead, including unchanged blank cells.
            for cell in &mut frame.buffer_mut().content {
                if cell.diff_option == ratatui::buffer::CellDiffOption::None {
                    cell.set_diff_option(ratatui::buffer::CellDiffOption::AlwaysUpdate);
                }
            }
        }
    })?;
    if !cfg!(target_os = "macos") {
        terminal.set_cursor_position((0, 0))?;
        terminal.backend_mut().flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::draw;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect, widgets::Paragraph};

    #[test]
    fn ime_anchor_stays_visible_on_macos_after_redraw_and_resize() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for (width, height) in [(80, 24), (40, 10), (100, 30)] {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            for _ in 0..2 {
                // The previous status-bar draw can leave the physical cursor
                // here; a redraw must restore the stable IME anchor.
                terminal
                    .set_cursor_position((width - 1, height - 1))
                    .unwrap();
                draw(&mut terminal, false, |frame| {
                    frame.render_widget(
                        Paragraph::new("status"),
                        Rect::new(0, height - 1, width, 1),
                    );
                })
                .unwrap();
                terminal.backend_mut().assert_cursor_position((0, 0));
                assert_eq!(
                    terminal.backend().cursor_visible(),
                    cfg!(target_os = "macos")
                );
                terminal.backend().assert_scrollback_empty();
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_output_tests {
    use super::draw;
    use ratatui::{
        Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect,
        widgets::Paragraph,
    };
    use std::{
        cell::RefCell,
        io::{self, Write},
        rc::Rc,
    };

    #[derive(Clone, Default)]
    struct Capture(Rc<RefCell<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn repeated_ime_repaints_emit_no_cursor_queries_erase_or_hide_sequences() {
        let capture = Capture::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(capture.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 120, 30)),
            },
        )
        .unwrap();
        for repaint in [false, true, true, true] {
            capture.0.borrow_mut().clear();
            draw(&mut terminal, repaint, |frame| {
                frame.render_widget(Paragraph::new("FRAME 한글"), frame.area());
            })
            .unwrap();
            let output = String::from_utf8(capture.0.borrow().clone()).unwrap();
            for text in ["FRAME", "한", "글"] {
                assert!(output.contains(text), "missing repainted text: {text}");
            }
            if repaint {
                assert!(
                    output.contains("\x1b[30;1H"),
                    "unchanged blank rows must repaint"
                );
            }
            for forbidden in ["\x1b[6n", "\x1b[2J", "\x1b[J", "\x1b[?25l"] {
                assert!(
                    !output.contains(forbidden),
                    "unexpected sequence: {forbidden:?}"
                );
            }
            assert!(output.contains("\x1b[?25h"), "real cursor must be visible");
            assert!(output.ends_with("\x1b[1;1H"), "IME anchor must be stable");
        }
    }

    #[test]
    fn editor_cursor_overrides_default_ime_anchor() {
        let capture = Capture::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(capture.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        draw(&mut terminal, false, |frame| {
            frame.set_cursor_position((5, 10));
        })
        .unwrap();
        assert!(capture.0.borrow().ends_with(b"\x1b[11;6H"));
    }
}
