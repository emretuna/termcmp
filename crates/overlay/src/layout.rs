use suggest::Suggestion;
use unicode_width::UnicodeWidthStr;

use crate::types::{PopupHints, PopupLayout};
use crate::util::display_text;

/// Display-column width of the gutter area (" K  ") in a popup row.
///
/// Nerd Font PUA codepoints used for kind icons (e.g. \u{F018D}) report
/// `UnicodeWidthChar::width == 1` but render as 2 columns in Nerd Font
/// terminals. We use 5 (space + 2-col icon + 2 spaces) to prevent off-by-one
/// overflow in width calculations and give visual separation before the text.
pub(crate) const GUTTER_COLS: usize = 5;
/// Number of suggestion rows that fit while preserving the prompt, padding,
/// and an optional feedback row.
pub(crate) fn content_capacity(
    screen_rows: u16,
    max_visible: usize,
    top_pad: u16,
    bottom_pad: u16,
    feedback_rows: u16,
) -> usize {
    let available = screen_rows.saturating_sub(1 + top_pad + bottom_pad + feedback_rows) as usize;
    max_visible.min(available)
}

/// Content rows that fit *below* the cursor without scrolling, used in
/// compact mode where viewport scrolling is disabled. Bounded by the rows
/// strictly below `cursor_row` (which is the row the popup will be anchored
/// under), minus padding and any reserved feedback row.
pub(crate) fn content_capacity_below(
    screen_rows: u16,
    cursor_row: u16,
    max_visible: usize,
    top_pad: u16,
    bottom_pad: u16,
    feedback_rows: u16,
) -> usize {
    let space_below = screen_rows.saturating_sub(cursor_row.saturating_add(1));
    let available = space_below.saturating_sub(top_pad + bottom_pad + feedback_rows) as usize;
    max_visible.min(available)
}

/// Display-column width of the gap rendered between suggestion text and its
/// inline description. Two spaces: wide enough to be visually distinct from
/// the text itself, narrow enough not to crowd out description content on
/// 80-column terminals.
pub(crate) const DESC_GAP_COLS: usize = 2;

/// One-column trailing pad appended after the text/description on every row.
/// Keeps a reset cursor from butting up against the right edge of the popup
/// (or the scrollbar column) and prevents off-by-one overflows when computing
/// layout width.
pub(crate) const TRAILING_PAD_COLS: usize = 1;

#[allow(clippy::too_many_arguments)]
pub fn compute_layout(
    suggestions: &[Suggestion],
    scroll_offset: usize,
    cursor_row: u16,
    cursor_col: u16,
    screen_rows: u16,
    screen_cols: u16,
    max_visible: usize,
    min_width: u16,
    max_width: u16,
    borders: bool,
    hints: &PopupHints,
    small_terminal_threshold: u16,
    small_terminal_max_visible: usize,
    compact_mode: config::CompactMode,
) -> PopupLayout {
    let is_compact = match compact_mode {
        config::CompactMode::Always => true,
        config::CompactMode::Never => false,
        config::CompactMode::Auto => screen_rows < small_terminal_threshold,
    };
    let max_visible = if is_compact {
        max_visible.min(small_terminal_max_visible)
    } else {
        max_visible
    };
    // Suppress rendering entirely when the terminal is too narrow for the
    // minimum popup width — rendering off-screen corrupts terminal state.
    if screen_cols < min_width {
        return PopupLayout {
            start_row: 0,
            start_col: 0,
            width: 0,
            height: 0,
            scroll_deficit: 0,
        };
    }

    let width_pad: u16 = if borders { 2 } else { 0 };
    let top_pad: u16 = if borders || hints.index.is_some() {
        1
    } else {
        0
    };
    let bottom_pad: u16 = if borders || hints.key_label.is_some() {
        1
    } else {
        0
    };
    let visible_capacity = if is_compact {
        content_capacity_below(screen_rows, cursor_row, max_visible, top_pad, bottom_pad, 0)
    } else {
        content_capacity(screen_rows, max_visible, top_pad, bottom_pad, 0)
    };
    let visible_count = suggestions.len().min(visible_capacity);

    // Compute width from visible suggestions (content only, no border)
    let content_width = suggestions
        .iter()
        .skip(scroll_offset)
        .take(visible_capacity)
        .map(item_display_width)
        .max()
        .unwrap_or(min_width.saturating_sub(width_pad) as usize);
    // Defense-in-depth: ensure the clamp upper bound is never below min_width.
    // The early return at the top of this function guards the common path
    // (screen_cols < min_width), but any future caller that bypasses it — or a
    // future refactor that lets max_width shrink below min_width — would
    // otherwise hit `clamp(min, max)` with min > max, which is an unconditional
    // panic in std. `.max(min_width)` collapses the degenerate bound into a
    // no-op clamp that returns min_width. Phase 1 CRIT-2 fix claimed this
    // guard was added to compute_layout but only added the early return in
    // render_popup — this line closes the gap.
    let effective_max_w = max_width.min(screen_cols).max(min_width);
    let width = (content_width as u16 + width_pad).clamp(min_width, effective_max_w);

    // Height includes border/hint rows when enabled
    let height = (visible_count as u16) + top_pad + bottom_pad;

    // Always render below the cursor. Compact mode never scrolls, so it
    // limits visible suggestions (above) to what fits below; non-compact mode
    // scrolls to make room, so below the (adjusted) cursor always fits.
    let start_row = cursor_row + 1;

    // Horizontal: start at cursor col, shift left if overflows
    let start_col = if cursor_col + width > screen_cols {
        screen_cols.saturating_sub(width)
    } else {
        cursor_col
    };

    PopupLayout {
        start_row,
        start_col,
        width,
        height,
        scroll_deficit: 0, // Caller sets this after computing scroll
    }
}

/// Calculate display width for a single suggestion line.
/// Format: " K text  description " where K is kind char.
fn item_display_width(suggestion: &Suggestion) -> usize {
    // gutter = GUTTER_COLS, then text, then optional "  desc", then trailing space
    let (dt, _) = display_text(suggestion);
    let text_len = UnicodeWidthStr::width(dt);
    let desc_len = suggestion
        .description
        .as_ref()
        .map(|d| UnicodeWidthStr::width(d.as_str()) + DESC_GAP_COLS)
        .unwrap_or(0);
    GUTTER_COLS + text_len + desc_len + TRAILING_PAD_COLS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DEFAULT_MAX_POPUP_WIDTH, DEFAULT_MAX_VISIBLE, DEFAULT_MIN_POPUP_WIDTH};
    use suggest::SuggestionKind;

    fn make(text: &str, desc: Option<&str>) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            description: desc.map(String::from),
            ..Default::default()
        }
    }

    fn make_path(text: &str, kind: SuggestionKind, desc: Option<&str>) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            kind,
            description: desc.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn test_popup_below_cursor() {
        let suggestions = vec![make("checkout", None), make("commit", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            5,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.scroll_deficit, 0);
        assert_eq!(layout.start_row, 6);
    }

    #[test]
    fn test_popup_always_below_even_near_bottom() {
        let suggestions: Vec<Suggestion> =
            (0..5).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            22,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        // Layout always places below — start_row = cursor_row + 1
        assert_eq!(layout.start_row, 23);
        assert_eq!(layout.scroll_deficit, 0);
    }

    #[test]
    fn test_popup_shifts_left() {
        let suggestions = vec![make("a-long-suggestion-name", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            5,
            70,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert!(layout.start_col + layout.width <= 80);
    }

    #[test]
    fn test_popup_at_top_left() {
        let suggestions = vec![make("ls", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.start_row, 1);
        assert_eq!(layout.start_col, 0);
    }

    #[test]
    fn test_width_clamped_min() {
        let suggestions = vec![make("x", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert!(layout.width >= DEFAULT_MIN_POPUP_WIDTH);
    }

    #[test]
    fn test_width_clamped_max() {
        let long_desc = "a".repeat(200);
        let suggestions = vec![make("cmd", Some(&long_desc))];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert!(layout.width <= DEFAULT_MAX_POPUP_WIDTH);
    }

    #[test]
    fn test_height_capped_at_max_visible() {
        let suggestions: Vec<Suggestion> =
            (0..50).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            80,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.height, DEFAULT_MAX_VISIBLE as u16 + 2); // +2 for borders
    }

    #[test]
    fn test_custom_max_visible() {
        let suggestions: Vec<Suggestion> =
            (0..50).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            80,
            5,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.height, 7); // 5 content + 2 borders
    }

    #[test]
    fn test_height_capped_by_small_screen() {
        let suggestions: Vec<Suggestion> =
            (0..50).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            6,
            80,
            15,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.height, 5, "three content rows plus two borders");
    }

    // --- Bug B4: filepath width uses basename only ---

    #[test]
    fn test_filepath_width_uses_basename() {
        // "src/deeply/nested/file.txt" as FilePath — display width should be
        // based on "file.txt" (8 chars), not the full 25-byte path.
        let deep = make_path("src/deeply/nested/file.txt", SuggestionKind::FilePath, None);
        let shallow = make_path("file.txt", SuggestionKind::FilePath, None);
        assert_eq!(item_display_width(&deep), item_display_width(&shallow));
    }

    #[test]
    fn test_directory_width_uses_basename() {
        // "path/to/mydir/" as Directory — display should be "mydir/" (6 chars).
        let deep = make_path("path/to/mydir/", SuggestionKind::Directory, None);
        // GUTTER_COLS(4) + "mydir/"(6) + trailing(1) = 11
        assert_eq!(
            item_display_width(&deep),
            GUTTER_COLS + 6 + TRAILING_PAD_COLS
        );
    }

    #[test]
    fn test_filepath_no_slash_unchanged() {
        let s = make_path("Cargo.toml", SuggestionKind::FilePath, None);
        // GUTTER_COLS(4) + "Cargo.toml"(10) + trailing(1) = 15
        assert_eq!(item_display_width(&s), GUTTER_COLS + 10 + TRAILING_PAD_COLS);
    }

    // --- Bug B5: non-ASCII char counting ---

    #[test]
    fn test_non_ascii_text_width() {
        // 3 CJK characters = 6 terminal columns (2 each via unicode-width)
        let s = make("\u{65E5}\u{672C}\u{8A9E}", None);
        // GUTTER_COLS(4) + text(6 cols) + trailing(1) = 11
        assert_eq!(item_display_width(&s), GUTTER_COLS + 6 + TRAILING_PAD_COLS);
    }

    #[test]
    fn test_non_ascii_description_width() {
        // 3 accented chars = 3 terminal columns (1 each, not fullwidth)
        let s = make("cmd", Some("\u{00E9}\u{00E8}\u{00EA}"));
        // GUTTER_COLS(4) + "cmd"(3) + gap(2) + desc(3 cols) + trailing(1) = 13
        assert_eq!(
            item_display_width(&s),
            GUTTER_COLS + 3 + DESC_GAP_COLS + 3 + TRAILING_PAD_COLS
        );
    }

    // --- CRIT-2 regression: narrow terminal must not panic ---

    #[test]
    fn test_narrow_terminal_suppressed() {
        // screen_cols=10 < DEFAULT_MIN_POPUP_WIDTH=40 — rendering suppressed
        let suggestions = vec![make("checkout", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            10, // narrow terminal
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        // Popup suppressed: zero-size layout prevents off-screen rendering
        assert_eq!(layout.width, 0);
        assert_eq!(layout.height, 0);
    }

    #[test]
    fn test_very_narrow_terminal_suppressed() {
        // screen_cols=1: extreme edge case — must not panic or render
        let suggestions = vec![make("x", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            1,
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout.width, 0);
        assert_eq!(layout.height, 0);
    }

    #[test]
    fn test_exact_min_width_terminal_renders() {
        // screen_cols == min_width — should render normally, not suppress
        let suggestions = vec![make("checkout", None)];
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            24,
            DEFAULT_MIN_POPUP_WIDTH, // exactly min_width
            DEFAULT_MAX_VISIBLE,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert!(layout.width > 0);
        assert!(layout.height > 0);
        assert!(layout.width <= DEFAULT_MIN_POPUP_WIDTH);
    }

    #[test]
    fn test_non_ascii_filepath_width() {
        // basename "ファイル.txt": 4 katakana (2 cols each = 8) + ".txt" (4) = 12 cols
        let s = make_path(
            "docs/\u{65E5}\u{672C}\u{8A9E}/\u{30D5}\u{30A1}\u{30A4}\u{30EB}.txt",
            SuggestionKind::FilePath,
            None,
        );
        // GUTTER_COLS(4) + basename(12 cols) + trailing(1) = 17
        assert_eq!(item_display_width(&s), GUTTER_COLS + 12 + TRAILING_PAD_COLS);
    }

    #[test]
    fn test_gutter_cols_accounts_for_nerd_font_width() {
        // GUTTER_COLS must be 5 to account for Nerd Font PUA icons rendering
        // as 2 columns: space(1) + icon(2) + space(1) + space(1) = 5
        assert_eq!(GUTTER_COLS, 5);
        // Simple command: gutter(5) + "ls"(2) + trailing(1) = 8
        let s = make("ls", None);
        assert_eq!(item_display_width(&s), GUTTER_COLS + 2 + TRAILING_PAD_COLS);
    }

    #[test]
    fn test_small_terminal_caps_visible() {
        // screen_rows=10 < threshold 15, max_visible=10 should be capped to 5 in Auto compact
        let suggestions: Vec<Suggestion> =
            (0..10).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            0,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        // Compact mode caps max_visible to 5 → height = 5 + 2 borders = 7
        assert_eq!(layout.height, 7, "small terminal must cap visible to 5");
        // Also verify that non-compact (Never) would not cap
        let layout_never = compute_layout(
            &suggestions,
            0,
            0,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Never,
        );
        // Without compact, capacity is min(10, available 7) =7 → height 9
        assert!(
            layout_never.height > layout.height,
            "Never mode should not cap"
        );
    }

    #[test]
    fn test_small_terminal_limits_below_cursor() {
        // screen_rows=10 (compact), borders → top/bottom pads reserve 2 rows.
        // Content is limited to what fits below the cursor, so a prompt near the
        // bottom shows fewer suggestions instead of rendering above the prompt.
        let suggestions: Vec<Suggestion> =
            (0..5).map(|i| make(&format!("item{i}"), None)).collect();

        // Cursor mid-screen: space_below = 10-(3+1)=6 → 4 content rows fit.
        let layout_mid = compute_layout(
            &suggestions,
            0,
            3,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout_mid.height, 4 + 2);
        assert_eq!(layout_mid.start_row, 4, "must render below cursor");

        // Cursor low: space_below = 10-(6+1)=3 → only 1 content row fits.
        let layout_low = compute_layout(
            &suggestions,
            0,
            6,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout_low.height, 1 + 2);
        assert_eq!(layout_low.start_row, 7, "still below cursor, never above");
    }

    #[test]
    fn test_small_terminal_no_scroll() {
        // screen_rows=10 → compact disables scroll, scroll_deficit must be 0
        let suggestions: Vec<Suggestion> =
            (0..10).map(|i| make(&format!("item{i}"), None)).collect();
        let layout = compute_layout(
            &suggestions,
            0,
            8,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(
            layout.scroll_deficit, 0,
            "small terminal must have scroll_deficit=0"
        );
        // Also verify via render helper if available (compact disables scroll)
        // Here we just double-check that even with many suggestions, deficit stays 0
        let many: Vec<Suggestion> = (0..50).map(|i| make(&format!("item{i}"), None)).collect();
        let layout_many = compute_layout(
            &many,
            0,
            5,
            0,
            10,
            80,
            10,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            true,
            &PopupHints::default(),
            15,
            5,
            config::CompactMode::Auto,
        );
        assert_eq!(layout_many.scroll_deficit, 0);
    }
}
