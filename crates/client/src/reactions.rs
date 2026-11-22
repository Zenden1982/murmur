//! Реакции и стикеры в чате: справочники и локальное обновление до ответа сервера.

use crate::kit::Cat;

/// Эмодзи реакции по имени из протокола.
pub fn emoji(name: &str) -> Option<&str> {
    proto::reaction_emoji(name)
}

/// Какой кот рисует стикер.
pub fn sticker_cat(id: &str) -> Option<Cat> {
    Some(match id {
        "cat-hello" => Cat::Hello,
        "cat-sleep" => Cat::Sleep,
        "cat-peek" => Cat::Peek,
        "cat-success" => Cat::Success,
        "cat-loading" => Cat::Loading,
        "cat-offline" => Cat::Offline,
        "cat-notfound" => Cat::NotFound,
        "cat-nomic" => Cat::NoMic,
        _ => return None,
    })
}

/// Подпись стикера для старых клиентов (они покажут её текстом).
pub fn fallback(id: &str) -> String {
    let name = proto::STICKERS.iter().find(|(s, _)| *s == id).map(|(_, n)| *n).unwrap_or("стикер");
    format!("[стикер: {name}]")
}

pub fn is_fallback(content: &str) -> bool {
    content.starts_with("[стикер: ") && content.ends_with(']')
}

pub fn mine(r: &proto::Reaction, me: i64) -> bool {
    r.users.contains(&me)
}

/// Поставить/убрать свою реакцию локально (сервер потом пришлёт точный список).
pub fn toggle(list: &mut Vec<proto::Reaction>, name: &str, me: i64, on: bool) {
    match list.iter_mut().position(|r| r.name == name) {
        Some(i) => {
            let users = &mut list[i].users;
            if on {
                if !users.contains(&me) {
                    users.push(me);
                }
            } else {
                users.retain(|u| *u != me);
                if users.is_empty() {
                    list.remove(i);
                }
            }
        }
        None if on => list.push(proto::Reaction { name: name.to_string(), users: vec![me] }),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_by_name() {
        assert_eq!(emoji("fire"), Some("🔥"));
        assert_eq!(emoji("nope"), None);
    }

    #[test]
    fn sticker_cats() {
        assert_eq!(sticker_cat("cat-sleep"), Some(Cat::Sleep));
        assert_eq!(sticker_cat("cat-nomic"), Some(Cat::NoMic));
        assert_eq!(sticker_cat("dog"), None);
        // все стикеры из протокола нарисуются
        assert!(proto::STICKERS.iter().all(|(id, _)| sticker_cat(id).is_some()));
    }

    #[test]
    fn toggle_local_add_and_remove() {
        let mut r: Vec<proto::Reaction> = Vec::new();
        toggle(&mut r, "fire", 7, true);
        toggle(&mut r, "fire", 8, true);
        toggle(&mut r, "+1", 7, true);
        toggle(&mut r, "fire", 7, true); // повтор — без дубля
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].users, vec![7, 8]);
        toggle(&mut r, "+1", 7, false);
        assert_eq!(r.len(), 1, "пустая реакция исчезает");
        assert!(mine(&r[0], 8));
    }

    #[test]
    fn sticker_fallback_text() {
        assert_eq!(fallback("cat-hello"), "[стикер: Кот машет]");
        assert!(is_fallback("[стикер: Кот машет]"));
        assert!(!is_fallback("привет"));
    }
}
