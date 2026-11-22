//! Чистая модель серверов (фаза 9.1): мои серверы, раскладка каналов, участники, кто в голосе, люди.
//! Всё меняется только через `apply` (события `/events`) и `set_*` (ответы API) — тестируется без UI.

use std::collections::HashMap;

use proto::{Category, Channel, ChannelLayout, Member, PublicUser, Role, Server, ServerEvent, VoiceRoster};

#[derive(Default)]
pub struct Model {
    /// Мой user id (для прав и «моя роль»).
    pub me: Option<i64>,
    pub servers: Vec<Server>,
    pub active: Option<i64>,
    pub layouts: HashMap<i64, ChannelLayout>,
    pub members: HashMap<i64, Vec<Member>>,
    /// Канал → кто в голосе.
    pub voice: HashMap<i64, Vec<i64>>,
    pub users: HashMap<i64, PublicUser>,
    /// Счётчик удалений серверов (для отбраковки устаревших ответов загрузки).
    seq: u64,
    /// Сервер → номер удаления.
    removed: HashMap<i64, u64>,
}

impl Model {
    /// Список моих серверов (ответ `GET /servers`). Активный сохраняется, если он ещё есть.
    pub fn set_servers(&mut self, list: Vec<Server>) {
        self.servers = list;
        let gone: Vec<i64> = self.layouts.keys().copied().filter(|id| !self.has_server(*id)).collect();
        for id in gone {
            self.forget(id);
        }
        self.members.retain(|id, _| self.servers.iter().any(|s| s.id == *id));
        self.fix_active();
    }

    pub fn set_active(&mut self, id: i64) {
        if self.has_server(id) {
            self.active = Some(id);
        }
    }

    pub fn set_members(&mut self, server: i64, list: Vec<Member>) {
        if !self.has_server(server) {
            return;
        }
        for m in &list {
            self.users.insert(m.user.id, m.user.clone());
        }
        self.members.insert(server, list);
    }

    pub fn set_layout(&mut self, server: i64, layout: ChannelLayout) {
        if !self.has_server(server) {
            return;
        }
        self.layouts.insert(server, layout);
    }

    /// Ответ `GET /servers/{id}/voice`: все голосовые каналы сервера сразу.
    pub fn set_voice(&mut self, server: i64, rosters: Vec<VoiceRoster>) {
        if !self.has_server(server) {
            return;
        }
        for ch in self.channel_ids(server) {
            self.voice.remove(&ch);
        }
        for r in rosters {
            self.put_roster(r);
        }
    }

    #[cfg(test)]
    pub fn with_me(me: i64) -> Self {
        Self { me: Some(me), ..Default::default() }
    }

    /// Отметка для запроса загрузки: с ней сравнивается `accept_snapshot`.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Ответ загрузки, начатой при `since`, можно применить к серверу (его не удалили после начала запроса).
    pub fn accept_snapshot(&self, since: u64, server: i64) -> bool {
        self.removed.get(&server).is_none_or(|&s| s <= since)
    }

    /// Мой голосовой канал пропал: сервер ушёл или канал удалён (раскладка известна).
    pub fn voice_channel_gone(&self, ch: &Channel) -> bool {
        // звонок в ЛС к серверам не относится
        if crate::dms::is_dm(ch) {
            return false;
        }
        if !self.has_server(ch.server_id) {
            return true;
        }
        self.layouts.get(&ch.server_id).is_some_and(|l| !l.channels.iter().any(|c| c.id == ch.id))
    }

    /// Сервер есть, а раскладки ещё нет (вступил по событию) — надо загрузить.
    pub fn needs_load(&self, id: i64) -> bool {
        self.has_server(id) && !self.layouts.contains_key(&id)
    }

    /// Голосовой канал на моих серверах, где сейчас человек: (сервер, канал).
    pub fn voice_of(&self, uid: i64) -> Option<(&Server, &Channel)> {
        let (&ch, _) = self.voice.iter().filter(|(_, ids)| ids.contains(&uid)).min_by_key(|(ch, _)| **ch)?;
        self.channel_place(ch)
    }

    /// Сервер и канал по id канала (если раскладка известна).
    pub fn channel_place(&self, ch: i64) -> Option<(&Server, &Channel)> {
        self.layouts.iter().find_map(|(sid, l)| {
            let c = l.channels.iter().find(|c| c.id == ch)?;
            Some((self.server(*sid)?, c))
        })
    }

    /// Все голосовые каналы, где кто-то есть: (сервер, канал, кто), по порядку серверов.
    pub fn busy_voice(&self) -> Vec<(&Server, &Channel, &[i64])> {
        let mut out: Vec<_> = self
            .voice
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .filter_map(|(ch, ids)| self.channel_place(*ch).map(|(s, c)| (s, c, ids.as_slice())))
            .collect();
        let order = |sid: i64| self.servers.iter().position(|s| s.id == sid).unwrap_or(usize::MAX);
        out.sort_by_key(|(s, c, _)| (order(s.id), c.position, c.id));
        out
    }

    /// Человек — участник сервера (по загруженному списку).
    pub fn is_member(&self, server: i64, uid: i64) -> bool {
        self.members.get(&server).is_some_and(|l| l.iter().any(|m| m.user.id == uid))
    }

    /// Участник сервера.
    pub fn member(&self, server: i64, uid: i64) -> Option<&Member> {
        self.members.get(&server).and_then(|l| l.iter().find(|m| m.user.id == uid))
    }

    pub fn server(&self, id: i64) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }

    pub fn active_server(&self) -> Option<&Server> {
        self.active.and_then(|id| self.server(id))
    }

    fn has_server(&self, id: i64) -> bool {
        self.servers.iter().any(|s| s.id == id)
    }

    fn fix_active(&mut self) {
        if !self.active.is_some_and(|id| self.has_server(id)) {
            self.active = self.servers.first().map(|s| s.id);
        }
    }

    fn channel_ids(&self, server: i64) -> Vec<i64> {
        self.layouts.get(&server).map(|l| l.channels.iter().map(|c| c.id).collect()).unwrap_or_default()
    }

    /// Убрать всё, что знаем о сервере.
    fn forget(&mut self, server: i64) {
        for ch in self.channel_ids(server) {
            self.voice.remove(&ch);
        }
        self.layouts.remove(&server);
        self.members.remove(&server);
    }

    fn put_roster(&mut self, r: VoiceRoster) {
        if r.user_ids.is_empty() {
            self.voice.remove(&r.channel_id);
        } else {
            self.voice.insert(r.channel_id, r.user_ids);
        }
    }

    pub fn apply(&mut self, e: &ServerEvent) {
        match e {
            ServerEvent::ServerUpserted { server } => {
                self.removed.remove(&server.id);
                match self.servers.iter_mut().find(|s| s.id == server.id) {
                    Some(s) => *s = server.clone(),
                    None => self.servers.push(server.clone()),
                }
                self.fix_active();
            }
            ServerEvent::ServerRemoved { server_id } => {
                self.seq += 1;
                self.removed.insert(*server_id, self.seq);
                self.servers.retain(|s| s.id != *server_id);
                self.forget(*server_id);
                self.fix_active();
            }
            ServerEvent::MemberUpserted { server_id, member } => {
                self.users.insert(member.user.id, member.user.clone());
                if Some(member.user.id) == self.me
                    && let Some(s) = self.servers.iter_mut().find(|s| s.id == *server_id)
                {
                    s.my_role_id = member.role_id;
                }
                if let Some(list) = self.members.get_mut(server_id) {
                    match list.iter_mut().find(|m| m.user.id == member.user.id) {
                        Some(m) => *m = member.clone(),
                        None => list.push(member.clone()),
                    }
                }
            }
            ServerEvent::MemberRemoved { server_id, user_id } => {
                if let Some(list) = self.members.get_mut(server_id) {
                    list.retain(|m| m.user.id != *user_id);
                }
            }
            ServerEvent::LayoutChanged { server_id, layout } => self.set_layout(*server_id, layout.clone()),
            ServerEvent::Voice { roster, .. } => self.put_roster(roster.clone()),
            ServerEvent::UserUpdated { user } => {
                self.users.insert(user.id, user.clone());
                for list in self.members.values_mut() {
                    for m in list.iter_mut().filter(|m| m.user.id == user.id) {
                        m.user = user.clone();
                    }
                }
            }
            ServerEvent::Presence { user_id, online } => {
                for m in self.members.values_mut().flat_map(|l| l.iter_mut()).filter(|m| m.user.id == *user_id) {
                    m.online = *online;
                    if !online {
                        m.activity = None;
                    }
                }
            }
            ServerEvent::Activity { user_id, activity } => {
                for m in self.members.values_mut().flat_map(|l| l.iter_mut()).filter(|m| m.user.id == *user_id) {
                    m.activity = activity.clone();
                }
            }
            ServerEvent::MessageCreated { .. }
            | ServerEvent::ReactionsChanged { .. }
            | ServerEvent::Hello
            | ServerEvent::Resync
            | ServerEvent::FriendsChanged
            | ServerEvent::Summon { .. }
            | ServerEvent::ReadState { .. }
            | ServerEvent::Call { .. } => {}
        }
    }

    fn my_role(&self, server: i64) -> Option<&Role> {
        let s = self.server(server)?;
        s.roles.iter().find(|r| r.id == s.my_role_id)
    }

    fn role_of(&self, server: i64, user: i64) -> Option<&Role> {
        let s = self.server(server)?;
        let rid = self.members.get(&server)?.iter().find(|m| m.user.id == user)?.role_id;
        s.roles.iter().find(|r| r.id == rid)
    }

    pub fn my_perms(&self, server: i64) -> u32 {
        self.my_role(server).map(|r| r.permissions).unwrap_or(0)
    }

    pub fn has_perm(&self, server: i64, p: u32) -> bool {
        proto::perm::has(self.my_perms(server), p)
    }

    pub fn is_owner(&self, server: i64) -> bool {
        self.me.is_some() && self.server(server).map(|s| s.owner_id) == self.me
    }

    /// Действие над человеком (кик, бан, роль, выкинуть из голоса) — только если его роль строго ниже моей.
    pub fn can_act_on(&self, server: i64, user: i64) -> bool {
        if Some(user) == self.me {
            return false;
        }
        match (self.my_role(server), self.role_of(server, user)) {
            (Some(me), Some(them)) => them.position < me.position,
            _ => false,
        }
    }

    /// Назначить можно только роль строго ниже своей (владелец не назначается — он выше всех).
    pub fn can_assign(&self, server: i64, role: i64) -> bool {
        let Some(s) = self.server(server) else { return false };
        match (self.my_role(server), s.roles.iter().find(|r| r.id == role)) {
            (Some(me), Some(r)) => r.position < me.position,
            _ => false,
        }
    }

    /// Каналы по категориям: без категории — сверху, дальше категории по позиции; внутри — по позиции.
    pub fn sorted_layout(&self, server: i64) -> Vec<(Option<Category>, Vec<Channel>)> {
        let Some(l) = self.layouts.get(&server) else { return Vec::new() };
        let mut chans = l.channels.clone();
        chans.sort_by_key(|c| (c.position, c.id));
        let mut cats = l.categories.clone();
        cats.sort_by_key(|c| (c.position, c.id));
        let mut out = Vec::new();
        let loose: Vec<Channel> = chans
            .iter()
            .filter(|c| c.category_id.is_none_or(|id| !cats.iter().any(|k| k.id == id)))
            .cloned()
            .collect();
        if !loose.is_empty() {
            out.push((None, loose));
        }
        for cat in cats {
            let inside = chans.iter().filter(|c| c.category_id == Some(cat.id)).cloned().collect();
            out.push((Some(cat), inside));
        }
        out
    }

    /// Все каналы сервера (плоско, в порядке показа).
    pub fn channels(&self, server: i64) -> Vec<Channel> {
        self.sorted_layout(server).into_iter().flat_map(|(_, c)| c).collect()
    }

    pub fn in_voice(&self, channel: i64) -> &[i64] {
        self.voice.get(&channel).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn user(&self, id: i64) -> Option<&PublicUser> {
        self.users.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{ChannelKind, perm};

    fn roles() -> Vec<Role> {
        vec![
            Role { id: 10, name: "Владелец".into(), position: 3, permissions: perm::ALL },
            Role { id: 11, name: "Админ".into(), position: 2, permissions: perm::ALL & !perm::MANAGE_CHANNELS },
            Role { id: 12, name: "Модератор".into(), position: 1, permissions: perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT },
            Role { id: 13, name: "Участник".into(), position: 0, permissions: perm::INVITE },
        ]
    }

    fn server(id: i64, my_role: i64) -> Server {
        Server { id, name: format!("s{id}"), icon: None, owner_id: 1, roles: roles(), my_role_id: my_role, ..Default::default() }
    }

    fn user(id: i64, name: &str) -> PublicUser {
        PublicUser { id, name: name.into(), display_name: None, avatar: None }
    }

    fn ch(id: i64, cat: Option<i64>, pos: i32) -> Channel {
        Channel { id, name: format!("c{id}"), kind: ChannelKind::Voice, server_id: 1, category_id: cat, position: pos, user_limit: None }
    }

    #[test]
    fn apply_server_removed_switches_active() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13), server(2, 13)]);
        m.set_active(2);
        m.apply(&ServerEvent::ServerRemoved { server_id: 2 });
        assert_eq!(m.servers.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(m.active, Some(1), "активным становится оставшийся сервер");
        m.apply(&ServerEvent::ServerRemoved { server_id: 1 });
        assert_eq!(m.active, None);
    }

    #[test]
    fn apply_layout_sorted_by_position() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13)]);
        let layout = ChannelLayout {
            categories: vec![
                Category { id: 7, name: "Б".into(), position: 1 },
                Category { id: 6, name: "А".into(), position: 0 },
            ],
            channels: vec![ch(3, Some(7), 0), ch(2, None, 1), ch(1, None, 0), ch(5, Some(6), 1), ch(4, Some(6), 0)],
        };
        m.apply(&ServerEvent::LayoutChanged { server_id: 1, layout });
        let got: Vec<(Option<i64>, Vec<i64>)> = m
            .sorted_layout(1)
            .into_iter()
            .map(|(c, chs)| (c.map(|c| c.id), chs.iter().map(|c| c.id).collect()))
            .collect();
        assert_eq!(got, vec![(None, vec![1, 2]), (Some(6), vec![4, 5]), (Some(7), vec![3])]);
    }

    #[test]
    fn apply_voice_replaces_roster() {
        let mut m = Model::default();
        m.apply(&ServerEvent::Voice { server_id: 1, roster: VoiceRoster { channel_id: 5, user_ids: vec![1, 2] } });
        m.apply(&ServerEvent::Voice { server_id: 1, roster: VoiceRoster { channel_id: 5, user_ids: vec![2] } });
        assert_eq!(m.voice.get(&5), Some(&vec![2]));
        m.apply(&ServerEvent::Voice { server_id: 1, roster: VoiceRoster { channel_id: 5, user_ids: vec![] } });
        assert!(m.in_voice(5).is_empty());
    }

    #[test]
    fn apply_user_updated_changes_member_name() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13)]);
        m.set_members(1, vec![Member { user: user(2, "kurisu"), role_id: 13, online: false, activity: None }]);
        let new = PublicUser { display_name: Some("Курису".into()), ..user(2, "kurisu") };
        m.apply(&ServerEvent::UserUpdated { user: new.clone() });
        assert_eq!(m.members[&1][0].user, new);
        assert_eq!(m.user(2).map(|u| u.shown_name()), Some("Курису"));
    }

    #[test]
    fn my_perms_from_role() {
        let mut m = Model::with_me(5);
        m.set_servers(vec![server(1, 12)]);
        assert_eq!(m.my_perms(1), perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT);
        assert_eq!(m.my_perms(99), 0, "чужой сервер — прав нет");
        // Моя роль поменялась событием участника.
        m.apply(&ServerEvent::MemberUpserted { server_id: 1, member: Member { user: user(5, "me"), role_id: 13, online: false, activity: None } });
        assert_eq!(m.my_perms(1), perm::INVITE);
    }

    #[test]
    fn upserted_server_without_layout_needs_load() {
        let mut m = Model::default();
        m.apply(&ServerEvent::ServerUpserted { server: server(3, 13) });
        assert!(m.needs_load(3), "вступил по событию — каналов ещё нет");
        m.set_layout(3, ChannelLayout { categories: vec![], channels: vec![] });
        assert!(!m.needs_load(3));
        assert!(!m.needs_load(99), "чужой сервер не грузим");
    }

    #[test]
    fn late_data_for_removed_server_is_ignored() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13), server(2, 13)]);
        m.apply(&ServerEvent::ServerRemoved { server_id: 2 });
        m.set_layout(2, ChannelLayout { categories: vec![], channels: vec![ch(5, None, 0)] });
        m.set_members(2, vec![Member { user: user(7, "x"), role_id: 13, online: false, activity: None }]);
        m.apply(&ServerEvent::LayoutChanged { server_id: 2, layout: ChannelLayout { categories: vec![], channels: vec![] } });
        assert!(!m.layouts.contains_key(&2), "раскладка ушедшего сервера не возвращается");
        assert!(!m.members.contains_key(&2));
        // вернулся по приглашению — надо загрузить заново
        m.apply(&ServerEvent::ServerUpserted { server: server(2, 13) });
        assert!(m.needs_load(2));
    }

    #[test]
    fn snapshot_started_before_removal_is_rejected() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13), server(2, 13)]);
        let since = m.seq();
        m.apply(&ServerEvent::ServerRemoved { server_id: 2 });
        assert!(!m.accept_snapshot(since, 2), "ответ запроса, начатого до удаления, не воскрешает сервер");
        assert!(m.accept_snapshot(since, 1));
        assert!(m.accept_snapshot(m.seq(), 2), "запрос после удаления — честный ответ");
    }

    #[test]
    fn voice_channel_gone_when_server_or_channel_removed() {
        let mut m = Model::default();
        m.set_servers(vec![server(1, 13)]);
        m.set_layout(1, ChannelLayout { categories: vec![], channels: vec![ch(5, None, 0)] });
        let mine = ch(5, None, 0);
        assert!(!m.voice_channel_gone(&mine));
        m.set_layout(1, ChannelLayout { categories: vec![], channels: vec![] });
        assert!(m.voice_channel_gone(&mine), "канал удалён");
        m.set_layout(1, ChannelLayout { categories: vec![], channels: vec![ch(5, None, 0)] });
        m.apply(&ServerEvent::ServerRemoved { server_id: 1 });
        assert!(m.voice_channel_gone(&mine), "сервер пропал");
    }

    #[test]
    fn hierarchy_can_act_and_assign() {
        let mut m = Model::with_me(5);
        m.set_servers(vec![server(1, 12)]); // модератор
        m.set_members(1, vec![Member { user: user(5, "me"), role_id: 12, online: false, activity: None }, Member { user: user(6, "a"), role_id: 11, online: false, activity: None }, Member { user: user(7, "b"), role_id: 13, online: false, activity: None }]);
        assert!(!m.can_act_on(1, 6), "админ выше модератора");
        assert!(m.can_act_on(1, 7));
        assert!(!m.can_act_on(1, 5), "над собой — нет");
        assert!(m.can_assign(1, 13));
        assert!(!m.can_assign(1, 12), "свою роль — нет, только ниже");
    }
}
