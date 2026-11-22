-- Свой баннер пользователя (ключ = sha256 файла в UPLOADS_DIR).
ALTER TABLE users ADD COLUMN banner_key TEXT;
