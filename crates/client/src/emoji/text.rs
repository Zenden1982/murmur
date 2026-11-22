//! Разбор текста на куски «текст / эмодзи» и картинки Twemoji.

pub enum Seg<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

use unicode_segmentation::UnicodeSegmentation;

/// Куски подряд: текст склеивается, каждая графема-эмодзи (с картинкой Twemoji) — отдельно.
pub fn segments(text: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut text_start: Option<usize> = None;
    for (i, g) in text.grapheme_indices(true) {
        if is_emoji(g) {
            if let Some(s) = text_start.take() {
                out.push(Seg::Text(&text[s..i]));
            }
            out.push(Seg::Emoji(g));
        } else if text_start.is_none() {
            text_start = Some(i);
        }
    }
    if let Some(s) = text_start {
        out.push(Seg::Text(&text[s..]));
    }
    out
}

pub fn has_emoji(text: &str) -> bool {
    text.graphemes(true).any(is_emoji)
}

fn is_emoji(g: &str) -> bool {
    // быстрый отсев: буквы (в т.ч. кириллица) и цифры не ищем в таблице Twemoji
    let candidate = g.chars().any(|c| matches!(c as u32, 0xA9 | 0xAE | 0x203C..=0x3299 | 0x1F000..=0x1FAFF));
    candidate && png(g).is_some()
}

/// PNG 72×72 из Twemoji (ищется и с U+FE0F, и без).
pub fn png(emoji: &str) -> Option<&'static [u8]> {
    use twemoji_assets::png::PngTwemojiAsset as A;
    let a = A::from_emoji(emoji).or_else(|| {
        let bare = emoji.replace('\u{FE0F}', "");
        A::from_emoji(&bare).or_else(|| A::from_emoji(&format!("{bare}\u{FE0F}")))
    })?;
    Some(a.data.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<String> {
        segments(s).iter().map(|x| match x { Seg::Text(t) => format!("T{t}"), Seg::Emoji(e) => format!("E{e}") }).collect()
    }

    #[test]
    fn segments_sequences() {
        assert_eq!(kinds("а👨‍👩‍👧б"), ["Tа", "E👨‍👩‍👧", "Tб"]);
        assert_eq!(kinds("🇷🇺"), ["E🇷🇺"]);
        assert_eq!(kinds("1️⃣"), ["E1️⃣"]);
        assert_eq!(kinds("❤"), ["E❤"]);
        assert_eq!(kinds("❤️"), ["E❤️"]);
        assert_eq!(kinds("привет, мир"), ["Tпривет, мир"]);
        assert_eq!(kinds("ок 🔥🔥"), ["Tок ", "E🔥", "E🔥"]);
        assert!(!has_emoji("1 2 # *"));
        assert!(has_emoji("ок 👍"));
    }
}
