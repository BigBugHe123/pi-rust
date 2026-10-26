//! Text selection for the fullscreen (alternate-screen) TUI.
//!
//! Mirrors upstream pi's `tui-alt-screen.ts` selection model: an anchor and a
//! focus live in *content* coordinates (row within the scroll view's cached
//! document, column within that row) while the drag stays inside the scroll
//! view, so a selection keeps pointing at the same text while the transcript
//! scrolls underneath. Points outside any scroll view fall back to raw screen
//! coordinates (upstream's non-scroll path); the two spaces never mix — a
//! selection anchored in one space ignores motion resolved into the other.
//!
//! Highlighting is applied at render time by reversing video over the selected
//! column span; extraction slices the cached document rows by visible column,
//! so what lands on the clipboard is exactly what was highlighted.

use std::sync::{Arc, Mutex};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::ansi::{strip_ansi, visible_width};
use crate::layout::{LayoutBox, LayoutFrame, LayoutRect};
use crate::line_diff;
use crate::scroll_view::ScrollView;

/// One endpoint of a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPoint {
    /// Row within the scroll view's document (`in_scroll`), or the screen row.
    pub row: usize,
    /// Column within that row.
    pub col: usize,
    /// Whether this point was resolved inside the anchored scroll view.
    pub in_scroll: bool,
}

/// The active selection: where the drag started and where it currently ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub anchor: SelectionPoint,
    pub focus: SelectionPoint,
}

/// Selection state owned by the fullscreen TUI.
#[derive(Default)]
pub struct SelectionState {
    selection: Mutex<Option<Selection>>,
    /// The scroll view the drag was anchored in, if any. Drag and release
    /// events resolve their position against THIS view (clamped to its
    /// viewport), not against whatever view sits under the pointer now —
    /// otherwise a drag that crosses the dock would jump coordinate spaces.
    anchor_scroll: Mutex<Option<Arc<ScrollView>>>,
    /// Button is currently held (a `Down` without its `Up` yet).
    press_active: Mutex<bool>,
    /// The pointer moved with the button held. Only a real drag selects and
    /// copies; a plain click collapses the selection.
    dragged: Mutex<bool>,
}

impl SelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current selection, if any.
    pub fn selection(&self) -> Option<Selection> {
        self.selection.lock().ok().and_then(|s| s.clone())
    }

    pub fn clear(&self) {
        if let Ok(mut sel) = self.selection.lock() {
            *sel = None;
        }
        if let Ok(mut view) = self.anchor_scroll.lock() {
            *view = None;
        }
    }

    /// Whether the completed press was a real drag (not a plain click).
    pub fn dragged(&self) -> bool {
        self.dragged.lock().map(|d| *d).unwrap_or(false)
    }

    /// Feed a left-button mouse event. Returns true when selection state
    /// changed and the caller should re-render.
    pub fn handle_mouse_event(
        &self,
        event: &MouseEvent,
        frame: &LayoutFrame,
        screen_rows: usize,
        screen_cols: usize,
    ) -> bool {
        let is_left = matches!(
            event.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        );
        if !is_left {
            return false;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (point, view) = resolve_point(
                    event.column,
                    event.row,
                    frame,
                    screen_rows,
                    screen_cols,
                    None,
                );
                *self.anchor_scroll.lock().unwrap() = view;
                *self.press_active.lock().unwrap() = true;
                *self.dragged.lock().unwrap() = false;
                *self.selection.lock().unwrap() = Some(Selection {
                    anchor: point.clone(),
                    focus: point,
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !*self.press_active.lock().unwrap() {
                    return false;
                }
                *self.dragged.lock().unwrap() = true;
                self.move_focus(event, frame, screen_rows, screen_cols)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if !*self.press_active.lock().unwrap() {
                    return false;
                }
                *self.press_active.lock().unwrap() = false;
                let changed = self.move_focus(event, frame, screen_rows, screen_cols);
                if !*self.dragged.lock().unwrap() {
                    // A plain click is not a selection: collapse it.
                    self.clear();
                    return true;
                }
                changed
            }
            _ => false,
        }
    }

    /// Move the selection focus to the event's position (clamped to the
    /// anchored scroll view's viewport when the drag started inside it).
    fn move_focus(
        &self,
        event: &MouseEvent,
        frame: &LayoutFrame,
        screen_rows: usize,
        screen_cols: usize,
    ) -> bool {
        let anchor_view = self.anchor_scroll.lock().unwrap().clone();
        let (point, _) = resolve_point(
            event.column,
            event.row,
            frame,
            screen_rows,
            screen_cols,
            anchor_view.as_ref(),
        );
        let mut sel = self.selection.lock().unwrap();
        match sel.as_mut() {
            Some(sel) if sel.focus != point => {
                sel.focus = point;
                true
            }
            _ => false,
        }
    }

    /// Ordered (start, end) bounds of the selection, or None when the
    /// selection is empty or its endpoints landed in different coordinate
    /// spaces (which cannot be ordered against each other).
    pub fn bounds(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let sel = self.selection()?;
        if sel.anchor == sel.focus || sel.anchor.in_scroll != sel.focus.in_scroll {
            return None;
        }
        let anchor_before = sel.anchor.row < sel.focus.row
            || (sel.anchor.row == sel.focus.row && sel.anchor.col < sel.focus.col);
        if anchor_before {
            Some((sel.anchor.clone(), sel.focus.clone()))
        } else {
            Some((sel.focus.clone(), sel.anchor.clone()))
        }
    }
}

/// Resolve a screen cell to a selection point.
///
/// When `anchor_view` is given (drag/release), the point is clamped into that
/// view's visible viewport — a drag that runs past the transcript edge keeps
/// selecting the first/last visible content row. Without an anchor (press),
/// the view under the pointer wins; a cell outside every scroll view resolves
/// to raw screen coordinates.
fn resolve_point(
    column: u16,
    row: u16,
    frame: &LayoutFrame,
    screen_rows: usize,
    screen_cols: usize,
    anchor_view: Option<&Arc<ScrollView>>,
) -> (SelectionPoint, Option<Arc<ScrollView>>) {
    if let Some(view) = anchor_view {
        if let Some(point) = scroll_point(view, column, row, frame) {
            return (point, Some(view.clone()));
        }
    } else {
        for view in crate::layout::get_scroll_views_at(frame, column as usize, row as usize) {
            if let Some(point) = scroll_point(&view, column, row, frame) {
                return (point, Some(view));
            }
        }
    }
    (
        SelectionPoint {
            row: (row as usize).min(screen_rows.saturating_sub(1)),
            col: (column as usize).min(screen_cols.saturating_sub(1)),
            in_scroll: false,
        },
        None,
    )
}

/// Map a screen cell into `view`'s content coordinates.
fn scroll_point(
    view: &Arc<ScrollView>,
    column: u16,
    row: u16,
    frame: &LayoutFrame,
) -> Option<SelectionPoint> {
    let (rect, clip) = find_scroll_box(&frame.root, view)?;
    let (x, y) = (column as usize, row as usize);
    if x < rect.x || x >= rect.x + rect.width || rect.width == 0 {
        return None;
    }
    let visible_top = rect.y.max(clip.y);
    let visible_bottom = (rect.y + rect.height).min(clip.y + clip.height);
    if visible_bottom <= visible_top {
        return None;
    }
    let pointer_row = y.clamp(visible_top, visible_bottom - 1);
    let scroll_top = view.scroll_top();
    let max_content_row = view.content_height().saturating_sub(1);
    Some(SelectionPoint {
        row: (scroll_top + pointer_row - rect.y).min(max_content_row),
        col: (x - rect.x).min(rect.width - 1),
        in_scroll: true,
    })
}

/// Locate `view`'s box (rect + clip) in the frame. Layout passes clone the
/// `ScrollView` into a fresh `Arc`, so identity is the shared scroll state.
fn find_scroll_box(lbox: &LayoutBox, view: &Arc<ScrollView>) -> Option<(LayoutRect, LayoutRect)> {
    if let Some(ref sv) = lbox.scroll_view {
        if sv.same_state(view) {
            return Some((lbox.rect, lbox.clip));
        }
    }
    for child in &lbox.children {
        if let Some(found) = find_scroll_box(child, view) {
            return Some(found);
        }
    }
    None
}

/// Reverse-video the selected span on the composited screen lines.
///
/// Applied after overlays and cursor-marker stripping, immediately before the
/// frame is diffed and written, exactly where upstream applies `applySelection`.
pub fn apply_selection_highlight(
    lines: &mut [String],
    selection: &SelectionState,
    frame: &LayoutFrame,
) {
    let Some((start, end)) = selection.bounds() else {
        return;
    };
    let (start, end, min_row, max_row, min_col, max_col) = if start.in_scroll {
        let view = match selection.anchor_scroll_view() {
            Some(view) => view,
            None => return,
        };
        let Some((rect, clip)) = find_scroll_box(&frame.root, &view) else {
            return;
        };
        let scroll_top = view.scroll_top();
        // Content coordinates → screen coordinates for this frame.
        let map = |p: &SelectionPoint| (rect.y + p.row.saturating_sub(scroll_top), rect.x + p.col);
        let (srow, scol) = map(&start);
        let (erow, ecol) = map(&end);
        (
            SelectionPoint {
                row: srow,
                col: scol,
                in_scroll: false,
            },
            SelectionPoint {
                row: erow,
                col: ecol,
                in_scroll: false,
            },
            rect.y.max(clip.y),
            (rect.y + rect.height)
                .saturating_sub(1)
                .min(clip.y + clip.height - 1),
            rect.x.max(clip.x),
            (rect.x + rect.width).min(clip.x + clip.width),
        )
    } else {
        (start, end, 0, lines.len().saturating_sub(1), 0, usize::MAX)
    };

    for (row, line) in lines.iter_mut().enumerate() {
        if row < min_row || row > max_row || row < start.row || row > end.row {
            continue;
        }
        if is_image_line(line) {
            continue;
        }
        let line_width = visible_width(line);
        let mut col_start = if row == start.row { start.col } else { 0 };
        let mut col_end = if row == end.row { end.col } else { line_width };
        col_start = col_start.min(line_width).max(min_col);
        col_end = col_end.min(line_width).max(col_start).min(max_col);
        if col_end <= col_start {
            continue;
        }
        if let Some(highlighted) = reverse_video_span(line, col_start, col_end) {
            *line = highlighted;
        }
    }
}

/// Extract the selected text as plain lines.
///
/// Scroll-space selections slice the scroll view's cached document rows —
/// the same rows the frame was painted from — by visible column. Screen-space
/// selections slice the frame's composited lines. Slicing (not indexing
/// characters) keeps wide graphemes and skipped padding columns honest.
pub fn extract_selection_text(selection: &SelectionState, frame: &LayoutFrame) -> Option<String> {
    let (start, end) = selection.bounds()?;
    let source: Vec<String> = if start.in_scroll {
        let view = selection.anchor_scroll_view()?;
        let content_width = view.get_content_width(frame.width);
        view.content_lines(content_width)?
    } else {
        frame.lines.clone()
    };
    if source.is_empty() {
        return None;
    }
    let max_row = source.len().saturating_sub(1);
    let start_row = start.row.min(max_row);
    let end_row = end.row.min(max_row);
    if start_row > end_row {
        return None;
    }
    let mut extracted = Vec::with_capacity(end_row - start_row + 1);
    for (row, line) in source.iter().enumerate().take(end_row + 1).skip(start_row) {
        let line_width = visible_width(line);
        let mut col_start = if row == start_row { start.col } else { 0 };
        let mut col_end = if row == end_row { end.col } else { line_width };
        col_start = col_start.min(line_width);
        col_end = col_end.min(line_width).max(col_start);
        let span = if col_end > col_start {
            line_diff::slice(line, col_start, col_end - col_start)
                .map(|text| strip_ansi(&text))
                .unwrap_or_default()
        } else {
            String::new()
        };
        extracted.push(span.trim_end().to_string());
    }
    let text = extracted.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Wrap a column span in reverse video, re-asserting it after every SGR run
/// inside the span so an embedded reset (`\x1b[0m`) cannot cancel the
/// highlight (upstream `applySelectionHighlight`).
fn reverse_video_span(line: &str, start_col: usize, end_col: usize) -> Option<String> {
    let sliced = line_diff::slice(line, start_col, end_col - start_col)?;
    let mut result = String::from("\x1b[7m");
    let mut rest = sliced.as_str();
    while let Some(escape_at) = rest.find('\x1b') {
        let (plain, tail) = rest.split_at(escape_at);
        result.push_str(plain);
        // Copy the complete escape sequence (CSI to final byte, OSC to
        // BEL/ST, or a two-char escape).
        let mut end = tail.len().min(2);
        if tail.starts_with("\x1b[") {
            end = tail
                .bytes()
                .enumerate()
                .skip(2)
                .find(|(_, b)| (0x40..=0x7e).contains(b))
                .map(|(i, _)| i + 1)
                .unwrap_or(tail.len());
        } else if tail.starts_with("\x1b]") {
            end = tail
                .find('\x07')
                .map(|i| i + 1)
                .or_else(|| tail.find("\x1b\\").map(|i| i + 2))
                .unwrap_or(tail.len());
        }
        let (sequence, next) = tail.split_at(end);
        result.push_str(sequence);
        if sequence.ends_with('m') {
            result.push_str("\x1b[7m");
        }
        rest = next;
    }
    result.push_str(rest);
    result.push_str("\x1b[27m");
    Some(result)
}

fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

impl SelectionState {
    /// The scroll view this selection is anchored in, if any. Public for
    /// extraction/highlight mapping within the crate.
    pub(crate) fn anchor_scroll_view(&self) -> Option<Arc<ScrollView>> {
        self.anchor_scroll.lock().ok().and_then(|v| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::render_layout_frame;
    use crate::scroll_view::{FollowMode, ScrollViewOptions};
    use crate::{Container, Text};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// A transcript document long enough to scroll: 30 numbered rows in a
    /// 12-row scroll viewport.
    fn scrolled_frame() -> (Arc<ScrollView>, LayoutFrame) {
        let doc = Container::new();
        for i in 0..30 {
            doc.add_child(Arc::new(Text::new(format!("line {:02} content", i), 0, 0)));
        }
        let view = Arc::new(ScrollView::new(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            ScrollViewOptions {
                follow: FollowMode::End,
                primary: true,
                ..Default::default()
            },
        ));
        let frame = render_layout_frame(view.clone(), 40, 12);
        (view, frame)
    }

    fn drag_from_to(state: &SelectionState, frame: &LayoutFrame, from: (u16, u16), to: (u16, u16)) {
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
            frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
            frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
            frame,
            12,
            40,
        );
    }

    #[test]
    fn plain_click_collapses_and_never_selects() {
        let state = SelectionState::new();
        let (_view, frame) = scrolled_frame();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 2, 2),
            &frame,
            12,
            40,
        );
        assert!(!state.dragged());
        assert!(state.bounds().is_none());
        assert!(extract_selection_text(&state, &frame).is_none());
    }

    #[test]
    fn drag_extracts_the_highlighted_rows_by_visible_column() {
        let state = SelectionState::new();
        let (_view, frame) = scrolled_frame();
        // The scroll view fills the whole frame; content is 30 rows in a
        // 12-row viewport following the end, so screen row 0 = content row 18.
        drag_from_to(&state, &frame, (5, 0), (12, 1));
        assert!(state.dragged());
        let sel = state.bounds().expect("drag produced a selection");
        assert!(sel.0.in_scroll && sel.1.in_scroll);
        assert_eq!(sel.0.row, 18);
        assert_eq!(sel.1.row, 19);
        // Columns 5..=12: "18 content" / "line 19 cont".
        let text = extract_selection_text(&state, &frame).expect("non-empty text");
        assert_eq!(text, "18 content\nline 19 cont", "got: {text:?}");
    }

    #[test]
    fn drag_keeps_selecting_the_same_content_when_the_view_scrolls() {
        let state = SelectionState::new();
        let (view, frame) = scrolled_frame();
        // Anchor near the top of the viewport.
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        // The view scrolls up by 3 rows mid-drag…
        view.scroll_by(-3);
        // …the renderer lays out the new position…
        let frame = render_layout_frame(view.clone(), 40, 12);
        // …and the drag continues at the same screen cell.
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 10, 1),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 10, 1),
            &frame,
            12,
            40,
        );
        let sel = state.bounds().expect("selection survived the scroll");
        // Anchor resolved against the pre-scroll frame (content row 18); the
        // release resolves against the post-scroll frame where screen row 1
        // is content row 16. Selection covers content rows 16..=18.
        assert_eq!(sel.0.row, 16);
        assert_eq!(sel.1.row, 18);
    }

    #[test]
    fn a_drag_out_of_the_transcript_clamps_to_the_last_visible_content_row() {
        let state = SelectionState::new();
        let (_view, frame) = scrolled_frame();
        state.handle_mouse_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &frame,
            12,
            40,
        );
        // Drag past the bottom of the scroll viewport (row 11 is the last).
        state.handle_mouse_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 5, 11),
            &frame,
            12,
            40,
        );
        state.handle_mouse_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 5, 11),
            &frame,
            12,
            40,
        );
        let sel = state.bounds().expect("clamped selection");
        assert_eq!(sel.1.row, 29, "clamped to the last content row");
    }

    #[test]
    fn selection_outside_any_scroll_view_uses_screen_coordinates() {
        let state = SelectionState::new();
        // A plain container root: no scroll view anywhere.
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("alpha beta", 0, 0)));
        doc.add_child(Arc::new(Text::new("gamma delta", 0, 0)));
        let frame = render_layout_frame(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            40,
            12,
        );
        drag_from_to(&state, &frame, (0, 0), (5, 1));
        let sel = state.bounds().expect("screen-space selection");
        assert!(!sel.0.in_scroll && !sel.1.in_scroll);
        let text = extract_selection_text(&state, &frame).expect("text");
        // Columns 0..=5 of row 0, 0..=5 of row 1 (trailing blanks trimmed).
        assert_eq!(text, "alpha beta\ngamma", "got: {text:?}");
    }

    #[test]
    fn reverse_video_wraps_the_span_and_survives_embedded_resets() {
        let line = "\x1b[31mred\x1b[0m plain";
        let highlighted = reverse_video_span(line, 0, visible_width(line)).expect("sliceable line");
        assert!(highlighted.starts_with("\x1b[7m"));
        assert!(highlighted.ends_with("\x1b[27m"));
        // The embedded reset must be followed by a re-asserted inverse.
        let reset = highlighted.find("\x1b[0m").expect("reset preserved");
        assert!(
            highlighted[reset + 4..].starts_with("\x1b[7m"),
            "inverse re-asserted after the reset: {highlighted:?}"
        );
    }

    #[test]
    fn image_lines_are_never_highlighted() {
        let state = SelectionState::new();
        let doc = Container::new();
        doc.add_child(Arc::new(Text::new("above", 0, 0)));
        doc.add_child(Arc::new(Text::new("\x1b_Gd=7;abc\x1b\\caption", 0, 0)));
        let frame = render_layout_frame(
            Arc::new(doc) as Arc<dyn crate::component::Component>,
            40,
            12,
        );
        drag_from_to(&state, &frame, (0, 0), (10, 2));
        let mut lines = frame.lines.clone();
        apply_selection_highlight(&mut lines, &state, &frame);
        // The kitty line must come through untouched.
        assert!(lines[1].contains("\x1b_G") && !lines[1].contains("\x1b[7m"));
        // The plain line above IS highlighted.
        assert!(lines[0].contains("\x1b[7m"));
    }
}
