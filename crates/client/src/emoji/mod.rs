//! Эмодзи: каталог (группы, :коды:, русские имена из CLDR), поиск, разбор текста, рисование, панель.

pub mod text;

/// Раздел панели эмодзи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Smileys,
    People,
    Animals,
    Food,
    Travel,
    Activities,
    Objects,
    Symbols,
    Flags,
}

pub const GROUPS: [Group; 9] =
    [Group::Smileys, Group::People, Group::Animals, Group::Food, Group::Travel, Group::Activities, Group::Objects, Group::Symbols, Group::Flags];

pub struct Entry {
    pub emoji: &'static str,
    pub group: Group,
    /// gemoji-коды (fire) и русское имя (огонь, морда_кошки) — без двоеточий, в нижнем регистре.
    pub codes: Vec<String>,
    pub ru_name: &'static str,
    pub ru_keywords: Vec<&'static str>,
}

struct Catalog {
    entries: Vec<Entry>,
    by_code: std::collections::HashMap<String, usize>,
}

static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();

const RU_TSV: &str = include_str!("../../assets/emoji-ru.tsv");

fn bare(e: &str) -> String {
    e.replace('\u{FE0F}', "")
}

/// Русское имя → код: «морда кошки» → «морда_кошки», «флаг: Россия» → «флаг_россия».
fn ru_code(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('_') {
            s.push('_');
        }
    }
    s.trim_end_matches('_').to_string()
}

fn build() -> Catalog {
    let mut ru = std::collections::HashMap::new();
    for line in RU_TSV.lines() {
        let mut p = line.split('\t');
        if let (Some(e), Some(name), kw) = (p.next(), p.next(), p.next().unwrap_or("")) {
            ru.insert(bare(e), (name, kw.split('|').filter(|k| !k.is_empty()).collect::<Vec<_>>()));
        }
    }
    let mut entries = Vec::new();
    let mut by_code = std::collections::HashMap::new();
    for e in emojis::iter() {
        let s = e.as_str();
        if s.chars().any(|c| ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)) || text::png(s).is_none() {
            continue;
        }
        let group = match e.group() {
            emojis::Group::SmileysAndEmotion => Group::Smileys,
            emojis::Group::PeopleAndBody => Group::People,
            emojis::Group::AnimalsAndNature => Group::Animals,
            emojis::Group::FoodAndDrink => Group::Food,
            emojis::Group::TravelAndPlaces => Group::Travel,
            emojis::Group::Activities => Group::Activities,
            emojis::Group::Objects => Group::Objects,
            emojis::Group::Symbols => Group::Symbols,
            emojis::Group::Flags => Group::Flags,
        };
        let (ru_name, ru_keywords) = ru.get(&bare(s)).cloned().unwrap_or(("", Vec::new()));
        let mut codes: Vec<String> = e.shortcodes().map(str::to_lowercase).collect();
        let rc = ru_code(ru_name);
        if !rc.is_empty() {
            codes.push(rc);
        }
        let i = entries.len();
        for c in &codes {
            by_code.entry(c.clone()).or_insert(i);
        }
        entries.push(Entry { emoji: s, group, codes, ru_name, ru_keywords });
    }
    Catalog { entries, by_code }
}

fn cat() -> &'static Catalog {
    CATALOG.get_or_init(build)
}

/// Все эмодзи панели: только с картинкой Twemoji, без оттенков кожи, в порядке Unicode.
pub fn catalog() -> &'static [Entry] {
    &cat().entries
}

/// Поиск без учёта регистра: точный код → код с начала → русское имя/ключевые слова с начала слова → подстрока.
pub fn search(q: &str, limit: usize) -> Vec<&'static Entry> {
    let q = q.trim().trim_matches(':').to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let word_start = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q));
    let mut hits: Vec<(u8, usize)> = catalog()
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let score = if e.codes.iter().any(|c| *c == q) {
                0
            } else if e.codes.iter().any(|c| c.starts_with(&q)) {
                1
            } else if word_start(e.ru_name) || e.ru_keywords.iter().any(|k| word_start(k)) {
                2
            } else if e.ru_name.to_lowercase().contains(&q) || e.codes.iter().any(|c| c.contains(&q)) {
                3
            } else {
                return None;
            };
            Some((score, i))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(limit).map(|(_, i)| &catalog()[i]).collect()
}

/// Эмодзи по коду без двоеточий (fire, огонь).
pub fn by_code(code: &str) -> Option<&'static str> {
    let c = cat();
    c.by_code.get(&code.to_lowercase()).map(|&i| c.entries[i].emoji)
}

pub(crate) fn code_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// Заменяет целые `:код:` из каталога на эмодзи; всё остальное — как было.
pub fn expand_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(|c: char| !code_char(c));
        match end {
            Some(n) if n > 0 && after[n..].starts_with(':') => {
                if let Some(e) = by_code(&after[..n]) {
                    out.push_str(e);
                    rest = &after[n + 1..];
                    continue;
                }
            }
            _ => {}
        }
        out.push(':');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(v: &[&Entry], e: &str) -> bool {
        v.iter().any(|x| x.emoji == e)
    }

    #[test]
    fn catalog_no_skin_tones() {
        assert!(catalog().iter().all(|e| !e.emoji.chars().any(|c| ('\u{1F3FB}'..='\u{1F3FF}').contains(&c))));
    }

    #[test]
    fn catalog_all_have_png() {
        assert!(catalog().iter().all(|e| text::png(e.emoji).is_some()));
    }

    #[test]
    fn catalog_size() {
        assert!(catalog().len() > 1800, "{}", catalog().len());
    }

    #[test]
    fn search_ru() {
        assert!(has(&search("огон", 5), "🔥"));
        assert!(has(&search("кот", 8), "😺"));
        assert!(has(&search("кошк", 8), "🐱"));
    }

    #[test]
    fn search_code() {
        assert_eq!(search("fire", 1)[0].emoji, "🔥");
    }

    #[test]
    fn by_code_ru_and_en() {
        assert_eq!(by_code("fire"), Some("🔥"));
        assert_eq!(by_code("огонь"), Some("🔥"));
        assert_eq!(by_code("нетакого"), None);
    }

    #[test]
    fn expand_basic() {
        assert_eq!(expand_codes(":fire: ок"), "🔥 ок");
        assert_eq!(expand_codes("а :огонь:!"), "а 🔥!");
        assert_eq!(expand_codes(":нетакого:"), ":нетакого:");
    }

    #[test]
    fn expand_keeps_plain_colons() {
        for s in ["12:30:45", "http://a:b", ":) :(", "a::b", "::"] {
            assert_eq!(expand_codes(s), s);
        }
    }
}
