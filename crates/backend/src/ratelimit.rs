//! Простой ограничитель попыток: фиксированное окно на ключ (IP клиента, id пользователя).

use std::{
    collections::HashMap,
    hash::Hash,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct RateLimiter<K = IpAddr> {
    max: u32,
    window: Duration,
    state: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Hash + Eq> RateLimiter<K> {
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Засчитать попытку. `Err(секунды до сброса)`, если лимит исчерпан.
    pub fn check(&self, key: K) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    /// Вернуть одну попытку (успешный вход не должен приближать блокировку).
    pub fn refund(&self, key: K) {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = map.get_mut(&key) {
            e.1 = e.1.saturating_sub(1);
        }
    }

    fn check_at(&self, key: K, now: Instant) -> Result<(), u64> {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = map.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.max {
            let left = self.window.saturating_sub(now.duration_since(entry.0));
            return Err(left.as_secs().max(1));
        }
        entry.1 += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_per_ip_and_resets() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let t0 = Instant::now();
        assert!(rl.check_at(a, t0).is_ok());
        assert!(rl.check_at(a, t0).is_ok());
        assert!(rl.check_at(a, t0).is_err());
        assert!(rl.check_at(b, t0).is_ok());
        assert!(rl.check_at(a, t0 + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn refund_gives_attempt_back() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        rl.check(a).unwrap();
        rl.check(a).unwrap();
        rl.refund(a);
        assert!(rl.check(a).is_ok());
        assert!(rl.check(a).is_err());
    }
}
