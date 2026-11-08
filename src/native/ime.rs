//! Text-input-v3 changes are one transaction, applied only at `done`.
use super::model::Model;

#[derive(Default)]
pub struct Pending {
    pub preedit: Option<String>,
    pub cursor: Option<(i32, i32)>,
    pub commit: Option<String>,
    pub delete: Option<(u32, u32)>,
}
impl Pending {
    pub fn apply(self, model: &mut Model) {
        model.preedit.clear();
        model.preedit_cursor = (0, 0);
        if let Some((before, after)) = self.delete {
            model.delete_surrounding(before, after);
        }
        if let Some(text) = self.commit.filter(|t| !t.is_empty()) {
            model.insert(&text);
        }
        if let Some(text) = self.preedit.filter(|t| !t.is_empty()) {
            // A new composition replaces the selection, not adjacent text.
            model.insert("");
            model.preedit = text;
            model.preedit_cursor = self.cursor.unwrap_or((0, 0));
        }
    }
}

// Protocol offsets are UTF-8 bytes, outside the selection. Reject malformed
// ranges rather than splitting a multibyte Korean character or panicking.
pub fn delete_surrounding(
    value: &mut String,
    cursor: &mut usize,
    selected: bool,
    before: u32,
    after: u32,
) -> bool {
    let (lo, hi) = if selected {
        (0, value.len())
    } else {
        (*cursor, *cursor)
    };
    let Some(start) = lo.checked_sub(before as usize) else {
        return false;
    };
    let Some(end) = hi
        .checked_add(after as usize)
        .filter(|end| *end <= value.len())
    else {
        return false;
    };
    if ![start, lo, hi, end]
        .iter()
        .all(|i| value.is_char_boundary(*i))
    {
        return false;
    }
    value.replace_range(hi..end, "");
    value.replace_range(start..lo, "");
    *cursor -= before as usize;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Page;
    #[test]
    fn korean_reconversion_deletes_old_syllable_before_commit_and_preedit() {
        let mut m = Model::new();
        m.navigate(Page::Launcher);
        m.insert("검색한글");
        m.preedit = "새".into();
        Pending {
            delete: Some((3, 0)),
            commit: Some("구".into()),
            preedit: Some("ㅁ".into()),
            cursor: Some((3, 3)),
        }
        .apply(&mut m);
        assert_eq!(m.launcher.query, "검색한구");
        assert_eq!(m.preedit, "ㅁ");
        Pending {
            preedit: Some(String::new()),
            ..Default::default()
        }
        .apply(&mut m);
        while !m.launcher.query.is_empty() {
            Pending {
                delete: Some((3, 0)),
                ..Default::default()
            }
            .apply(&mut m);
        }
        assert_eq!(m.launcher.cursor, 0);
        assert!(m.preedit.is_empty());
        m.insert("새검색");
        assert_eq!(m.launcher.query, "새검색");
    }
    #[test]
    fn byte_deletion_preserves_selection_and_valid_utf8() {
        let mut value = "한글ab".to_string();
        let mut cursor = 6;
        assert!(!delete_surrounding(&mut value, &mut cursor, false, 1, 0));
        assert_eq!(value, "한글ab");
        assert!(delete_surrounding(&mut value, &mut cursor, false, 3, 1));
        assert_eq!((value.as_str(), cursor), ("한b", 3));
        assert!(!delete_surrounding(&mut value, &mut cursor, true, 3, 0));
        assert!(delete_surrounding(&mut value, &mut cursor, true, 0, 0));
    }
    #[test]
    fn preedit_replaces_select_all_and_empty_done_clears_composition() {
        let mut m = Model::new();
        m.navigate(Page::Launcher);
        m.insert("검색어");
        m.launcher.selected_all = true;
        Pending {
            preedit: Some("ㅎ".into()),
            ..Default::default()
        }
        .apply(&mut m);
        assert!(m.launcher.query.is_empty());
        assert_eq!(m.preedit, "ㅎ");
        Pending::default().apply(&mut m);
        assert!(m.preedit.is_empty());
    }
}
