//! Pure keystroke-driven buffer model for non-zsh shells.
//!
//! For shells like fish and bash that do not self-report their command buffer
//! via OSC 7772 (zsh's shell integration does), this model reconstructs the
//! command line by tracking keystrokes locally. The proxy feeds `KeyEvent`s
//! from the input parser through `BufferModel::apply_key`, then pushes the
//! resulting buffer into the same `set_command_buffer` -> `buffer_dirty` ->
//! debounce/trigger pipeline that zsh's OSC 7772 feeds.
//!
//! **Drift limitation:** non-modeled events (paste, history recall, shell
//! completion) reset the model to empty. The model resyncs only on the next
//! typed character or prompt boundary (OSC 133;A / 7771;A).

use crate::input::KeyEvent;
use buffer::{byte_to_char_offset, char_to_byte_offset};

/// Local keystroke model of the shell's command line.
///
/// Only populated for shells that don't self-report (non-zsh). When the
/// model is active, `apply_key` is called for every keystroke before the
/// popup dispatch so the buffer is available for suggestion triggering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BufferModel {
    pub buffer: String,
    /// Character offset into `buffer` (matches `TerminalState.buffer_cursor`).
    pub cursor: usize,
}

impl BufferModel {
    /// Reset to empty — alias for `clear`. Used on detected drift boundary.
    pub fn reset(&mut self) {
        self.clear();
    }

    /// Clear the model to empty. Used on Enter / prompt boundary.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    /// Apply one keystroke to the model.
    ///
    /// Returns `true` if the model changed (buffer content or cursor
    /// position). Non-modeled events that reset the model to empty also
    /// return `true` so the caller knows to push the empty buffer through
    /// `set_command_buffer`.
    pub fn apply_key(&mut self, key: &KeyEvent) -> bool {
        match key {
            // --- Modeled events -------------------------------------------------
            KeyEvent::Printable(c) => {
                let byte_off = char_to_byte_offset(&self.buffer, self.cursor);
                self.buffer.insert(byte_off, *c);
                self.cursor += 1;
                true
            }
            KeyEvent::Backspace => {
                if self.cursor > 0 {
                    let byte_off = self.previous_char_byte_offset();
                    self.buffer.remove(byte_off);
                    self.cursor -= 1;
                    true
                } else {
                    false
                }
            }
            KeyEvent::ArrowLeft => {
                let old = self.cursor;
                self.cursor = self.cursor.saturating_sub(1);
                old != self.cursor
            }
            KeyEvent::ArrowRight => {
                let char_count = self.buffer.chars().count();
                let new = (self.cursor + 1).min(char_count);
                let changed = new != self.cursor;
                self.cursor = new;
                changed
            }
            KeyEvent::Home
            | KeyEvent::HomeCsiTilde
            | KeyEvent::HomeCsi7Tilde
            | KeyEvent::HomeSs3
            | KeyEvent::Ctrl('a') => {
                let changed = self.cursor != 0;
                self.cursor = 0;
                changed
            }
            KeyEvent::End
            | KeyEvent::EndCsiTilde
            | KeyEvent::EndCsi8Tilde
            | KeyEvent::EndSs3
            | KeyEvent::Ctrl('e') => {
                let char_count = self.buffer.chars().count();
                let changed = self.cursor != char_count;
                self.cursor = char_count;
                changed
            }
            KeyEvent::Ctrl('u') => {
                if self.buffer.is_empty() || self.cursor == 0 {
                    return false;
                }
                let byte_off = char_to_byte_offset(&self.buffer, self.cursor);
                self.buffer.drain(..byte_off);
                self.cursor = 0;
                true
            }
            KeyEvent::Ctrl('w') => {
                if self.cursor == 0 {
                    return false;
                }
                let byte_before = char_to_byte_offset(&self.buffer, self.cursor);
                let prefix = &self.buffer[..byte_before];
                // Walk backward from the end of `prefix`, skipping trailing
                // whitespace, then skip one whitespace-delimited word.
                let mut end = prefix.len();
                // Skip trailing whitespace
                let bytes = prefix.as_bytes();
                while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                // Skip one non-whitespace run (the word)
                while end > 0 && !bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let deleted = byte_before - end;
                if deleted > 0 {
                    self.buffer.drain(end..byte_before);
                    self.cursor = byte_to_char_offset(&self.buffer, end);
                    true
                } else {
                    false
                }
            }
            KeyEvent::Enter => {
                self.clear();
                true
            }

            // --- Non-modeled events that reset ----------------------------------
            // ArrowUp/ArrowDown: shell history recall replaces the buffer
            // Tab (when popup is hidden): shell tab-completion rewrites buffer
            // Ctrl('r'): reverse-history search rewrites buffer
            // Ctrl('d'): EOF / delete-char-forward — shell-dependent
            // CtrlSpace: rarely typed, safer to reset
            // Escape: dismisses popup / enters vi-mode
            // Raw(_): bracketed paste, Alt+key, unclassified sequences
            // CursorPositionReport: parser-internal, shouldn't reach here
            // Ctrl(c) for c not in {a, e, u, w}: unmodeled control char
            KeyEvent::ArrowUp | KeyEvent::ArrowDown => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
            KeyEvent::Tab => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
            KeyEvent::Ctrl('r') | KeyEvent::Ctrl('d') | KeyEvent::CtrlSpace => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
            KeyEvent::Escape => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
            KeyEvent::Raw(_) | KeyEvent::CursorPositionReport(_, _) => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
            // CtrlSlash does NOT modify the shell line; don't reset.
            KeyEvent::CtrlSlash => false,
            // Ctrl(_) for any unlisted c (Ctrl+b, Ctrl+f, Ctrl+k, etc.) — reset.
            KeyEvent::Ctrl(_) => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }

            // PageUp, PageDown — not buffer-modifying, but reset defensively.
            KeyEvent::PageUp | KeyEvent::PageDown => {
                let was_empty = self.buffer.is_empty() && self.cursor == 0;
                self.clear();
                !was_empty
            }
        }
    }

    /// Byte offset of the character just before the current cursor position.
    /// Panics if `cursor == 0` — caller must check.
    fn previous_char_byte_offset(&self) -> usize {
        // Walk backward from the cursor char boundary.
        // We know cursor > 0 here.
        let byte_cursor = char_to_byte_offset(&self.buffer, self.cursor);
        // If cursor points at a char boundary, the previous char ends at that
        // byte. We need to find the start of the *previous* char.
        let mut pos = byte_cursor;
        if pos > 0 && pos <= self.buffer.len() {
            // Walk backward over continuation bytes (0x80–0xBF)
            pos -= 1;
            while pos > 0 && (self.buffer.as_bytes()[pos] & 0xC0) == 0x80 {
                pos -= 1;
            }
        }
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Insert at start, middle, end -----------------------------------------

    #[test]
    fn insert_into_empty() {
        let mut m = BufferModel::default();
        assert!(m.apply_key(&KeyEvent::Printable('a')));
        assert_eq!(m.buffer, "a");
        assert_eq!(m.cursor, 1);
    }

    #[test]
    fn insert_at_end() {
        let mut m = BufferModel {
            buffer: "ab".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Printable('c')));
        assert_eq!(m.buffer, "abc");
        assert_eq!(m.cursor, 3);
    }

    #[test]
    fn insert_at_middle() {
        let mut m = BufferModel {
            buffer: "ac".into(),
            cursor: 1,
        };
        assert!(m.apply_key(&KeyEvent::Printable('b')));
        assert_eq!(m.buffer, "abc");
        assert_eq!(m.cursor, 2);
    }

    #[test]
    fn insert_at_start() {
        let mut m = BufferModel {
            buffer: "bc".into(),
            cursor: 0,
        };
        assert!(m.apply_key(&KeyEvent::Printable('a')));
        assert_eq!(m.buffer, "abc");
        assert_eq!(m.cursor, 1);
    }

    // --- UTF-8 multibyte insert -----------------------------------------------

    #[test]
    fn insert_utf8_middle() {
        let mut m = BufferModel {
            buffer: "nave".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Printable('ï'))); // U+00EF, 2 bytes
        assert_eq!(m.buffer, "naïve");
        assert_eq!(m.cursor, 3);
    }

    #[test]
    fn insert_utf8_cjk() {
        let mut m = BufferModel::default();
        // 中 = U+4E2D, 3 bytes
        assert!(m.apply_key(&KeyEvent::Printable('中')));
        assert_eq!(m.buffer, "中");
        assert_eq!(m.cursor, 1);
        // Insert before the CJK character
        assert!(m.apply_key(&KeyEvent::ArrowLeft));
        assert!(m.apply_key(&KeyEvent::Printable('a')));
        assert_eq!(m.buffer, "a中");
        assert_eq!(m.cursor, 1);
    }

    // --- Backspace -------------------------------------------------------------

    #[test]
    fn backspace_middle_ascii() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Backspace));
        assert_eq!(m.buffer, "ac");
        assert_eq!(m.cursor, 1);
    }

    #[test]
    fn backspace_at_start_noop() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(!m.apply_key(&KeyEvent::Backspace));
        assert_eq!(m.buffer, "abc");
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn backspace_empty_noop() {
        let mut m = BufferModel::default();
        assert!(!m.apply_key(&KeyEvent::Backspace));
        assert_eq!(m.buffer, "");
        assert_eq!(m.cursor, 0);
    }
    #[test]
    fn backspace_utf8() {
        // "naïve" — n(0) a(1) ï(2) v(3) e(4)
        // Place cursor after 'ï' (position 3), backspace removes 'ï' (2 bytes).
        let mut m = BufferModel {
            buffer: "naïve".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Backspace));
        assert_eq!(m.buffer, "nave");
        assert_eq!(m.cursor, 2);
    }

    // --- ArrowLeft / ArrowRight ------------------------------------------------

    #[test]
    fn arrow_left_from_middle() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::ArrowLeft));
        assert_eq!(m.cursor, 1);
    }

    #[test]
    fn arrow_left_at_start_noop() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(!m.apply_key(&KeyEvent::ArrowLeft));
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn arrow_right_from_middle() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 1,
        };
        assert!(m.apply_key(&KeyEvent::ArrowRight));
        assert_eq!(m.cursor, 2);
    }

    #[test]
    fn arrow_right_at_end_clamped() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 3,
        };
        assert!(!m.apply_key(&KeyEvent::ArrowRight));
        assert_eq!(m.cursor, 3);
    }

    // --- Home / End ------------------------------------------------------------

    #[test]
    fn home_resets_cursor() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Home));
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn home_already_at_start() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(!m.apply_key(&KeyEvent::Home));
    }

    #[test]
    fn end_moves_to_end() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(m.apply_key(&KeyEvent::End));
        assert_eq!(m.cursor, 3);
    }

    #[test]
    fn end_variants() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(m.apply_key(&KeyEvent::EndCsiTilde));
        assert_eq!(m.cursor, 3);
        let mut m2 = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(m2.apply_key(&KeyEvent::EndSs3));
        assert_eq!(m2.cursor, 3);
    }

    #[test]
    fn ctrl_a_and_e() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('a')));
        assert_eq!(m.cursor, 0);

        assert!(m.apply_key(&KeyEvent::Ctrl('e')));
        assert_eq!(m.cursor, 3);
    }

    // --- Ctrl+U ----------------------------------------------------------------

    #[test]
    fn ctrl_u_clears_before_cursor() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 2,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('u')));
        assert_eq!(m.buffer, "c");
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn ctrl_u_at_start_noop() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(!m.apply_key(&KeyEvent::Ctrl('u')));
        assert_eq!(m.buffer, "abc");
    }

    #[test]
    fn ctrl_u_empty_noop() {
        let mut m = BufferModel::default();
        assert!(!m.apply_key(&KeyEvent::Ctrl('u')));
    }

    // --- Ctrl+W ----------------------------------------------------------------

    #[test]
    fn ctrl_w_deletes_one_word() {
        let mut m = BufferModel {
            buffer: "abc def".into(),
            cursor: 7,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('w')));
        assert_eq!(m.buffer, "abc ");
        assert_eq!(m.cursor, 4);
    }

    #[test]
    fn ctrl_w_mid_word() {
        // "abc def", cursor=5 (before 'e'): delete "d" back to space
        let mut m = BufferModel {
            buffer: "abc def".into(),
            cursor: 5,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('w')));
        assert_eq!(m.buffer, "abc ef");
        assert_eq!(m.cursor, 4);
    }

    #[test]
    fn ctrl_w_trailing_spaces() {
        // "abc   ", cursor=6 (past trailing spaces): skip spaces then delete "abc"
        let mut m = BufferModel {
            buffer: "abc   ".into(),
            cursor: 6,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('w')));
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);
    }
    #[test]
    fn ctrl_w_at_start_noop() {
        let mut m = BufferModel {
            buffer: "abc".into(),
            cursor: 0,
        };
        assert!(!m.apply_key(&KeyEvent::Ctrl('w')));
    }

    // --- Enter ----------------------------------------------------------------

    #[test]
    fn enter_clears_model() {
        let mut m = BufferModel {
            buffer: "git push".into(),
            cursor: 8,
        };
        assert!(m.apply_key(&KeyEvent::Enter));
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);
    }

    // --- CtrlSlash (manual trigger) does not modify ----------------------------

    #[test]
    fn ctrl_slash_noop() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(!m.apply_key(&KeyEvent::CtrlSlash));
        assert_eq!(m.buffer, "git");
        assert_eq!(m.cursor, 3);
    }

    // --- Reset-triggering keys -------------------------------------------------

    #[test]
    fn arrow_up_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::ArrowUp));
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn arrow_down_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::ArrowDown));
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn tab_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Tab));
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn ctrl_r_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('r')));
        assert!(m.buffer.is_empty());
    }

    #[test]
    fn ctrl_d_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('d')));
        assert!(m.buffer.is_empty());
    }

    #[test]
    fn ctrl_space_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::CtrlSpace));
        assert!(m.buffer.is_empty());
    }

    #[test]
    fn escape_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Escape));
        assert!(m.buffer.is_empty());
    }

    #[test]
    fn raw_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Raw(vec![0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e])));
        assert!(m.buffer.is_empty());
    }

    // --- Non-modelled Ctrl char resets ---------------------------------------

    #[test]
    fn unmodeled_ctrl_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::Ctrl('b'))); // Ctrl+B not modeled
        assert!(m.buffer.is_empty());
    }

    // --- Already-empty reset returns false ------------------------------------

    #[test]
    fn reset_when_already_empty_returns_false() {
        let mut m = BufferModel::default();
        assert!(!m.apply_key(&KeyEvent::ArrowUp));
        assert!(!m.apply_key(&KeyEvent::ArrowDown));
        assert!(!m.apply_key(&KeyEvent::Escape));
        assert!(!m.apply_key(&KeyEvent::Tab));
        assert!(!m.apply_key(&KeyEvent::Ctrl('r')));
        assert!(!m.apply_key(&KeyEvent::Ctrl('d')));
    }

    // --- PageUp / PageDown not modeled ---------------------------------------

    #[test]
    fn page_up_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::PageUp));
        assert!(m.buffer.is_empty());
    }

    #[test]
    fn page_down_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::PageDown));
        assert!(m.buffer.is_empty());
    }

    // --- CursorPositionReport -------------------------------------------------

    #[test]
    fn cpr_resets() {
        let mut m = BufferModel {
            buffer: "git".into(),
            cursor: 3,
        };
        assert!(m.apply_key(&KeyEvent::CursorPositionReport(1, 1)));
        assert!(m.buffer.is_empty());
    }

    // --- Reset + clear --------------------------------------------------------

    #[test]
    fn reset_and_clear_are_same() {
        let mut m = BufferModel {
            buffer: "data".into(),
            cursor: 2,
        };
        m.reset();
        assert!(m.buffer.is_empty());
        assert_eq!(m.cursor, 0);

        let mut m2 = BufferModel {
            buffer: "data".into(),
            cursor: 2,
        };
        m2.clear();
        assert_eq!(m, m2);
    }
}
