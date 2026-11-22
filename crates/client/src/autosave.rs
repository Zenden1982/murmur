//! Автосохранение поля: через 1 с после последнего ввода или сразу при уходе из поля; одна отправка в полёте,
//! правка во время отправки — отправить ещё раз; закрытие настроек — дослать несохранённое.

/// Пауза после ввода.
pub const DELAY_S: f64 = 1.0;

#[derive(Default)]
pub struct Autosave {
    /// Есть несохранённое изменение.
    dirty: bool,
    last_edit: f64,
    /// Ушли из поля — сохранить, не дожидаясь паузы.
    now: bool,
    in_flight: bool,
    /// Последняя отправка не удалась — ждём «повторить» или новой правки.
    failed: bool,
}

impl Autosave {
    pub fn edited(&mut self, now: f64) {
        self.dirty = true;
        self.failed = false;
        self.last_edit = now;
    }

    pub fn focus_lost(&mut self) {
        if self.dirty {
            self.now = true;
        }
    }

    /// Пора отправлять (после true вызывающий отправляет и зовёт `sent`).
    pub fn due(&mut self, now: f64) -> bool {
        !self.in_flight && self.dirty && !self.failed && (self.now || now - self.last_edit >= DELAY_S)
    }

    pub fn sent(&mut self) {
        self.in_flight = true;
        self.dirty = false;
        self.now = false;
    }

    pub fn done(&mut self, ok: bool) {
        self.in_flight = false;
        if !ok {
            self.dirty = true;
            self.failed = true;
        }
    }

    /// «Повторить» после ошибки — отправить сразу.
    pub fn retry(&mut self) {
        if self.failed {
            self.failed = false;
            self.now = true;
        }
    }

    /// Закрытие настроек: есть несохранённое — true (вызывающий отправляет сразу).
    pub fn flush(&mut self) -> bool {
        self.dirty
    }

    /// Ждёт ли что-то отправки (для перерисовки по таймеру).
    pub fn pending(&self) -> bool {
        self.dirty && !self.failed && !self.in_flight
    }
}

/// Индикатор у заголовка раздела.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SaveState {
    #[default]
    Idle,
    Saving,
    Saved,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_after_one_second() {
        let mut a = Autosave::default();
        a.edited(0.0);
        assert!(!a.due(0.9));
        assert!(a.due(1.0));
        a.sent();
        assert!(!a.due(1.1));
    }

    #[test]
    fn edit_restarts_timer() {
        let mut a = Autosave::default();
        a.edited(0.0);
        a.edited(0.8);
        assert!(!a.due(1.5));
        assert!(a.due(1.8));
    }

    #[test]
    fn focus_lost_saves_now() {
        let mut a = Autosave::default();
        a.edited(0.0);
        a.focus_lost();
        assert!(a.due(0.1));
    }

    #[test]
    fn nothing_to_save_is_not_due() {
        let mut a = Autosave::default();
        a.focus_lost();
        assert!(!a.due(5.0));
    }

    #[test]
    fn edit_during_send_resaves() {
        let mut a = Autosave::default();
        a.edited(0.0);
        assert!(a.due(1.0));
        a.sent();
        a.edited(1.2);
        assert!(!a.due(2.5), "пока отправка в полёте — не слать");
        a.done(true);
        assert!(a.due(2.5));
    }

    #[test]
    fn flush_reports_pending() {
        let mut a = Autosave::default();
        a.edited(0.0);
        assert!(a.flush());
        a.sent();
        a.done(true);
        assert!(!a.flush());
    }

    #[test]
    fn failed_keeps_pending() {
        let mut a = Autosave::default();
        a.edited(0.0);
        assert!(a.due(1.0));
        a.sent();
        a.done(false);
        assert!(a.flush());
        assert!(!a.due(10.0), "после ошибки — ждать «повторить», а не долбить сервер");
    }
}
