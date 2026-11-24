//! The tray's unread INBOX count: icons are pre-rendered for 0 to 9 and
//! "9+" (`icons/tray/`), since not every Linux tray can draw a badge.

/// Counts above this share the "9+" icon.
pub const MAX_SHOWN: i64 = 9;

/// Index into the pre-rendered icons: the count, or 10 for "9+".
pub fn icon_index(unread: i64) -> usize {
    unread.clamp(0, MAX_SHOWN + 1) as usize
}

pub fn tooltip(unread: i64) -> String {
    match unread {
        i64::MIN..=0 => "Tagmail".to_owned(),
        1 => "Tagmail: 1 unread conversation".to_owned(),
        n => format!("Tagmail: {n} unread conversations"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_the_icon_at_nine_plus() {
        let indexes: Vec<usize> = [-1, 0, 1, 9, 10, 250].map(icon_index).to_vec();
        assert_eq!(indexes, [0, 0, 1, 9, 10, 10]);
        assert_eq!(tooltip(0), "Tagmail");
        assert_eq!(tooltip(1), "Tagmail: 1 unread conversation");
        assert_eq!(tooltip(12), "Tagmail: 12 unread conversations");
    }
}
