//! Test-only helper that exercises the real selection pipeline against a
//! synthetic transcript, mirroring how the fullscreen host wires drag events
//! into `TuiAltScreen` (layout → mouse events → extraction).

use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use rpi_tui::scroll_view::{FollowMode, ScrollView, ScrollViewOptions};
use rpi_tui::{Container, Text, TuiAltScreen};

/// Screen geometry a probe drags over.
#[derive(Clone, Copy)]
pub(super) struct SelectionGeometry {
    pub width: usize,
    pub rows: usize,
}

/// Terminal that reports a fixed size and records every write into a shared
/// buffer the probe reads back.
struct RecordingTerminal {
    size: (usize, usize),
    output: Arc<std::sync::Mutex<String>>,
}

impl rpi_tui::terminal::Terminal for RecordingTerminal {
    fn info(&self) -> rpi_tui::terminal::TerminalInfo {
        rpi_tui::terminal::TerminalInfo {
            columns: self.size.0,
            rows: self.size.1,
            ..Default::default()
        }
    }
    fn write(&self, data: &str) {
        self.output.lock().unwrap().push_str(data);
    }
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _row: usize, _col: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _title: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn start(
        &self,
        _on_input: Box<dyn Fn(rpi_tui::terminal::InputEvent) + Send + Sync>,
        _on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
    }
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, _active: bool) {}
    fn flush(&self) {}
}

struct Probe {
    tui: Arc<TuiAltScreen>,
    output: Arc<std::sync::Mutex<String>>,
}

impl Probe {
    fn build(long_line: &str, tail_line: &str, geometry: SelectionGeometry) -> Self {
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new(long_line, 0, 0)));
        doc.add_child(Arc::new(Text::new(tail_line, 0, 0)));
        let view = Arc::new(ScrollView::new(
            Arc::new(doc) as Arc<dyn rpi_tui::component::Component>,
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let recorder = RecordingTerminal {
            size: (geometry.width, geometry.rows),
            output: output.clone(),
        };
        let tui = Arc::new(TuiAltScreen::new(Box::new(recorder), false, None));
        tui.set_layout_root(Some(view as Arc<dyn rpi_tui::component::Component>));
        // start_readerless paints the first frame inline.
        tui.start_readerless();
        Self { tui, output }
    }

    fn mouse(&self, kind: MouseEventKind, column: u16, row: u16) {
        self.tui.handle_selection_mouse_event(&MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
    }

    fn frame(&self) -> String {
        self.output.lock().unwrap().clone()
    }
}

/// Drag rows 0..=1 of the first message and return the extracted text.
pub(super) fn probe_extract_at_width(
    long_line: &str,
    tail_line: &str,
    geometry: SelectionGeometry,
) -> Option<String> {
    let probe = Probe::build(long_line, tail_line, geometry);
    probe.mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
    probe.mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1);
    probe.mouse(MouseEventKind::Up(MouseButton::Left), 10, 1);
    probe.tui.selection_text()
}

/// A plain click (Down+Up at one cell, no Drag): returns the terminal output
/// (to assert no reverse-video reached the screen) and the selection text
/// (must be None — a click never selects nor copies).
pub(super) fn probe_click(
    long_line: &str,
    tail_line: &str,
    geometry: SelectionGeometry,
) -> (String, Option<String>) {
    let probe = Probe::build(long_line, tail_line, geometry);
    let before = probe.frame();
    probe.mouse(MouseEventKind::Down(MouseButton::Left), 2, 0);
    probe.mouse(MouseEventKind::Up(MouseButton::Left), 2, 0);
    let frame = probe.frame();
    let text = probe.tui.selection_text();
    (frame[divergence(&frame, &before)..].to_string(), text)
}

/// A real drag: returns the terminal output written after the Drag re-render
/// (to assert the highlight DID reach the screen) and the extracted text.
pub(super) fn probe_drag(
    long_line: &str,
    tail_line: &str,
    geometry: SelectionGeometry,
) -> (String, Option<String>) {
    let probe = Probe::build(long_line, tail_line, geometry);
    probe.mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
    let before = probe.frame();
    probe.mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1);
    let frame = probe.frame();
    let text = probe.tui.selection_text();
    (frame[divergence(&frame, &before)..].to_string(), text)
}

/// Byte offset where `after` diverges from `before` (recorded output is
/// append-only in these probes).
fn divergence(after: &str, before: &str) -> usize {
    after
        .as_bytes()
        .iter()
        .zip(before.as_bytes())
        .take_while(|(a, b)| a == b)
        .count()
        .min(after.len())
}
