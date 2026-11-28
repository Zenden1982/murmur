-- Новое право MANAGE_MESSAGES (128): удалять чужие сообщения. Стандартным Админу и Модератору (Владельцу можно всё и так).
UPDATE server_roles SET permissions = permissions | 128 WHERE position >= 1;
