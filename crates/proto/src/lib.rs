//! Общие типы HTTP API Murmur (бэкенд ↔ клиент).
//!
//! Все тела запросов/ответов — JSON. Авторизация — заголовок `Authorization: Bearer <session token>`.

use serde::{Deserialize, Serialize};

/// Версия протокола API. Увеличивается при несовместимых изменениях.
pub const PROTOCOL_VERSION: u32 = 2;

/// Ограничения, общие для клиента и сервера (клиент может проверять заранее).
pub mod limits {
    pub const NAME_MIN: usize = 2;
    pub const NAME_MAX: usize = 32;
    pub const PASSWORD_MIN: usize = 8;
    pub const PASSWORD_MAX: usize = 128;
    pub const CHANNEL_NAME_MAX: usize = 64;
    pub const INVITE_USES_MAX: u32 = 100;
    /// Текст сообщения, символов.
    pub const MESSAGE_MAX: usize = 4000;
    /// Вложений в одном сообщении.
    pub const ATTACHMENTS_MAX: usize = 4;
    /// Размер одной картинки, байт.
    pub const IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;
    /// Сторона картинки, пикселей.
    pub const IMAGE_MAX_SIDE: u32 = 16_384;
    /// Сообщений в одной странице истории (по умолчанию / максимум).
    pub const HISTORY_PAGE: u32 = 50;
    pub const HISTORY_PAGE_MAX: u32 = 100;
    /// Название сервера и категории, символов.
    pub const SERVER_NAME_MAX: usize = 50;
    pub const CATEGORY_NAME_MAX: usize = 50;
    /// Отображаемое имя, символов.
    pub const DISPLAY_NAME_MAX: usize = 32;
    /// Лимит участников голосового канала (1..=).
    pub const USER_LIMIT_MAX: u32 = 99;
    /// Сколько серверов может создать (владеть) один человек.
    pub const SERVERS_OWNED_MAX: usize = 20;
    /// Аватар / иконка сервера: размер файла и сторона квадрата.
    pub const IMAGE_PROFILE_MAX_BYTES: usize = 1024 * 1024;
    pub const IMAGE_PROFILE_MAX_SIDE: u32 = 512;
    /// Своя картинка баннера сервера: ровно 960×480, до 2 МБ.
    pub const BANNER_W: u32 = 960;
    pub const BANNER_H: u32 = 480;
    pub const IMAGE_BANNER_MAX_BYTES: usize = 2 * 1024 * 1024;
    /// Аватар пользователя (итоговый файл): ровно 256×256, до 4 МБ; PNG/JPEG/WebP/GIF.
    pub const AVATAR_SIDE: u32 = 256;
    pub const AVATAR_MAX_BYTES: usize = 4 * 1024 * 1024;
    /// Баннер пользователя (итоговый файл): ровно 960×384 (5:2), до 6 МБ.
    pub const USER_BANNER_W: u32 = 960;
    pub const USER_BANNER_H: u32 = 384;
    pub const USER_BANNER_MAX_BYTES: usize = 6 * 1024 * 1024;
    /// Кадров в GIF профиля (и суммарно пикселей — не больше GIF_FRAMES_MAX холстов).
    pub const GIF_FRAMES_MAX: usize = 300;
    /// Исходник, который можно открыть в окне обрезки.
    pub const PROFILE_SOURCE_MAX_BYTES: usize = 20 * 1024 * 1024;
    /// Описание сервера.
    pub const SERVER_DESCRIPTION_MAX: usize = 120;
    /// «О себе» в профиле.
    pub const ABOUT_MAX: usize = 190;
    /// Свой статус («чиню сервер»).
    pub const STATUS_MAX: usize = 60;
    /// Название игры в активности.
    pub const GAME_NAME_MAX: usize = 100;
    /// Неотвеченных исходящих заявок в друзья.
    pub const FRIEND_REQUESTS_PENDING_MAX: usize = 50;
    /// Друзей у одного человека.
    pub const FRIENDS_MAX: usize = 500;
    /// Сколько звонок в ЛС звонит без ответа, секунд.
    pub const CALL_RING_SECS: u64 = 45;
}

/// Права на сервере — битовая маска (`Role::permissions`).
pub mod perm {
    pub const INVITE: u32 = 1;
    pub const KICK: u32 = 2;
    pub const BAN: u32 = 4;
    /// Выкинуть из голосового канала.
    pub const DISCONNECT: u32 = 8;
    pub const MANAGE_CHANNELS: u32 = 16;
    /// Назначать роли ниже своей.
    pub const MANAGE_ROLES: u32 = 32;
    /// Название, иконка, все приглашения сервера.
    pub const MANAGE_SERVER: u32 = 64;
    pub const ALL: u32 = 127;

    pub fn has(perms: u32, p: u32) -> bool {
        perms & p == p
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub protocol: u32,
}

/// `GET /version` — последняя выпущенная версия клиента. Файлы отдаёт `GET /download/{file}` (без авторизации),
/// `GET /get` ведёт на установщик.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// Версия `x.y.z`.
    pub version: String,
    /// Что нового (коротко, для окна обновления).
    #[serde(default)]
    pub notes: String,
    /// Новый exe клиента для самообновления.
    pub file: String,
    /// SHA-256 файла `file` (hex, строчные).
    pub sha256: String,
    /// Размер `file`, байт.
    pub size: u64,
    /// Установщик для новых пользователей.
    #[serde(default)]
    pub installer: Option<String>,
    /// Минимальная версия сервера (бэкенда), с которой работает этот клиент. Чужой сервер передаёт выпуск
    /// своим клиентам (обновления с официального), только если сам не старее.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_server: Option<String>,
    /// Подпись ed25519 (hex, 128 символов) над `signing_message()`; ключ издателя вшит в клиент (`RELEASE_PUBKEY`).
    /// Без верной подписи клиент не ставит обновление: сервер (в т.ч. чужой self-host) не может подсунуть свой exe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// Публичный ключ подписи выпусков (ed25519). Приватный — `.tools/release-signing.key`, не в репозитории.
pub const RELEASE_PUBKEY: [u8; 32] = [
    0xb1, 0xf7, 0x5d, 0x96, 0x53, 0x08, 0x3b, 0x90,
    0x87, 0xee, 0xc1, 0x43, 0x5c, 0x86, 0x79, 0x49,
    0x34, 0xb7, 0xa0, 0x53, 0x3c, 0x38, 0xf6, 0x63,
    0x50, 0x13, 0x74, 0xe4, 0x18, 0x1d, 0x07, 0x81,
];

impl ReleaseInfo {
    /// Что подписывается: версия, файл, SHA-256 и размер (заметки и min_server — только для показа).
    pub fn signing_message(&self) -> Vec<u8> {
        format!("murmur-release-v1\n{}\n{}\n{}\n{}", self.version, self.file, self.sha256, self.size).into_bytes()
    }

    /// Подпись есть и верна для ключа `pubkey`.
    pub fn verify_with(&self, pubkey: &[u8; 32]) -> bool {
        use ed25519_dalek::{Signature, VerifyingKey};
        let Some(sig) = self.sig.as_deref() else { return false };
        let Some(sig) = unhex::<64>(sig) else { return false };
        let Ok(key) = VerifyingKey::from_bytes(pubkey) else { return false };
        key.verify_strict(&self.signing_message(), &Signature::from_bytes(&sig)).is_ok()
    }

    /// Подпись верна для ключа издателя Murmur.
    pub fn verify(&self) -> bool {
        self.verify_with(&RELEASE_PUBKEY)
    }

    /// Сервер версии `server_version` может отдавать этот выпуск: `min_server` нет или сервер не старее.
    pub fn server_ok(&self, server_version: &str) -> bool {
        match self.min_server.as_deref() {
            None => true,
            Some(min) => matches!((parse_version(server_version), parse_version(min)), (Some(s), Some(m)) if s >= m),
        }
    }

    /// Имена файлов безопасны, хэш похож на SHA-256, версия разбирается.
    pub fn is_valid(&self) -> bool {
        parse_version(&self.version).is_some()
            && safe_file_name(&self.file)
            && self.installer.as_deref().is_none_or(safe_file_name)
            && self.min_server.as_deref().is_none_or(|v| parse_version(v).is_some())
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Имя файла для `/download/{file}`: латиница, цифры, `.`, `-`, `_`; не начинается с точки.
pub fn safe_file_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// `"1.2.3"` → (1, 2, 3). Суффиксы (`-beta`) не поддерживаются.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.trim().split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// `candidate` новее `current`? Неразборчивые версии — не новее.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub invite_code: String,
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub name: String,
    pub password: String,
}

/// Сеанс входа (Настройки → Аккаунт).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Короткий id для завершения сеанса.
    pub id: String,
    /// Устройство (имя компьютера из клиента или User-Agent); пусто у старых сеансов.
    pub device: String,
    pub created_at: i64,
    /// Последняя активность, unix-секунды.
    pub last_seen: i64,
    /// Этот самый сеанс.
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteAccountRequest {
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    /// Админ инстанса (CLI, выпуски); к правам на серверах отношения не имеет.
    pub is_admin: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Ключ картинки (`GET /images/{key}`).
    #[serde(default)]
    pub avatar: Option<String>,
    /// Писать в ЛС могут и люди с общих серверов (по умолчанию — только друзья).
    #[serde(default)]
    pub dm_from_servers: bool,
    /// «О себе» (видно в карточке профиля).
    #[serde(default)]
    pub about: Option<String>,
    /// Свой статус — перекрывает автоматическую активность.
    #[serde(default)]
    pub status: Option<String>,
    /// Ключ своего баннера (`GET /images/{key}`).
    #[serde(default)]
    pub banner: Option<String>,
}

/// Человек, каким его видят другие.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicUser {
    pub id: i64,
    /// Логин.
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

impl PublicUser {
    /// Отображаемое имя, а если его нет — логин.
    pub fn shown_name(&self) -> &str {
        match self.display_name.as_deref() {
            Some(n) if !n.is_empty() => n,
            _ => &self.name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: i64,
    pub name: String,
    /// Чем больше, тем выше (владелец 3, админ 2, модератор 1, участник 0).
    pub position: i32,
    pub permissions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub owner_id: i64,
    pub roles: Vec<Role>,
    /// Моя роль на этом сервере.
    pub my_role_id: i64,
    /// Описание (под баннером и в приглашении).
    #[serde(default)]
    pub description: Option<String>,
    /// Готовый узор баннера ([`banner`]); None — «Волны».
    #[serde(default)]
    pub banner: Option<String>,
    /// Своя картинка баннера (ключ `/images/{key}`), важнее узора.
    #[serde(default)]
    pub banner_image: Option<String>,
    /// Цвет сервера (один из [`SERVER_COLORS`]); None — фирменный.
    #[serde(default)]
    pub color: Option<String>,
}

/// Готовые узоры баннера сервера (рисует клиент).
pub mod banner {
    pub const WAVES: &str = "waves";
    pub const NIGHT: &str = "night";
    pub const SUNSET: &str = "sunset";
    pub const ALL: [&str; 3] = [WAVES, NIGHT, SUNSET];
}

/// Цвета сервера (фон иконки, полоска выбранного канала).
pub const SERVER_COLORS: [&str; 6] = ["#14B8A6", "#0EA5E9", "#8B5CF6", "#EC4899", "#F59E0B", "#22C55E"];

/// Чем человек занят: игра (автоматически, клиент) и/или свой статус. Голос — отдельно (составы каналов).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Activity {
    /// «Играет в …».
    #[serde(default)]
    pub game: Option<String>,
    /// С какого времени играет (unix-секунды).
    #[serde(default)]
    pub since: Option<i64>,
    /// Свой статус.
    #[serde(default)]
    pub status: Option<String>,
}

impl Activity {
    pub fn is_empty(&self) -> bool {
        self.game.is_none() && self.status.is_none()
    }
}

/// `PUT /me/activity` — во что играю (None — ни во что). Хранится только пока клиент в сети.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetActivityRequest {
    #[serde(default)]
    pub game: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
}

/// `GET /users/{id}` — карточка профиля (только тем, кто видит человека: общий сервер, друг, ЛС).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub user: PublicUser,
    /// Ключ своего баннера (`GET /images/{key}`); нет — узор в цвете аватара.
    #[serde(default)]
    pub banner: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    pub online: bool,
    #[serde(default)]
    pub activity: Option<Activity>,
    pub is_friend: bool,
}

/// `POST /channels/{id}/summon` — позвать человека в голосовой канал.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummonRequest {
    pub user_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub user: PublicUser,
    pub role_id: i64,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub activity: Option<Activity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub position: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelLayout {
    pub categories: Vec<Category>,
    pub channels: Vec<Channel>,
}

/// Кто сейчас в голосовом канале (user id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceRoster {
    pub channel_id: i64,
    pub user_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ban {
    pub user: PublicUser,
    pub by: i64,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateServerRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Пустая строка — убрать.
    #[serde(default)]
    pub description: Option<String>,
    /// Узор баннера ([`banner`]); пустая строка — по умолчанию. Своя картинка — `PUT /servers/{id}/banner`.
    #[serde(default)]
    pub banner: Option<String>,
    /// Один из [`SERVER_COLORS`]; пустая строка — фирменный.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRequest {
    pub user_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoleRequest {
    pub role_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanRequest {
    pub user_id: i64,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateChannelRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// `Some(None)` — снять лимит.
    #[serde(default, with = "double_option")]
    pub user_limit: Option<Option<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRequest {
    pub name: String,
}

/// Новый порядок: категории сверху вниз и каналы сверху вниз (с категорией каждого).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutRequest {
    pub categories: Vec<i64>,
    pub channels: Vec<ChannelPlace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPlace {
    pub id: i64,
    #[serde(default)]
    pub category_id: Option<i64>,
}

/// `PATCH /me`. Пустая строка — сбросить отображаемое имя.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateMeRequest {
    #[serde(default)]
    pub display_name: Option<String>,
    /// См. `User::dm_from_servers`.
    #[serde(default)]
    pub dm_from_servers: Option<bool>,
    /// «О себе»; пустая строка — убрать.
    #[serde(default)]
    pub about: Option<String>,
    /// Свой статус; пустая строка — убрать.
    #[serde(default)]
    pub status: Option<String>,
}

/// `GET /invites/{code}` без входа: куда зовут.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitePreview {
    pub server_name: String,
    #[serde(default)]
    pub server_icon: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub member_count: u32,
    #[serde(default)]
    pub online_count: u32,
}

/// Отличает «поле не пришло» от «пришло null» (для снятия лимита).
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, T: Serialize>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    /// Сессионный токен (передавать как `Authorization: Bearer …`).
    pub token: String,
    /// Unix-время (секунды), когда сессия истекает.
    pub expires_at: i64,
    pub user: User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Voice,
    Text,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Voice => "voice",
            ChannelKind::Text => "text",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "voice" => Some(ChannelKind::Voice),
            "text" => Some(ChannelKind::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default)]
    pub server_id: i64,
    #[serde(default)]
    pub category_id: Option<i64>,
    #[serde(default)]
    pub position: i32,
    /// Только у голосовых.
    #[serde(default)]
    pub user_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default)]
    pub category_id: Option<i64>,
    #[serde(default)]
    pub user_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    pub livekit_url: String,
    pub livekit_token: String,
    /// Имя комнаты LiveKit (для отладки; клиенту знать не обязательно).
    pub room: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateInviteRequest {
    /// Сколько раз можно использовать (по умолчанию 1).
    #[serde(default)]
    pub uses: Option<u32>,
    /// Срок действия в часах (по умолчанию 168 = 7 дней; 0 — бессрочно).
    #[serde(default)]
    pub expires_in_hours: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub code: String,
    pub uses_left: u32,
    /// Unix-время истечения; None — бессрочно.
    pub expires_at: Option<i64>,
    pub revoked: bool,
    pub created_by: i64,
    #[serde(default)]
    pub server_id: i64,
}

/// Автор сообщения (минимум, чтобы нарисовать строку).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

/// Вложение (пока только картинки). Файл — `GET /attachments/{id}` с авторизацией.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: i64,
    /// `image/png`, `image/jpeg`, `image/gif`, `image/webp`.
    pub mime: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
}

/// Сообщение текстового канала. Время — unix-секунды.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: i64,
    pub author: Author,
    pub content: String,
    pub created_at: i64,
    /// Задел на правку (чат, вариант B): когда сообщение правили.
    #[serde(default)]
    pub edited_at: Option<i64>,
    /// Задел на ответы (вариант B): id сообщения, на которое отвечают.
    #[serde(default)]
    pub reply_to: Option<i64>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Реакции в порядке появления (у старого бэкенда — пусто).
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// Стикер (id из `STICKERS`); `content` тогда — подпись для старых клиентов.
    #[serde(default)]
    pub sticker: Option<String>,
}

/// Реакция на сообщение: имя (из `REACTIONS` или сам Unicode-эмодзи, см. `reaction_valid`) и кто поставил.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub name: String,
    pub users: Vec<i64>,
}

/// Разрешённые реакции: (имя в API, эмодзи).
pub const REACTIONS: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("heart", "❤"),
    ("joy", "😂"),
    ("fire", "🔥"),
    ("open_mouth", "😮"),
    ("cry", "😢"),
    ("rage", "😡"),
    ("tada", "🎉"),
    ("eyes", "👀"),
    ("100", "💯"),
    ("pray", "🙏"),
    ("sunglasses", "😎"),
    ("thinking", "🤔"),
    ("clap", "👏"),
    ("check", "✅"),
    ("x", "❌"),
];

/// Сколько разных реакций может быть на одном сообщении.
pub const REACTIONS_PER_MESSAGE_MAX: usize = 20;

fn unicode_emoji(s: &str) -> bool {
    emojis::get(s).is_some() || emojis::get(&s.replace('\u{FE0F}', "")).is_some()
}

/// Допустимое имя реакции: одно из `REACTIONS` или один Unicode-эмодзи.
/// Префикс `c:` зарезервирован под свои эмодзи сервера.
pub fn reaction_valid(name: &str) -> bool {
    name.len() <= 32 && !name.starts_with("c:") && (REACTIONS.iter().any(|(n, _)| *n == name) || unicode_emoji(name))
}

/// Что показать для реакции: эмодзи из `REACTIONS` или само имя, если это эмодзи.
pub fn reaction_emoji(name: &str) -> Option<&str> {
    if let Some((_, e)) = REACTIONS.iter().find(|(n, _)| *n == name) {
        return Some(e);
    }
    (name.len() <= 32 && unicode_emoji(name)).then_some(name)
}

/// Имя реакции для эмодзи: из 16 быстрых — старое имя (их понимают старые клиенты), иначе сам эмодзи.
pub fn reaction_name(emoji: &str) -> String {
    let bare = emoji.replace('\u{FE0F}', "");
    REACTIONS.iter().find(|(_, e)| e.replace('\u{FE0F}', "") == bare).map_or_else(|| emoji.to_string(), |(n, _)| n.to_string())
}

/// Стикеры (коты Murmur): (id, подпись).
pub const STICKERS: &[(&str, &str)] = &[
    ("cat-hello", "Кот машет"),
    ("cat-sleep", "Кот спит"),
    ("cat-peek", "Кот выглядывает"),
    ("cat-success", "Кот доволен"),
    ("cat-loading", "Кот ждёт"),
    ("cat-offline", "Кот грустит"),
    ("cat-notfound", "Кот в недоумении"),
    ("cat-nomic", "Кот молчит"),
];

/// `POST /channels/{id}/messages`. Вложения сначала загружаются через `POST /uploads`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendMessageRequest {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<i64>,
    /// Стикер из `STICKERS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticker: Option<String>,
}

/// Событие потока `GET /events` (SSE, поле `data` — этот JSON). Незнакомые `type` клиент пропускает.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Поток открыт. После переподключения клиент дозапрашивает пропущенное (`?after=`).
    Hello,
    /// Сервер не успел доставить часть событий — перечитать открытые каналы.
    Resync,
    MessageCreated { message: Message },
    /// Изменились реакции на сообщение — весь список заново.
    ReactionsChanged { channel_id: i64, message_id: i64, reactions: Vec<Reaction> },
    /// Сервер появился у меня (создал, вступил) или изменился.
    ServerUpserted { server: Server },
    /// Сервер пропал у меня (удалён, вышел, кикнут, забанен).
    ServerRemoved { server_id: i64 },
    MemberUpserted { server_id: i64, member: Member },
    MemberRemoved { server_id: i64, user_id: i64 },
    /// Полная раскладка каналов сервера после любого изменения.
    LayoutChanged { server_id: i64, layout: ChannelLayout },
    /// Новый состав голосового канала.
    Voice { server_id: i64, roster: VoiceRoster },
    /// Сменилось отображаемое имя или аватар.
    UserUpdated { user: PublicUser },
    /// Изменились мои друзья или заявки — перечитать `GET /friends`.
    FriendsChanged,
    /// Друг появился в сети или вышел (открыт ли у него поток событий).
    Presence { user_id: i64, online: bool },
    /// Я прочитал канал до `last_read_id` (с другого моего клиента или этим же) — сбросить непрочитанные.
    ReadState { channel_id: i64, last_read_id: i64 },
    /// Звонок в ЛС начался, изменился или закончился (`call: None`).
    Call { channel_id: i64, call: Option<CallInfo> },
    /// Активность друга или соседа по серверу (None — ничем не занят / вышел).
    Activity { user_id: i64, activity: Option<Activity> },
    /// Меня зовут в голосовой канал.
    Summon { from: PublicUser, server_id: i64, channel_id: i64 },
}

/// Личный диалог (ЛС): канал на двоих. Сообщения — как у текстового канала: `/channels/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmChannel {
    /// id канала.
    pub id: i64,
    /// Собеседник.
    pub user: PublicUser,
    /// Последнее сообщение (для сортировки), None — диалог пуст.
    #[serde(default)]
    pub last_message_id: Option<i64>,
    /// Время последнего сообщения или открытия диалога.
    pub last_at: i64,
    /// Идёт ли звонок.
    #[serde(default)]
    pub call: Option<CallInfo>,
}

/// `POST /dms` — открыть (или найти) диалог с человеком.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenDmRequest {
    pub user_id: i64,
}

/// Звонок в ЛС. Комната LiveKit — `dm-{channel_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallInfo {
    pub channel_id: i64,
    /// Кто позвонил.
    pub caller_id: i64,
    pub started_at: i64,
    /// Звонит: второй ещё не ответил и не отклонил.
    pub ringing: bool,
    /// Кто сейчас в комнате звонка.
    #[serde(default)]
    pub participants: Vec<i64>,
}

/// Непрочитанное в канале (текстовом или ЛС). `GET /unread` отдаёт только каналы, где что-то есть.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadChannel {
    pub channel_id: i64,
    /// Сервер канала; None — ЛС.
    #[serde(default)]
    pub server_id: Option<i64>,
    /// Сколько чужих сообщений не прочитано.
    pub count: u32,
    pub last_message_id: i64,
}

/// `POST /channels/{id}/read` — прочитано до этого сообщения включительно.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub message_id: i64,
}

/// Друг (заявка принята).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Friend {
    pub user: PublicUser,
    /// В сети: клиент запущен и вошёл.
    pub online: bool,
    #[serde(default)]
    pub activity: Option<Activity>,
}

/// Заявка в друзья (входящая или исходящая).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRequest {
    pub user: PublicUser,
    pub created_at: i64,
}

/// `GET /friends`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendsList {
    pub friends: Vec<Friend>,
    /// Мне прислали.
    pub incoming: Vec<FriendRequest>,
    /// Я отправил, ещё не приняли.
    pub outgoing: Vec<FriendRequest>,
}

/// `POST /friends/requests` — заявка по логину.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddFriendRequest {
    pub name: String,
}

/// Тело любого ответа с ошибкой.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Машинный код: `unauthorized`, `forbidden`, `not_found`, `invalid_invite`, `name_taken`,
    /// `bad_credentials`, `validation`, `rate_limited`, `internal`, `bad_request`.
    pub error: String,
    /// Человекочитаемое описание.
    pub message: String,
}

#[cfg(test)]
mod tests {
    #[test]
    fn user_and_profile_without_banner_parse() {
        let u: super::User = serde_json::from_str(r#"{"id":1,"name":"a","is_admin":false}"#).unwrap();
        assert_eq!(u.banner, None);
        let p: super::UserProfile =
            serde_json::from_str(r#"{"user":{"id":1,"name":"a"},"online":false,"is_friend":false}"#).unwrap();
        assert_eq!(p.banner, None);
        assert_eq!(super::limits::AVATAR_SIDE, 256);
    }

    use super::*;

    #[test]
    fn channel_kind_serde_is_lowercase() {
        let c = Channel {
            id: 1,
            name: "Общий".into(),
            kind: ChannelKind::Voice,
            server_id: 1,
            category_id: None,
            position: 0,
            user_limit: None,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains(r#""kind":"voice""#), "{s}");
        assert_eq!(ChannelKind::parse("text"), Some(ChannelKind::Text));
        assert_eq!(ChannelKind::parse("x"), None);
    }

    #[test]
    fn server_event_tagged() {
        let s = serde_json::to_string(&ServerEvent::Hello).unwrap();
        assert_eq!(s, r#"{"type":"hello"}"#);
        let e: ServerEvent = serde_json::from_str(
            r#"{"type":"message_created","message":{"id":1,"channel_id":2,"author":{"id":3,"name":"a"},"content":"hi","created_at":5}}"#,
        )
        .unwrap();
        assert!(matches!(e, ServerEvent::MessageCreated { message } if message.attachments.is_empty()));
        assert!(serde_json::from_str::<ServerEvent>(r#"{"type":"future_thing"}"#).is_err());
        assert_eq!(serde_json::to_string(&ServerEvent::FriendsChanged).unwrap(), r#"{"type":"friends_changed"}"#);
        let p: ServerEvent = serde_json::from_str(r#"{"type":"presence","user_id":3,"online":true}"#).unwrap();
        assert_eq!(p, ServerEvent::Presence { user_id: 3, online: true });
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.10.2"), Some((0, 10, 2)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn release_validation() {
        let mut r = ReleaseInfo {
            version: "0.2.0".into(),
            notes: String::new(),
            file: "murmur-0.2.0.exe".into(),
            sha256: "a".repeat(64),
            size: 1,
            installer: Some("murmur-setup-0.2.0.exe".into()),
            min_server: None,
            sig: None,
        };
        assert!(r.is_valid());
        r.file = "../x.exe".into();
        assert!(!r.is_valid());
        r.file = "x.exe".into();
        r.sha256 = "A".repeat(64);
        assert!(!r.is_valid());
        assert!(!safe_file_name(".hidden"));
        assert!(!safe_file_name("a/b"));
        assert!(!safe_file_name("a\\b"));
    }

    fn release(min_server: Option<&str>) -> ReleaseInfo {
        ReleaseInfo {
            version: "0.3.13".into(),
            notes: String::new(),
            file: "murmur-0.3.13.exe".into(),
            sha256: "a".repeat(64),
            size: 1,
            installer: None,
            min_server: min_server.map(Into::into),
            sig: None,
        }
    }

    #[test]
    fn release_signature() {
        use ed25519_dalek::{Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey = *key.verifying_key().as_bytes();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let mut r = release(None);
        assert!(!r.verify_with(&pubkey), "без подписи нельзя");
        r.sig = Some(hex(&key.sign(&r.signing_message()).to_bytes()));
        assert!(r.verify_with(&pubkey));
        assert!(!r.verify(), "чужой ключ издателя не подходит");
        // любое поле из подписанных — меняет сообщение
        let mut bad = r.clone();
        bad.sha256 = "b".repeat(64);
        assert!(!bad.verify_with(&pubkey));
        let mut bad = r.clone();
        bad.file = "murmur-evil.exe".into();
        assert!(!bad.verify_with(&pubkey));
        // заметки и min_server не подписываются
        let mut ok = r.clone();
        ok.notes = "другой текст".into();
        assert!(ok.verify_with(&pubkey));
        // мусор вместо подписи
        for s in ["", "zz", &"0".repeat(128), &"é".repeat(64)] {
            let mut g = r.clone();
            g.sig = Some(s.into());
            assert!(!g.verify_with(&pubkey));
        }
    }

    #[test]
    fn release_min_server_roundtrip() {
        let r: ReleaseInfo = serde_json::from_str(
            r#"{"version":"0.3.13","file":"murmur-0.3.13.exe","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1}"#,
        )
        .unwrap();
        assert_eq!(r.min_server, None);
        assert!(!serde_json::to_string(&r).unwrap().contains("min_server"));
        let with: ReleaseInfo = serde_json::from_value(serde_json::json!({
            "version": "0.3.13", "file": "murmur-0.3.13.exe", "sha256": "a".repeat(64), "size": 1, "min_server": "0.3.13"
        }))
        .unwrap();
        assert_eq!(with.min_server.as_deref(), Some("0.3.13"));
    }

    #[test]
    fn release_min_server_invalid() {
        assert!(release(Some("0.3.13")).is_valid());
        assert!(!release(Some("abc")).is_valid());
    }

    #[test]
    fn release_server_ok() {
        let r = release(Some("0.3.13"));
        assert!(!r.server_ok("0.3.12"));
        assert!(r.server_ok("0.3.13"));
        assert!(r.server_ok("0.4.0"));
        assert!(!r.server_ok("x"));
        assert!(release(None).server_ok("0.1.0"));
    }

    #[test]
    fn create_invite_defaults() {
        let r: CreateInviteRequest = serde_json::from_str("{}").unwrap();
        assert!(r.uses.is_none() && r.expires_in_hours.is_none());
    }
    #[test]
    fn old_channel_json_still_parses() {
        let c: Channel = serde_json::from_str(r#"{"id":1,"name":"a","kind":"voice"}"#).unwrap();
        assert_eq!(c.server_id, 0);
        assert_eq!(c.category_id, None);
        assert_eq!(c.position, 0);
        assert_eq!(c.user_limit, None);
    }

    fn roundtrip(e: ServerEvent, tag: &str) {
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.starts_with(&format!(r#"{{"type":"{tag}""#)), "{s}");
        let back: ServerEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn event_roundtrip() {
        let user = PublicUser { id: 2, name: "okabe".into(), display_name: Some("Окабэ".into()), avatar: None };
        let role = Role { id: 1, name: "Участник".into(), position: 0, permissions: perm::INVITE };
        let server = Server { id: 3, name: "Лаборатория".into(), icon: None, owner_id: 2, roles: vec![role.clone()], my_role_id: 1, description: None, banner: None, banner_image: None, color: None };
        let ch = Channel { id: 5, name: "General".into(), kind: ChannelKind::Voice, server_id: 3, category_id: Some(4), position: 1, user_limit: Some(5) };
        let layout = ChannelLayout { categories: vec![Category { id: 4, name: "Голос".into(), position: 0 }], channels: vec![ch] };
        roundtrip(ServerEvent::ServerUpserted { server }, "server_upserted");
        roundtrip(ServerEvent::ServerRemoved { server_id: 3 }, "server_removed");
        roundtrip(ServerEvent::MemberUpserted { server_id: 3, member: Member { user: user.clone(), role_id: 1, online: false, activity: None } }, "member_upserted");
        roundtrip(ServerEvent::MemberRemoved { server_id: 3, user_id: 2 }, "member_removed");
        roundtrip(ServerEvent::LayoutChanged { server_id: 3, layout }, "layout_changed");
        roundtrip(ServerEvent::Voice { server_id: 3, roster: VoiceRoster { channel_id: 5, user_ids: vec![2] } }, "voice");
        roundtrip(ServerEvent::UserUpdated { user }, "user_updated");
    }

    #[test]
    fn reactions_any_emoji() {
        assert!(reaction_valid("fire"));
        assert!(reaction_valid("🦆"));
        assert!(reaction_valid("❤️"));
        assert!(!reaction_valid("nope"));
        assert!(!reaction_valid("c:42"));
        assert!(!reaction_valid("a🦆"));
        assert!(!reaction_valid(&"x".repeat(40)));
        assert_eq!(reaction_name("🔥"), "fire");
        assert_eq!(reaction_name("❤️"), "heart");
        assert_eq!(reaction_name("🦆"), "🦆");
        assert_eq!(reaction_emoji("fire"), Some("🔥"));
        assert_eq!(reaction_emoji("🦆"), Some("🦆"));
        assert_eq!(reaction_emoji("zzz"), None);
    }

    #[test]
    fn perm_has() {
        assert!(perm::has(perm::ALL, perm::BAN));
        assert!(!perm::has(perm::INVITE, perm::KICK));
        assert_eq!(perm::ALL, 127);
    }

    #[test]
    fn shown_name() {
        let mut u = PublicUser { id: 1, name: "okabe".into(), display_name: None, avatar: None };
        assert_eq!(u.shown_name(), "okabe");
        u.display_name = Some(String::new());
        assert_eq!(u.shown_name(), "okabe");
        u.display_name = Some("Окабэ".into());
        assert_eq!(u.shown_name(), "Окабэ");
    }
}
