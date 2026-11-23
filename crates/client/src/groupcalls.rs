//! Групповые звонки на клиенте: состояние из событий сервера, создание/приглашения, выбор друзей.
//! Для голосового движка звонок — «канал» `dms::group_channel(id)` (id отрицательный).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;

use eframe::egui;
use proto::{GroupCall, GroupCallJoin, ServerEvent};

use crate::api::Api;

enum Ev {
    List(Result<Vec<GroupCall>, String>),
    Created(Result<GroupCallJoin, String>),
    Failed(String),
}

pub struct GroupCalls {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    calls: HashMap<i64, GroupCall>,
    /// Входящие, которые я отклонил в окне (пока звонок идёт — не показывать снова).
    dismissed: HashSet<i64>,
    /// Ошибка последнего действия (показать человеку).
    pub error: Option<String>,
}

impl GroupCalls {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, calls: HashMap::new(), dismissed: HashSet::new(), error: None }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.calls.clear();
        self.dismissed.clear();
        self.error = None;
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    pub fn fetch(&self) {
        let api = self.api.clone();
        self.spawn(async move { Ev::List(api.group_calls().await.map_err(|e| e.message)) });
    }

    pub fn on_event(&mut self, e: &ServerEvent) {
        match e {
            ServerEvent::Hello | ServerEvent::Resync => self.fetch(),
            ServerEvent::GroupCall { id, call } => match call {
                Some(c) => {
                    self.calls.insert(*id, c.clone());
                }
                None => {
                    self.calls.remove(id);
                    self.dismissed.remove(id);
                }
            },
            _ => {}
        }
    }

    pub fn get(&self, id: i64) -> Option<&GroupCall> {
        self.calls.get(&id)
    }

    /// Мне звонят (я в списке «звонят»), я не в этом звонке и не отклонил.
    pub fn incoming(&self, me: i64, in_call: Option<i64>) -> Option<&GroupCall> {
        let mut v: Vec<&GroupCall> = self
            .calls
            .values()
            .filter(|c| c.ringing.contains(&me) && Some(c.id) != in_call && !self.dismissed.contains(&c.id))
            .collect();
        v.sort_by_key(|c| c.id);
        v.into_iter().next()
    }

    pub fn dismiss(&mut self, id: i64) {
        self.dismissed.insert(id);
    }

    /// Начать звонок: ответ придёт в `poll`.
    pub fn create(&self, user_ids: Vec<i64>) {
        let api = self.api.clone();
        self.spawn(async move {
            Ev::Created(api.group_create(user_ids).await.map_err(|e| match e.code.as_str() {
                "validation" => e.detail,
                _ => e.message,
            }))
        });
    }

    pub fn invite(&self, id: i64, user_ids: Vec<i64>) {
        let api = self.api.clone();
        self.spawn(async move {
            match api.group_invite(id, user_ids).await {
                Ok(()) => Ev::Failed(String::new()),
                Err(e) => Ev::Failed(if e.code == "validation" { e.detail } else { e.message }),
            }
        });
    }

    pub fn hang_up(&self, id: i64) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.group_hang_up(id).await {
                log::warn!("групповой звонок {id}: не удалось положить трубку: {e}");
            }
        });
    }

    /// Разобрать ответы. Some(id) — звонок создан, в него надо войти.
    pub fn poll(&mut self) -> Option<i64> {
        let mut created = None;
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::List(Ok(l)) => {
                    self.calls = l.into_iter().map(|c| (c.id, c)).collect();
                }
                Ev::List(Err(e)) => log::warn!("групповые звонки: {e}"),
                Ev::Created(Ok(j)) => {
                    self.calls.insert(j.call.id, j.call.clone());
                    created = Some(j.call.id);
                }
                Ev::Created(Err(e)) | Ev::Failed(e) if !e.is_empty() => self.error = Some(e),
                Ev::Created(Err(_)) | Ev::Failed(_) => {}
            }
        }
        created
    }
}
