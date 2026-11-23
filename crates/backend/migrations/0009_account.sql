-- Аккаунт: устройство и последняя активность сеанса; «удалённый пользователь» для сообщений ушедших.
ALTER TABLE sessions ADD COLUMN device    TEXT    NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN last_seen INTEGER NOT NULL DEFAULT 0;

-- Сообщения удалённого аккаунта переходят сюда (id 0). Войти нельзя: хэш пароля не разбирается.
INSERT OR IGNORE INTO users (id, name, password_hash, is_admin, created_at) VALUES (0, 'Удалённый пользователь', '!', 0, 0);
