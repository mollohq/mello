//! "Say hi" on a joined card: the greeting the card sends to the crew chat.
//!
//! The click is the intent, so the message goes at once. The viewer picks no
//! words: the line is one of six, chosen by the joiner's id, so the same
//! member gets the same line on every device. The member is written in the
//! mention form the chat uses (`@name`, with a `MentionRef` for the core, which
//! turns it into `<@user_id>`). Copy rules: plain words, "crew", no exclamation
//! points.

use mello_core::chat::MentionRef;

/// The six lines. `{name}` stands for the mention.
const LINES: [&str; 6] = [
    "@{name} made it. Welcome to the crew.",
    "@{name} is in. Grab a seat.",
    "Look who showed up. Hey @{name}.",
    "@{name}, you found us. Good.",
    "New blood. Welcome @{name}.",
    "@{name} is here. Mic on, let's go.",
];

/// A stable hash of the id (FNV-1a), so the pick does not change between runs.
fn seed(user_id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in user_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The greeting for this member and the mention it carries.
pub fn greeting(user_id: &str, name: &str) -> (String, MentionRef) {
    let line = LINES[(seed(user_id) % LINES.len() as u64) as usize];
    (
        line.replace("{name}", name),
        MentionRef {
            user_id: user_id.to_string(),
            name: name.to_string(),
        },
    )
}

/// The member id of a joined card id (`joined-<user id>`).
pub fn user_of_card(card_id: &str) -> Option<&str> {
    card_id.strip_prefix("joined-").filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_mentions_the_member_in_the_chat_form() {
        let (text, mention) = greeting("u-sara", "Sara");
        assert!(text.contains("@Sara"), "{text}");
        assert_eq!(mention.user_id, "u-sara");
        assert_eq!(mention.name, "Sara");
    }

    #[test]
    fn the_same_member_gets_the_same_line_and_the_six_lines_all_occur() {
        assert_eq!(greeting("u1", "Sara"), greeting("u1", "Sara"));
        let picked: std::collections::HashSet<String> = (0..200)
            .map(|n| greeting(&format!("user-{n}"), "X").0)
            .collect();
        assert_eq!(picked.len(), LINES.len());
    }

    #[test]
    fn no_line_has_an_exclamation_point_an_em_dash_or_the_word_team() {
        for l in LINES {
            assert!(!l.contains('!') && !l.contains('\u{2014}'), "{l}");
            assert!(!l.to_lowercase().contains("team"), "{l}");
            assert!(l.contains("@{name}"));
        }
    }

    #[test]
    fn a_card_id_names_its_member() {
        assert_eq!(user_of_card("joined-u2"), Some("u2"));
        assert_eq!(user_of_card("joined-"), None);
        assert_eq!(user_of_card("head2head"), None);
    }
}
