-- Профиль сервера (описание, баннер, цвет) и профиль человека («о себе», свой статус).
ALTER TABLE servers ADD COLUMN description TEXT;
-- готовый узор: waves / night / sunset (NULL — waves)
ALTER TABLE servers ADD COLUMN banner TEXT;
-- своя картинка баннера (ключ в UPLOADS_DIR), важнее узора
ALTER TABLE servers ADD COLUMN banner_key TEXT;
ALTER TABLE servers ADD COLUMN color TEXT;
ALTER TABLE users ADD COLUMN about TEXT;
ALTER TABLE users ADD COLUMN status TEXT;
