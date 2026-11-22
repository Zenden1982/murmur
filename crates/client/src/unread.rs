//! Непрочитанные сообщения: счётчики по каналам (текстовые каналы серверов и ЛС).
//! Источник — `GET /unread` при входе и переподключении, дальше — события `message_created` / `read_state`.
//! Чистая модель (без UI и сети): что прочитано здесь, забирает приложение (`take_to_send`) и шлёт на сервер.

use std::collections::HashMap;

use proto::{Message, UnreadChannel};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Entry {
    /// Сервер канала; None — ЛС.
    server: Option<i64>,
    count: u32,
    last: i64,
}

#[derive(Default)]
pub struct Unread {
    map: HashMap<i64, Entry>,
    /// Прочитано здесь, ещё не отправлено: канал → до какого сообщения.
    to_send: HashMap<i64, i64>,
}

impl Unread {
    /// Ответ `GET /unread` — всё заново.
    pub fn set(&mut self, list: Vec<UnreadChannel>) {
        self.map = list
            .into_iter()
            .filter(|u| u.count > 0)
            .map(|u| (u.channel_id, Entry { server: u.server_id, count: u.count, last: u.last_message_id }))
            .collect();
    }

    /// Новое сообщение. Своё — канал прочитан (сервер это уже знает).
    pub fn on_message(&mut self, m: &Message, server: Option<i64>, me: Option<i64>) {
        if Some(m.author.id) == me {
            self.map.remove(&m.channel_id);
            return;
        }
        let e = self.map.entry(m.channel_id).or_insert(Entry { server, count: 0, last: 0 });
        if m.id > e.last {
            e.count = e.count.saturating_add(1);
            e.last = m.id;
        }
    }

    /// Прочитано на другом моём клиенте (или подтверждение этого).
    pub fn on_read(&mut self, channel: i64, upto: i64) {
        if self.map.get(&channel).is_some_and(|e| e.last <= upto) {
            self.map.remove(&channel);
        }
    }

    /// Канал открыт и на виду — прочитан; сервер узнает через `take_to_send`.
    pub fn mark_read(&mut self, channel: i64) {
        if let Some(e) = self.map.remove(&channel) {
            let v = self.to_send.entry(channel).or_insert(0);
            *v = (*v).max(e.last);
        }
    }

    pub fn take_to_send(&mut self) -> Vec<(i64, i64)> {
        self.to_send.drain().collect()
    }

    pub fn count(&self, channel: i64) -> u32 {
        self.map.get(&channel).map(|e| e.count).unwrap_or(0)
    }

    /// Есть непрочитанное в текстовых каналах сервера.
    pub fn server_has(&self, server: i64) -> bool {
        self.map.values().any(|e| e.server == Some(server))
    }

    /// Всего непрочитанных сообщений в ЛС.
    pub fn dm_total(&self) -> u32 {
        self.map.values().filter(|e| e.server.is_none()).map(|e| e.count).sum()
    }
}

/// Число для бейджа: «7», «99+».
pub fn badge_text(n: u32) -> String {
    if n > 99 { "99+".into() } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::Author;

    fn msg(id: i64, ch: i64, author: i64) -> Message {
        Message {
            id,
            channel_id: ch,
            author: Author { id: author, name: format!("u{author}"), display_name: None, avatar: None },
            content: "x".into(),
            created_at: 0,
            edited_at: None,
            reply_to: None,
            attachments: Vec::new(),
            reactions: Vec::new(),
            sticker: None,
        }
    }

    #[test]
    fn counts_and_reads() {
        let mut u = Unread::default();
        u.set(vec![UnreadChannel { channel_id: 5, server_id: Some(1), count: 2, last_message_id: 10 }]);
        assert!(u.server_has(1));
        u.on_message(&msg(11, 5, 2), Some(1), Some(1));
        assert_eq!(u.count(5), 3);
        // повтор того же сообщения (дочитка после переподключения) не считается
        u.on_message(&msg(11, 5, 2), Some(1), Some(1));
        assert_eq!(u.count(5), 3);
        u.on_message(&msg(20, 7, 2), None, Some(1));
        u.on_message(&msg(21, 7, 2), None, Some(1));
        assert_eq!(u.dm_total(), 2);
        // прочитано на другом клиенте не всё — остаётся
        u.on_read(7, 20);
        assert_eq!(u.count(7), 2);
        u.on_read(7, 21);
        assert_eq!(u.count(7), 0);
        // открыл канал — прочитан, на сервер уйдёт последнее
        u.mark_read(5);
        assert!(!u.server_has(1));
        assert_eq!(u.take_to_send(), vec![(5, 11)]);
        assert!(u.take_to_send().is_empty());
        // своё сообщение — всё прочитано
        u.on_message(&msg(30, 5, 2), Some(1), Some(1));
        u.on_message(&msg(31, 5, 1), Some(1), Some(1));
        assert_eq!(u.count(5), 0);
    }

    #[test]
    fn badge() {
        assert_eq!(badge_text(7), "7");
        assert_eq!(badge_text(100), "99+");
    }
}
