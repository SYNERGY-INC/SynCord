//! Emoji helpers: picker groups, search, `:shortcode:` replacement and autocomplete.

use emojis::{Emoji, EmojiVersion, Group};

/// Newest emoji offered: the bundled Twemoji font has everything up to Emoji 15.
const NEWEST: EmojiVersion = EmojiVersion::new(15, 0);

fn supported(e: &Emoji) -> bool {
    e.emoji_version() <= NEWEST
}

/// Picker tabs: group, label, and the emoji shown on the tab.
pub const GROUPS: [(Group, &str, &str); 9] = [
    (Group::SmileysAndEmotion, "Smileys", "😀"),
    (Group::PeopleAndBody, "People", "👋"),
    (Group::AnimalsAndNature, "Nature", "🐶"),
    (Group::FoodAndDrink, "Food", "🍕"),
    (Group::TravelAndPlaces, "Travel", "✈️"),
    (Group::Activities, "Activities", "⚽"),
    (Group::Objects, "Objects", "💡"),
    (Group::Symbols, "Symbols", "❤️"),
    (Group::Flags, "Flags", "🏁"),
];

pub fn group(index: usize) -> impl Iterator<Item = &'static Emoji> {
    GROUPS[index.min(GROUPS.len() - 1)]
        .0
        .emojis()
        .filter(|e| e.skin_tone().is_none_or(|t| t == emojis::SkinTone::Default) && supported(e))
}

/// Emoji whose name or shortcode matches every word of `query`, best matches first.
pub fn search(query: &str, limit: usize) -> Vec<&'static Emoji> {
    let words: Vec<String> = query.split_whitespace().map(|w| w.trim_matches(':').to_lowercase()).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u8, &'static Emoji)> = emojis::iter()
        .filter(|e| supported(e))
        .filter_map(|e| {
            let name = e.name().to_lowercase();
            let codes: Vec<&str> = e.shortcodes().collect();
            let all_match =
                words.iter().all(|w| name.contains(w.as_str()) || codes.iter().any(|c| c.contains(w.as_str())));
            if !all_match {
                return None;
            }
            let first = &words[0];
            let score = if codes.iter().any(|c| c == first) {
                0
            } else if codes.iter().any(|c| c.starts_with(first.as_str())) || name.starts_with(first.as_str()) {
                1
            } else {
                2
            };
            Some((score, e))
        })
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// Turn `:smile:`-style codes into emoji. Unknown codes are left alone.
pub fn replace_shortcodes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-'));
        match end {
            Some(end) if end > 0 && after[end..].starts_with(':') => {
                let code = &after[..end];
                match emojis::get_by_shortcode(code) {
                    Some(e) => {
                        out.push_str(e.as_str());
                        rest = &after[end + 1..];
                    }
                    None => {
                        out.push(':');
                        rest = after;
                    }
                }
            }
            _ => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// If the text ends with a half-typed `:code`, return where it starts and the code.
pub fn autocomplete_token(text: &str) -> Option<(usize, &str)> {
    let start = text.rfind(':')?;
    let code = &text[start + 1..];
    let preceded_ok = start == 0 || text[..start].ends_with(char::is_whitespace);
    let valid = code.len() >= 2 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-');
    (preceded_ok && valid).then_some((start, code))
}

/// Emoji suggestions for a half-typed shortcode: (emoji, shortcode).
pub fn suggestions(code: &str, limit: usize) -> Vec<(&'static Emoji, &'static str)> {
    let code = code.to_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for e in emojis::iter().filter(|e| supported(e)) {
        if let Some(sc) = e.shortcodes().find(|sc| sc.starts_with(code.as_str())) {
            starts.push((e, sc));
        } else if let Some(sc) = e.shortcodes().find(|sc| sc.contains(code.as_str())) {
            contains.push((e, sc));
        }
    }
    starts.sort_by_key(|(_, sc)| sc.len());
    starts.into_iter().chain(contains).take(limit).collect()
}

/// True if the message is just a few emoji (shown big, like other chat apps).
pub fn is_jumbo(text: &str) -> bool {
    let mut count = 0;
    let mut rest = text.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // Longest prefix that is a known emoji (emoji can be several characters long).
        let ends: Vec<usize> = rest.char_indices().map(|(i, c)| i + c.len_utf8()).take(12).collect();
        match ends.iter().rev().find(|&&end| emojis::get(&rest[..end]).is_some()) {
            Some(&end) => {
                rest = &rest[end..];
                count += 1;
                if count > 10 {
                    return false;
                }
            }
            None => return false,
        }
    }
    count > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcodes() {
        assert_eq!(replace_shortcodes("hi :smile: there"), "hi 😄 there");
        assert_eq!(replace_shortcodes(":+1::fire:"), "👍🔥");
        assert_eq!(replace_shortcodes("time 10:30 :nope_not_real: :"), "time 10:30 :nope_not_real: :");
        assert_eq!(replace_shortcodes("no codes"), "no codes");
    }

    #[test]
    fn autocomplete() {
        assert_eq!(autocomplete_token("hello :smi"), Some((6, "smi")));
        assert_eq!(autocomplete_token(":th"), Some((0, "th")));
        assert_eq!(autocomplete_token("hello :s"), None, "too short");
        assert_eq!(autocomplete_token("at 10:30"), None, "not after a space");
        assert_eq!(autocomplete_token("hi :smile: "), None);
        let s = suggestions("thumbs", 3);
        assert_eq!(s[0].0.as_str(), "👍");
    }

    #[test]
    fn searching() {
        let r = search("pizza", 5);
        assert_eq!(r[0].as_str(), "🍕");
        assert!(search("red heart", 5).iter().any(|e| e.as_str() == "❤️"));
        assert!(search("", 5).is_empty());
        assert!(group(0).count() > 100);
        assert!(group(0).all(supported), "nothing newer than the bundled font");
        assert!(!search("melting", 5).is_empty(), "the melting face (2021) is in Twemoji");
        assert!(!search("shaking face", 5).is_empty(), "so is Emoji 15");
    }

    #[test]
    fn jumbo() {
        assert!(is_jumbo("😂"));
        assert!(is_jumbo("🔥 🔥🔥"));
        assert!(is_jumbo("👨‍👩‍👧"));
        assert!(is_jumbo("❤️"));
        assert!(!is_jumbo("lol 😂"));
        assert!(!is_jumbo(""));
        assert!(!is_jumbo(&"😂".repeat(11)));
    }
}
