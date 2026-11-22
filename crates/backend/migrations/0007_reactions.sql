-- Реакции на сообщения и стикеры.
CREATE TABLE message_reactions (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, user_id, name)
);
CREATE INDEX message_reactions_msg ON message_reactions (message_id, created_at);
-- id стикера (proto::STICKERS), NULL — обычное сообщение
ALTER TABLE messages ADD COLUMN sticker TEXT;
