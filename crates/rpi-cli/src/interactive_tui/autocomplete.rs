//! Completion suggestions and editor cursor operations.

use super::*;

// ===========================================================================
// Autocomplete
// ===========================================================================

/// Refresh the autocomplete suggestion list from the current editor text +
/// cursor. Renders the suggestions into `autocomplete_container` (above the
/// editor) or clears it when there are none.
pub(super) fn refresh_autocomplete(state: &Arc<TuiState>, editor: &Arc<Editor>) {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let suggestions = state.autocomplete.get_suggestions(&text, cursor);
    render_autocomplete(state, suggestions);
}

/// Convert the editor's logical `(row, byte-column)` caret into the absolute
/// byte offset expected by autocomplete providers.
pub(super) fn editor_cursor_offset(editor: &Editor, text: &str) -> usize {
    let (row, col) = editor.cursor_position();
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == row {
            return (offset + col.min(line.len())).min(text.len());
        }
        offset = offset.saturating_add(line.len() + 1);
    }
    text.len()
}

/// Restore an editor caret from an absolute byte offset after autocomplete
/// replaces a span in a multi-line draft.
pub(super) fn set_editor_cursor_offset(editor: &Editor, text: &str, offset: usize) {
    let offset = offset.min(text.len());
    let mut consumed = 0;
    for (row, line) in text.split('\n').enumerate() {
        let end = consumed + line.len();
        if offset <= end {
            editor.set_cursor(row, offset - consumed);
            return;
        }
        consumed = end + 1;
    }
    let last_row = text.bytes().filter(|byte| *byte == b'\n').count();
    editor.set_cursor(
        last_row,
        text.rsplit('\n').next().map(str::len).unwrap_or(0),
    );
}

/// Retire the explicit-selection stamp when the popup is rebuilt for a
/// different completed span (typing, backspace, another token) or torn down.
/// Called with the key of the popup being rendered NOW (`None` when closing).
fn stamp_selection_for(state: &Arc<TuiState>, key: Option<&str>) {
    let mut stamp = state.autocomplete_selection_stamped_for.lock().unwrap();
    match (key, stamp.as_ref()) {
        // Same popup key → the user's navigation is still about this popup.
        (Some(current), Some(previous)) if current == previous => {}
        // New key or closing → the navigation intent does not carry over.
        _ => *stamp = None,
    }
}

/// Stable identity of a suggestion popup: what span it completes plus the
/// candidate list it offers. A popup that completes the same span with the
/// same candidates is "the same popup" however many times it re-renders.
fn sugg_key(sugg: &AutocompleteSuggestions) -> String {
    let items = sugg
        .items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\u{1}");
    format!("{}\u{2}{}\u{2}{}", sugg.start, sugg.end, items)
}

/// Whether the popup currently completing `sugg` is the one the user last
/// navigated — the explicit-selection stamp matches its key.
fn selection_is_explicit(state: &Arc<TuiState>, sugg: &AutocompleteSuggestions) -> bool {
    let key = sugg_key(sugg);
    state
        .autocomplete_selection_stamped_for
        .lock()
        .unwrap()
        .as_deref()
        == Some(key.as_str())
}

/// Render (or clear) the autocomplete suggestion list into the container.
pub(super) fn render_autocomplete(
    state: &Arc<TuiState>,
    suggestions: Option<AutocompleteSuggestions>,
) {
    state.autocomplete_container.clear();
    let Some(sugg) = suggestions else {
        *state.autocomplete_selection.lock().unwrap() = 0;
        stamp_selection_for(state, None);
        return;
    };
    if sugg.items.is_empty() {
        *state.autocomplete_selection.lock().unwrap() = 0;
        stamp_selection_for(state, None);
        return;
    }
    // Clamp the highlight to the full candidate list, then compute the visible
    // window (with a scroll offset) so ↓ can reach items past `max_visible`.
    let max_visible = state.autocomplete_max_visible;
    let total = sugg.items.len();
    let selection = {
        let mut sel = state.autocomplete_selection.lock().unwrap();
        *sel = (*sel).min(total.saturating_sub(1));
        *sel
    };
    let window_start = if total <= max_visible {
        0
    } else {
        selection
            .saturating_sub(max_visible / 2)
            .min(total - max_visible)
    };
    let window_end = (window_start + max_visible).min(total);
    // The rebuilt popup completes this span now; a stamp for any other span
    // belongs to a previous popup and must not survive the rebuild. The span
    // is keyed by the suggestion's own [start, end) window so typing or
    // backspacing within the token retires it, while a pure re-render (same
    // span) keeps the user's navigation.
    stamp_selection_for(state, Some(&sugg_key(&sugg)));
    // Build a compact list: the highlighted item marked with `→`, the rest `  `.
    // Cap the list so the dock doesn't swallow the transcript.
    let accent = state.theme_manager.get().colors.accent;
    let muted = state.theme_manager.get().colors.muted;
    for (i, item) in sugg
        .items
        .iter()
        .enumerate()
        .skip(window_start)
        .take(window_end - window_start)
    {
        let prefix = if i == selection { "→ " } else { "  " };
        let label = item.display_text();
        let line = if i == selection {
            format!(
                "{prefix}{} {}",
                accent.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
        } else {
            format!(
                "{prefix}{} {}",
                muted.fg(label),
                muted.fg(item.description.as_deref().unwrap_or(""))
            )
        };
        state
            .autocomplete_container
            .add_child(Arc::new(Text::new(line, 1, 0)));
    }
}

/// Accept the highlighted autocomplete suggestion: replace `text[start..end]`
/// with the suggestion text, reposition the caret, and clear the suggestion
/// list. Returns `true` if a suggestion was accepted.
pub(super) fn accept_top_suggestion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    let index = {
        let sel = *state.autocomplete_selection.lock().unwrap();
        sel.min(sugg.items.len().saturating_sub(1))
    };
    let Some(top) = sugg.items.get(index) else {
        return false;
    };
    // Replace the [start, end) span with the suggestion text. `start`/`end`
    // are byte offsets emitted by the providers on char boundaries, so the
    // `text[..start]` / `text[end..]` slices are sound for multibyte input.
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    let mut replaced = String::with_capacity(text.len() + top.text.len());
    replaced.push_str(&text[..start]);
    replaced.push_str(&top.text);
    // Keep the text AFTER the replaced span (mid-line completion: replacing
    // `[start, end)` must not drop the rest of the line).
    replaced.push_str(&text[end..]);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    // New caret position: after the inserted text (byte offset; the editor
    // snaps `set_cursor` to a char boundary as a safety net).
    let new_cursor = replaced.len().min(
        start
            + top.text.len()
            + if top.insert_space && !top.text.ends_with('/') {
                1
            } else {
                0
            },
    );
    editor.set_text(&replaced);
    set_editor_cursor_offset(editor, &replaced, new_cursor);
    state.autocomplete_container.clear();
    *state.autocomplete_selection.lock().unwrap() = 0;
    true
}

/// What a bare Enter does while a completion popup is open. Mirrors the TS
/// editor's `tui.select.confirm` handling (`editor.ts:757-815`): apply the
/// highlighted completion, then — for a `/` prefix — fall through to submit;
/// for an `@`-path completion only apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnterCompletionAction {
    /// Apply the completion and let the Enter fall through to submit.
    ApplyAndSubmit,
    /// Apply the completion but keep the Enter (a path completion must not
    /// send the prompt).
    ApplyOnly,
    /// No completion to apply — submit the text exactly as typed.
    Submit,
}

/// Decide what a bare Enter does in the current completion context.
///
/// Returns `None` when no completion popup applies (plain text, a `/` token
/// that is not at the start of the input, or no suggestions) — the key falls
/// through to the editor unchanged, preserving the upstream behavior that an
/// unknown `/skill:name` reaches the model untouched.
///
/// Interception is armed only when the choice is unambiguous: the candidate
/// list holds exactly one item, or the user has explicitly moved the
/// highlight. A fresh ambiguous popup (several candidates, index 0) resolves
/// to `Submit` — Enter sends what was typed, never a silently guessed row.
pub(super) fn enter_completion_action(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
) -> Option<EnterCompletionAction> {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let sugg = state.autocomplete.get_suggestions(&text, cursor)?;
    if sugg.items.is_empty() {
        return None;
    }
    let start = sugg.start.min(text.len());
    // Same explicit-context rule as `navigate_autocomplete`: only the `/`
    // token at input start or an `@`-file token counts as a popup the Enter
    // key belongs to.
    let explicit = text[start..].starts_with('/') || text[..start].ends_with('@');
    if !explicit {
        return None;
    }
    let is_slash = text[start..].starts_with('/');
    // Explicit selection: the user navigated THIS popup with ↑/↓. The stamp
    // (set by navigation, retired when the popup changes) is what carries the
    // intent — the row index alone cannot express "↓ then ↑ selected row 0".
    let explicit_selection = selection_is_explicit(state, &sugg);
    if sugg.items.len() == 1 || explicit_selection {
        Some(if is_slash {
            EnterCompletionAction::ApplyAndSubmit
        } else {
            EnterCompletionAction::ApplyOnly
        })
    } else {
        Some(EnterCompletionAction::Submit)
    }
}

/// Apply the completion an Enter decision selected. Returns `true` when the
/// editor text was rewritten (the caller lets the Enter fall through to
/// submit for `ApplyAndSubmit`, or consumes it for `ApplyOnly`).
pub(super) fn accept_enter_completion(state: &Arc<TuiState>, editor: &Arc<Editor>) -> bool {
    match enter_completion_action(state, editor) {
        Some(EnterCompletionAction::ApplyAndSubmit) | Some(EnterCompletionAction::ApplyOnly) => {
            accept_top_suggestion(state, editor)
        }
        _ => false,
    }
}

/// Move the autocomplete highlight up (`direction < 0`) or down (`direction > 0`)
/// and re-render the list. Returns `true` when the key was consumed.
///
/// Only takes over the arrow keys in an explicit completion context (`/` or
/// `@` prefix); a bare-word path completion (e.g. a draft containing `docs`)
/// must NOT swallow ↑/↓, which the editor uses for cursor/history movement.
pub(super) fn navigate_autocomplete(
    state: &Arc<TuiState>,
    editor: &Arc<Editor>,
    direction: i32,
) -> bool {
    let text = editor.get_text();
    let cursor = editor_cursor_offset(editor, &text);
    let Some(sugg) = state.autocomplete.get_suggestions(&text, cursor) else {
        return false;
    };
    if sugg.items.is_empty() {
        return false;
    }
    let start = sugg.start.min(text.len());
    // `/` commands keep the slash at `start`; the path provider keeps `@` by
    // placing `start` *after* it, so look just before `start` for `@`.
    let explicit = text[start..].starts_with('/') || text[..start].ends_with('@');
    if !explicit {
        return false;
    }
    let total = sugg.items.len();
    {
        let mut sel = state.autocomplete_selection.lock().unwrap();
        if direction < 0 {
            *sel = sel.saturating_sub(1);
        } else {
            *sel = (*sel + 1).min(total.saturating_sub(1));
        }
    }
    // This popup has now been explicitly navigated: stamp it so a later Enter
    // accepts the (possibly zeroth) highlighted row as a deliberate choice.
    *state.autocomplete_selection_stamped_for.lock().unwrap() = Some(sugg_key(&sugg));
    render_autocomplete(state, Some(sugg));
    true
}
