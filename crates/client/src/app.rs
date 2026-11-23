//! Окно клиента: вход по инвайту → каналы → комната, настройки звука. Вёрстка — design/mockup-phase3.html.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, TextEdit, Ui, pos2, vec2,
};
use egui_phosphor::regular as ph;
use proto::{AuthResponse, Channel, ChannelKind, InvitePreview, User};

use crate::api::{self, Api, ApiResult};
use crate::channels_ui::{self, ChannelsUi};
use crate::chat::Chat;
use crate::config::{NoiseMode, Settings, ThemePref};
use crate::desktop::{self, Window};
use crate::tray::Tray;
use crate::update::{self, Updater};
use crate::mictest::{self, MicTest};
use crate::profile_card;
use crate::people::{self, People};
use crate::picker::{self, Picker};
use crate::ptt::{self, Ptt};
use crate::servers_ui::{self, ServersUi};
use crate::kit::{self, DockStyle};
use crate::layers::Quality;
use crate::link::LinkState;
use crate::theme as t;
use crate::video::VideoHub;
use crate::voice::{self, AudioPrefs, Cmd, Conn, Peer, VoiceHandle, VoiceState};

/// (микрофон, вывод, шумоподавление, автогромкость, слышать себя).
type MicTestKey = (Option<String>, Option<String>, NoiseMode, bool, bool);

const CHANNELS_REFRESH: Duration = Duration::from_secs(60);

enum Msg {
    Me(ApiResult<User>),
    Auth(ApiResult<AuthResponse>),
    /// Проверка сервера на экране входа: (адрес, отвечает ли).
    Health(String, bool),
    /// Превью приглашения на экране входа: (код, куда зовут).
    Preview(String, Result<InvitePreview, String>),
    /// Профиль сохранён (`Ok(None)` — выбор файла отменён).
    Profile(Result<Option<User>, String>),
    /// Аватар/баннер сохранён; подпись — если видео пришлось урезать.
    ProfileImage(Result<Option<User>, String>, Option<String>),
    /// Картинка для аватара выбрана — открыть обрезку.
    /// Поле профиля сохранено (0 — имя, 1 — статус, 2 — о себе).
    ProfileField(usize, Result<User, String>),
    /// Картинка профиля выбрана: исходник и форма (аватар/баннер).
    AvatarPicked(Result<Option<crate::crop::Source>, String>, crate::crop::Shape),
    /// Установщик Murmur скачан (переход со старой установки).
    Legacy(Result<std::path::PathBuf, String>),
    /// Установщик для тихой переустановки как murmur.exe.
    Rename(Result<std::path::PathBuf, String>),
}

#[derive(PartialEq)]
enum Screen {
    /// Проверяем сохранённую сессию.
    Boot,
    Login,
    Main,
}

/// Что в центре окна: голосовой канал (сцена) или текстовый канал (чат). Голос при этом не прерывается.
#[derive(PartialEq, Clone, Copy, Debug)]
enum View {
    Voice,
    Text(i64),
    /// Личный диалог (id канала ЛС).
    Dm(i64),
    /// Экран «Друзья».
    Friends,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Login,
    Invite,
}

/// Разделы настроек.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SetTab {
    Profile,
    Account,
    Voice,
    Stream,
    Hotkeys,
    Look,
    Notify,
}

impl SetTab {
    fn icon(self) -> &'static str {
        match self {
            SetTab::Profile => ph::USER,
            SetTab::Account => ph::SHIELD,
            SetTab::Voice => ph::MICROPHONE,
            SetTab::Stream => ph::MONITOR_ARROW_UP,
            SetTab::Hotkeys => ph::KEYBOARD,
            SetTab::Look => ph::PALETTE,
            SetTab::Notify => ph::BELL,
        }
    }
}

/// (раздел, группа, название, подпись, слова для поиска — строки раздела, в нижнем регистре).
const SET_TABS: [(SetTab, &str, &str, &str, &str); 7] = [
    (SetTab::Profile, "ПОЛЬЗОВАТЕЛЬ", "Профиль", "Как вас видят другие", "аватар имя отображаемое о себе активность игра личные сообщения"),
    (SetTab::Account, "ПОЛЬЗОВАТЕЛЬ", "Аккаунт", "Логин, сервер и вход", "логин сервер пароль сеансы выйти"),
    (SetTab::Voice, "ПРИЛОЖЕНИЕ", "Голос и видео", "Устройства, микрофон и шумоподавление", "микрофон вывод звука наушники проверка рация голосом порог чувствительность шумоподавление эхоподавление громкость"),
    (SetTab::Stream, "ПРИЛОЖЕНИЕ", "Стрим экрана", "Качество, кодировщик, звук и окно в окне", "трансляция качество разрешение кадры звук"),
    (SetTab::Hotkeys, "ПРИЛОЖЕНИЕ", "Горячие клавиши", "Работают, даже когда окно свёрнуто", "клавиша рация сочетание"),
    (SetTab::Look, "ПРИЛОЖЕНИЕ", "Внешний вид", "Тема, анимации и звуки интерфейса", "тема тёмная светлая анимации индикатор речи пинг плитки кот звуки"),
    (SetTab::Notify, "ПРИЛОЖЕНИЕ", "Уведомления", "Трей, автозапуск и обновления", "трей автозапуск windows обновления версия"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerState {
    Checking,
    Up,
    Down,
}

struct LoginForm {
    tab: Tab,
    name: String,
    password: String,
    /// Ссылка или код приглашения как ввели.
    invite: String,
    show_password: bool,
    /// Ошибка «неверный ник или пароль» — под паролем, рамка поля красная.
    bad_password: bool,
    forgot_hint: bool,
    /// Поставить фокус на первое пустое поле (открытие экрана, смена вкладки).
    focus_pending: bool,
    server_state: ServerState,
    /// Какой адрес и когда проверяли.
    server_checked: Option<(String, Instant)>,
    /// Куда зовёт введённый код: (код, ответ).
    preview: Option<(String, Result<InvitePreview, String>)>,
    error: Option<String>,
    busy: bool,
    server_edit: Option<String>,
}

struct VolumePopup {
    identity: String,
    name: String,
    pos: Pos2,
    opened_at: Instant,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    ctx: egui::Context,
    settings: Settings,
    api: Api,
    tx: std_mpsc::Sender<Msg>,
    rx: std_mpsc::Receiver<Msg>,
    voice: VoiceHandle,
    ptt: Ptt,
    ptt_rx: std_mpsc::Receiver<u32>,
    screen: Screen,
    boot_error: Option<String>,
    /// Когда повторить запрос профиля при старте с сохранённой сессией (сервер был недоступен), и сколько попыток уже было.
    boot_retry: Option<Instant>,
    boot_attempts: u32,
    login: LoginForm,
    me: Option<User>,
    /// Серверы, раскладки, участники, голос (фаза 9.1).
    model: crate::model::Model,
    servers: ServersUi,
    channel_list: ChannelsUi,
    server_settings: crate::server_settings::ServerSettings,
    avatar_crop: Option<crate::crop::CropDialog>,
    /// Канал, из которого уже вышли, потому что он пропал (чтобы не слать Leave каждый кадр).
    left_gone: Option<i64>,
    /// Каналы активного сервера (из модели; в демо — свои).
    channels: Vec<Channel>,
    channels_at: Instant,
    settings_open: bool,
    /// Раздел настроек и строка поиска в их меню.
    set_tab: SetTab,
    set_query: String,
    /// Отладочный оверлей метрик (F3).
    metrics_open: bool,
    metrics_init: bool,
    popup: Option<VolumePopup>,
    mic_test: Option<MicTest>,
    /// Параметры, с которыми запущена проверка: (микрофон, вывод, шумодав, agc, слышать себя).
    mic_test_key: Option<MicTestKey>,
    mic_test_error: Option<String>,
    mic_level: Arc<AtomicU32>,
    mic_playback: bool,
    /// Пикер источника трансляции.
    picker: Option<Picker>,
    /// Кадры просматриваемых трансляций.
    video: VideoHub,
    /// Трансляция в «большом виде» (identity); `None` — сетка.
    focus: Option<String>,
    /// Полноэкранный просмотр трансляции `focus`.
    fullscreen: bool,
    /// Окно было развёрнуто до полного экрана (вернуть при выходе).
    fs_was_maximized: bool,
    /// Что последним сообщили движку о видимости/размере трансляций.
    views_sent: HashMap<String, (bool, u32, u32)>,
    /// Последнее движение мыши (в полном экране панель и курсор прячутся).
    pointer_moved_at: Instant,
    /// Какие трансляции нарисованы в текущем кадре и какого размера (физ. пиксели).
    views_frame: HashMap<String, (u32, u32)>,
    /// Мини-плеер трансляции («окно в окне»): выбор стрима, закрыт/закреплён; положение и ширина панели.
    pip: crate::pip::Pip,
    pip_pos: Option<Pos2>,
    pip_w: f32,
    /// Канал звонка, в котором плеер сбрасывался (новый вход → снова показывать).
    pip_chan: Option<i64>,
    /// Murmur неактивен с этого момента (для переезда плеера в окно ОС).
    pip_away: Option<Instant>,
    /// Ширина окна ОС с плеером.
    pip_win_w: f32,
    /// «Смотреть» нажато, движок ещё не подтвердил (фокус не сбрасывать).
    watch_pending: Option<(String, Instant)>,
    /// Текстовый чат.
    chat: Chat,
    /// Имена и аватары.
    people: People,
    friends: crate::friends::Friends,
    /// Список участников справа и карточка профиля.
    members: crate::members_ui::Members,
    /// Меня позвали в голосовой канал: (кто, канал, когда).
    summon: Option<(proto::PublicUser, i64, Instant)>,
    /// Во что я играю (фоновая проверка), что сейчас и что последним ушло на сервер.
    activity: Option<crate::activity::Detector>,
    my_game: crate::activity::Game,
    game_sent: Option<crate::activity::Game>,
    /// Отправка игры не удалась — sync_activity повторит.
    activity_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Поиск на экране друзей.
    friend_filter: String,
    /// Окошко «Задать статус» (текст).
    status_edit: Option<String>,
    /// Настройки → Профиль: «о себе».
    profile_about: String,
    /// ЛС и звонки другу.
    dms: crate::dms::Dms,
    unread: crate::unread::Unread,
    /// Звонок в ЛС: с какого момента я в нём один и никто не звонит (через несколько секунд — выйти).
    call_alone_since: Option<Instant>,
    /// Когда последний раз играл звонок / гудок.
    ring_at: Option<Instant>,
    /// В каком звонке ЛС был на прошлом кадре (звонок кончился — вернуться к диалогу).
    last_call: Option<i64>,
    /// Настройки → Профиль: поле имени, идёт сохранение, ошибка.
    profile_name: String,
    /// Настройки → Профиль: свой статус.
    profile_status: String,
    /// Автосохранение полей (имя, статус, о себе) и индикатор у заголовка.
    profile_saves: [crate::autosave::Autosave; 3],
    profile_save_state: crate::autosave::SaveState,
    profile_busy: bool,
    profile_error: Option<String>,
    /// Под картинками профиля: «Взяты первые 5 с…» (после видео).
    profile_note: Option<String>,
    /// Идёт сборка GIF из видео (другая подпись вместо «Сохраняю картинку…»).
    profile_video: bool,
    view: View,
    /// Окно (HWND) — прятать в трей и показывать.
    window: Window,
    /// Сколько запросов управления было на прошлом кадре (новый — поднять окно).
    rc_asked: usize,
    /// Фокус окна, последний отправленный движку (клавиатура управляющего — только в фокусе).
    rc_focus: Option<bool>,
    /// Значок в трее; `None` — не создался, тогда «закрыть» закрывает.
    tray: Option<Tray>,
    updater: Updater,
    /// Окно «Доступно обновление».
    update_open: bool,
    /// Выходим по-настоящему (меню трея, перезапуск после обновления): «закрыть» не прятать в трей.
    quitting: bool,
    /// Не удалось прописать автозапуск.
    autostart_error: Option<String>,
    /// Запуск из автозагрузки: до этого момента держать окно спрятанным (eframe показывает его после первого кадра).
    hide_until: Option<Instant>,
    /// Демо-режим без сервера (только debug, AMADEUS_DEMO).
    demo: bool,
    /// Запущены из старой установки Amadeus — окно «поставьте Murmur» (legacy.rs).
    legacy: bool,
    legacy_busy: bool,
    legacy_error: Option<String>,
    /// Тихая переустановка amadeus.exe → murmur.exe: скачанный установщик ждёт, пока не выйдем из звонка.
    rename_installer: Option<std::path::PathBuf>,
    rename_checked: bool,
    #[cfg(debug_assertions)]
    gallery: crate::gallery::GalleryState,
    /// Кто был в моём канале на прошлом кадре (канал, identity) — кто-то зашёл → уши кота в заголовке.
    prev_peers: (Option<i64>, Vec<String>),
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime) -> Self {
        t::install(&cc.egui_ctx);
        crate::frame::style_native_window(cc);
        let ctx = cc.egui_ctx.clone();
        start_watchdog(ctx.clone());
        #[allow(unused_mut)]
        let mut settings = Settings::load();
        let (pip_panel, pip_window) = (settings.pip_panel, settings.pip_window);
        crate::stream::set_prefs(settings.stream_encoder, settings.stream_bitrate_mbps);
        // Отладка: AMADEUS_THEME=dark|light — тема для скриншотов (не сохраняется).
        #[cfg(debug_assertions)]
        match std::env::var("AMADEUS_THEME").as_deref() {
            Ok("dark") => settings.theme = ThemePref::Dark,
            Ok("light") => settings.theme = ThemePref::Light,
            _ => {}
        }
        crate::anim::apply(&settings);
        crate::sounds::set_volume(if settings.sounds { settings.sounds_volume } else { 0 });
        let api = Api::new(&settings.server_url, settings.session_token.clone());
        let repaint: Arc<dyn Fn() + Send + Sync> = {
            let ctx = ctx.clone();
            Arc::new(move || ctx.request_repaint())
        };
        let prefs = AudioPrefs {
            input: settings.input_device.clone(),
            output: settings.output_device.clone(),
            aec: settings.echo_cancellation,
            noise: settings.noise,
            agc: settings.auto_gain,
            ptt_enabled: settings.ptt_enabled,
            sounds: settings.sounds,
            volumes: settings.user_volume.keys().map(|k| (k.clone(), settings.effective_volume(k))).collect::<HashMap<_, _>>(),
            stream_volumes: settings.stream_volume.clone(),
            output_volume: settings.output_volume,
        };
        push_gate(&settings);
        let video = VideoHub::new(&ctx);
        let chat = Chat::new(rt.handle().clone(), ctx.clone(), api.clone());
        let people = People::new(rt.handle().clone(), ctx.clone(), api.clone());
        let friends = crate::friends::Friends::new(rt.handle().clone(), ctx.clone(), api.clone());
        let members = crate::members_ui::Members::new(rt.handle().clone(), ctx.clone(), api.clone());
        let dms = crate::dms::Dms::new(rt.handle().clone(), ctx.clone(), api.clone());
        let servers = ServersUi::new(rt.handle().clone(), ctx.clone(), api.clone());
        let channel_list = ChannelsUi::new(rt.handle().clone(), ctx.clone(), api.clone());
        let server_settings = crate::server_settings::ServerSettings::new(rt.handle().clone(), ctx.clone(), api.clone());
        let updater = Updater::new(rt.handle().clone(), ctx.clone(), api.clone());
        let window = Window::from_cc(cc);
        desktop::mark_window(window);
        let tray = Tray::new(window, ctx.clone());
        if tray.is_none() {
            desktop::show(window); // без трея окно не прячем (даже при запуске из автозагрузки)
        }
        let autostart_error = if desktop::is_installed() { desktop::set_autostart(settings.autostart).err() } else { None };
        let voice = voice::spawn(rt.handle(), api.clone(), prefs, video.clone(), repaint.clone());
        let (ptt_tx, ptt_rx) = std_mpsc::channel();
        let ptt = Ptt::start(settings.ptt_key, settings.ptt_enabled, voice.sender(), ptt_tx, repaint);
        voice.send(Cmd::RcPttKey(settings.ptt_key));
        // Отладка: AMADEUS_START_DEAF=1 — второй клиент на том же ПК без заводки через колонки.
        #[cfg(debug_assertions)]
        if std::env::var_os("AMADEUS_START_DEAF").is_some() {
            voice.send(Cmd::SetDeafened(true));
        }
        let (tx, rx) = std_mpsc::channel();
        let has_token = settings.session_token.is_some();
        let login = LoginForm {
            tab: if settings.last_name.is_empty() { Tab::Invite } else { Tab::Login },
            name: settings.last_name.clone(),
            password: String::new(),
            invite: String::new(),
            show_password: false,
            bad_password: false,
            forgot_hint: false,
            focus_pending: true,
            server_state: ServerState::Checking,
            server_checked: None,
            preview: None,
            error: None,
            busy: false,
            server_edit: None,
        };
        let mut app = Self {
            rt,
            ctx,
            settings,
            api,
            tx,
            rx,
            voice,
            ptt,
            ptt_rx,
            screen: if has_token { Screen::Boot } else { Screen::Login },
            boot_error: None,
            boot_retry: None,
            boot_attempts: 0,
            login,
            me: None,
            model: Default::default(),
            servers,
            channel_list,
            server_settings,
            avatar_crop: None,
            left_gone: None,
            channels: Vec::new(),
            channels_at: Instant::now(),
            settings_open: false,
            set_tab: SetTab::Voice,
            set_query: String::new(),
            metrics_open: false,
            metrics_init: false,
            popup: None,
            mic_test: None,
            mic_test_key: None,
            mic_test_error: None,
            mic_level: Arc::new(AtomicU32::new(0)),
            mic_playback: false,
            picker: None,
            video,
            focus: None,
            fullscreen: false,
            fs_was_maximized: false,
            views_sent: HashMap::new(),
            pointer_moved_at: Instant::now(),
            views_frame: HashMap::new(),
            pip: crate::pip::Pip::default(),
            pip_pos: pip_panel.map(|(x, y, _)| pos2(x, y)),
            pip_w: pip_panel.map_or(320.0, |(_, _, w)| w),
            pip_chan: None,
            pip_away: None,
            pip_win_w: pip_window.map_or(400.0, |(_, _, w)| w),
            watch_pending: None,
            chat,
            people,
            members,
            summon: None,
            activity: None,
            activity_failed: Default::default(),
            my_game: None,
            game_sent: None,
            friend_filter: String::new(),
            status_edit: None,
            profile_about: String::new(),
            friends,
            dms,
            unread: Default::default(),
            call_alone_since: None,
            ring_at: None,
            last_call: None,
            profile_name: String::new(),
            profile_status: String::new(),
            profile_saves: Default::default(),
            profile_save_state: Default::default(),
            profile_busy: false,
            profile_error: None,
            profile_note: None,
            profile_video: false,
            view: View::Voice,
            window,
            rc_asked: 0,
            rc_focus: None,
            tray,
            updater,
            update_open: false,
            quitting: false,
            autostart_error,
            hide_until: None,
            demo: false,
            legacy: crate::legacy::detect(),
            legacy_busy: false,
            legacy_error: None,
            rename_installer: None,
            rename_checked: false,
            #[cfg(debug_assertions)]
            gallery: Default::default(),
            prev_peers: (None, Vec::new()),
        };
        if app.tray.is_some() && std::env::args().any(|a| a == desktop::ARG_TRAY) {
            // eframe показывает окно после первого кадра; свёрнутое — покажется только в панели задач,
            // а через пару кадров уйдёт в трей (на экране ничего не мелькнёт)
            desktop::minimize_inactive(window);
            app.hide_until = Some(Instant::now() + Duration::from_secs(3));
        }
        if has_token {
            app.fetch_me();
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("AMADEUS_DEMO").is_some() {
            app.start_demo();
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("AMADEUS_OPEN_SETTINGS").is_some() {
            app.open_settings();
            // AMADEUS_SETTINGS_TAB=<номер раздела 0–6> — какой раздел открыть
            if let Some(i) = std::env::var("AMADEUS_SETTINGS_TAB").ok().and_then(|v| v.parse::<usize>().ok()) {
                app.set_tab = SET_TABS[i.min(SET_TABS.len() - 1)].0;
            }
        }
        #[cfg(debug_assertions)]
        if let Ok(tab) = std::env::var("AMADEUS_OPEN_FRIENDS") {
            app.friends.debug_open(&tab);
        }
        app
    }

    /// Отладка: AMADEUS_DEMO=1 — главный экран без сервера (фиктивные «я», каналы и чат) для скриншотов.
    #[cfg(debug_assertions)]
    fn start_demo(&mut self) {
        self.demo = true;
        self.set_me(User { id: 1, name: "tester".into(), is_admin: false, display_name: Some("Тестер".into()), avatar: None, dm_from_servers: false, ..Default::default() });
        let ch = |id: i64, name: &str, kind: ChannelKind, position: i32| proto::Channel {
            id,
            name: name.into(),
            kind,
            server_id: 1,
            category_id: None,
            position,
            user_limit: None,
        };
        self.model.set_servers(vec![proto::Server {
            id: 1,
            name: "Лаборатория".into(),
            icon: None,
            owner_id: 1,
            roles: vec![proto::Role { id: 1, name: "Владелец".into(), position: 3, permissions: proto::perm::ALL }],
            my_role_id: 1,
            ..Default::default()
        }]);
        self.channels = vec![
            ch(3, "общий", ChannelKind::Text, 0),
            ch(4, "учёба", ChannelKind::Text, 1),
            ch(1, "General", ChannelKind::Voice, 2),
            ch(2, "Тихая", ChannelKind::Voice, 3),
        ];
        let msg = |id: i64, author: (i64, &str), content: &str, mins_ago: i64| proto::Message {
            id,
            channel_id: 3,
            author: proto::Author { id: author.0, name: author.1.into(), display_name: None, avatar: None },
            content: content.into(),
            created_at: chrono::Utc::now().timestamp() - mins_ago * 60,
            edited_at: None,
            reply_to: None,
            attachments: Vec::new(),
            reactions: if id == 2 { vec![proto::Reaction { name: "fire".into(), users: vec![1, 2] }, proto::Reaction { name: "+1".into(), users: vec![3] }, proto::Reaction { name: "🦆".into(), users: vec![2] }] } else { Vec::new() },
            sticker: None,
        };
        self.chat.demo_seed(3, vec![
            msg(1, (2, "mayuri"), "Туттуру! Кто сегодня в катку?", 42),
            msg(2, (3, "kurisu"), "Я после пар, часов в восемь.", 40),
            msg(3, (1, "tester"), "Го в катку через 5 минут? 😎🎮 Кто не придёт — тот 🐢. Флаг 🇷🇺, семья 👨‍👩‍👧, 1️⃣ и ❤ — всё цветное.", 2),
            msg(5, (3, "kurisu"), "🔥🔥", 1),
            proto::Message { sticker: Some("cat-hello".into()), ..msg(4, (2, "mayuri"), "[стикер: Кот машет]", 1) },
        ]);
        // Отладка (скриншоты): AMADEUS_OPEN_STICKERS=1 — открыть выбор стикеров.
        if std::env::var_os("AMADEUS_OPEN_STICKERS").is_some() {
            self.chat.debug_open_stickers();
        }
        // AMADEUS_OPEN_EMOJI=1 — открыть панель эмодзи.
        if std::env::var_os("AMADEUS_OPEN_EMOJI").is_some() {
            self.chat.debug_open_emoji();
        }
        self.chat.demo_seed(4, Vec::new());
        if let Some(id) = std::env::var("AMADEUS_OPEN_TEXT").ok().and_then(|v| v.parse::<i64>().ok()) {
            self.view = View::Text(id);
        }
        self.screen = Screen::Main;
    }

    // ------------------------------------------------------------ фон

    fn spawn<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = T> + Send + 'static,
        wrap: impl FnOnce(T) -> Msg + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let r = fut.await;
            let _ = tx.send(wrap(r));
            ctx.request_repaint();
        });
    }

    fn fetch_me(&mut self) {
        self.boot_error = None;
        self.boot_retry = None;
        let api = self.api.clone();
        self.spawn(async move { api.me().await }, Msg::Me);
    }

    /// Перечитать серверы, каналы, участников и голос.
    fn fetch_channels(&mut self) {
        self.channels_at = Instant::now();
        self.servers.reload(self.model.seq());
    }

    /// Первая загрузка серверов: выбрать сохранённый, отладочные автодействия.
    fn on_servers_loaded(&mut self) {
        if let Some(id) = self.settings.last_server {
            self.model.set_active(id);
        }
        self.sync_channels();
        // Отладка: AMADEUS_AUTOJOIN=<id канала> — сразу зайти (только debug-сборка).
        #[cfg(debug_assertions)]
        if let Some(id) = std::env::var("AMADEUS_AUTOJOIN").ok().and_then(|v| v.parse::<i64>().ok()) {
            let all = self.model.servers.iter().flat_map(|s| self.model.channels(s.id)).collect::<Vec<_>>();
            if let Some(ch) = all.into_iter().find(|c| c.id == id) {
                self.model.set_active(ch.server_id);
                self.sync_channels();
                self.voice.send(Cmd::Join(ch));
            }
        }
        // Отладка: AMADEUS_OPEN_TEXT=<id канала> — сразу открыть текстовый канал.
        #[cfg(debug_assertions)]
        if let Some(id) = std::env::var("AMADEUS_OPEN_TEXT").ok().and_then(|v| v.parse::<i64>().ok()) {
            self.view = View::Text(id);
        }
        // Отладка (скриншоты): AMADEUS_OPEN_SERVER_SETTINGS=1 — настройки активного сервера;
        // AMADEUS_OPEN_PROFILE=<id> — карточка профиля человека с активного сервера.
        #[cfg(debug_assertions)]
        if let Some(sid) = self.model.active {
            if std::env::var_os("AMADEUS_OPEN_SERVER_SETTINGS").is_some() {
                self.server_settings.open(&self.model, sid);
            }
            if let Some(uid) = std::env::var("AMADEUS_OPEN_PROFILE").ok().and_then(|v| v.parse::<i64>().ok()) {
                self.members.open_card(uid, pos2(560.0, 120.0), Some(sid));
            }
        }
    }

    /// Каналы активного сервера — из модели; открытый текстовый канал пропал — к сцене.
    fn sync_channels(&mut self) {
        if self.demo {
            return;
        }
        self.channels = self.model.active.map(|id| self.model.channels(id)).unwrap_or_default();
        if let View::Text(id) = self.view
            && !self.channels.iter().any(|c| c.id == id && c.kind == ChannelKind::Text)
        {
            self.view = View::Voice;
        }
    }

    /// Сервер ушёл (кик, бан, удалён) или канал удалён — выйти из голоса самому, не дожидаясь сервера.
    fn leave_if_gone(&mut self, vs: &VoiceState) {
        if self.demo || !self.servers.loaded_once() {
            return;
        }
        let active = matches!(vs.conn, Conn::Connecting | Conn::Connected | Conn::Reconnecting | Conn::Rejoining { .. });
        if vs.channel.as_ref().is_some_and(|ch| !self.model.voice_channel_gone(ch)) {
            self.left_gone = None;
        }
        if let Some(ch) = &vs.channel
            && active
            && self.model.voice_channel_gone(ch)
            && self.left_gone != Some(ch.id)
        {
            log::info!("канал {} больше недоступен — выхожу из голоса", ch.id);
            self.left_gone = Some(ch.id);
            self.voice.send(Cmd::Leave);
        }
    }

    fn select_server(&mut self, id: i64) {
        self.model.set_active(id);
        // из личного диалога или друзей — к серверу
        if matches!(self.view, View::Dm(_) | View::Friends) {
            self.view = View::Voice;
        }
        if self.settings.last_server != Some(id) {
            self.settings.last_server = Some(id);
            self.settings.save();
        }
        self.sync_channels();
    }

    fn set_me(&mut self, user: User) {
        self.people.learn(people::public(&user));
        // поля профиля — только при входе (иначе ответ на загрузку аватара затёр бы то, что сейчас печатают)
        if self.me.as_ref().map(|m| m.id) != Some(user.id) {
            self.profile_name = user.display_name.clone().unwrap_or_default();
            self.profile_status = user.status.clone().unwrap_or_default();
            self.profile_about = user.about.clone().unwrap_or_default();
            self.profile_saves = Default::default();
            self.profile_save_state = Default::default();
        }
        self.model.me = Some(user.id);
        self.chat.set_me(user.id);
        self.chat.set_recent_emoji(self.settings.recent_emoji.clone());
        self.me = Some(user);
    }

    fn set_api(&mut self, api: Api) {
        self.api = api.clone();
        self.chat.set_api(api.clone());
        self.people.set_api(api.clone());
        self.friends.set_api(api.clone());
        self.members.set_api(api.clone());
        self.game_sent = None;
        self.summon = None;
        self.dms.set_api(api.clone());
        self.unread = Default::default();
        self.servers.set_api(api.clone());
        self.channel_list.set_api(api.clone());
        self.server_settings.set_api(api.clone());
        self.model = Default::default();
        self.updater.set_api(api.clone());
        self.view = View::Voice;
        self.voice.send(Cmd::SetApi(api));
    }

    fn handle_msgs(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Me(Ok(user)) => {
                    self.boot_attempts = 0;
                    self.set_me(user);
                    self.screen = Screen::Main;
                    self.fetch_channels();
                }
                Msg::Me(Err(e)) => {
                    if e.is_unauthorized() {
                        self.settings.session_token = None;
                        self.settings.save();
                        self.set_api(self.api.with_token(None));
                        self.screen = Screen::Login;
                    } else {
                        self.boot_error = Some(e.message);
                        // автоповтор с нарастающей паузой: 3, 6, 12, 30 с (и дальше 30 с)
                        let wait = [3u64, 6, 12, 30][(self.boot_attempts as usize).min(3)];
                        self.boot_attempts += 1;
                        self.boot_retry = Some(Instant::now() + Duration::from_secs(wait));
                    }
                }
                Msg::ProfileField(i, res) => {
                    let ok = res.is_ok();
                    self.profile_saves[i].done(ok);
                    match res {
                        Ok(user) => {
                            self.members.forget(user.id);
                            self.set_me(user);
                        }
                        Err(e) => {
                            log::warn!("профиль: не сохранилось поле {i}: {e}");
                            self.profile_save_state = crate::autosave::SaveState::Failed;
                        }
                    }
                    // правили во время отправки и уже закрыли настройки — дослать сейчас
                    let now = self.ctx.input(|i| i.time);
                    if self.profile_saves[i].due(now) && !self.settings_open {
                        self.profile_send(i);
                    }
                    if ok && !self.profile_saves.iter().any(|s| s.busy()) {
                        self.profile_save_state = crate::autosave::SaveState::Saved;
                    }
                }
                Msg::ProfileImage(res, note) => {
                    self.profile_busy = false;
                    self.profile_video = false;
                    match res {
                        Ok(Some(user)) => {
                            self.profile_error = None;
                            self.profile_note = note;
                            self.set_me(user);
                        }
                        Ok(None) => {}
                        Err(e) => self.profile_error = Some(e),
                    }
                }
                Msg::Profile(res) => {
                    self.profile_busy = false;
                    match res {
                        Ok(Some(user)) => {
                            self.profile_error = None;
                            self.profile_note = None;
                            self.set_me(user);
                        }
                        Ok(None) => {}
                        Err(e) => self.profile_error = Some(e),
                    }
                }
                Msg::Auth(res) => {
                    self.login.busy = false;
                    match res {
                        Ok(auth) => {
                            // «Не выходить» выключено — сеанс только до закрытия приложения
                            self.settings.session_token = self.settings.stay_signed_in.then(|| auth.token.clone());
                            self.login.server_state = ServerState::Up;
                            self.login.bad_password = false;
                            self.settings.last_name = auth.user.name.clone();
                            self.settings.save();
                            self.set_api(self.api.with_token(Some(auth.token)));
                            self.set_me(auth.user);
                            self.login.password.clear();
                            self.login.invite.clear();
                            self.login.preview = None;
                            self.login.error = None;
                            self.screen = Screen::Main;
                            self.fetch_channels();
                        }
                        Err(e) => {
                            self.login.bad_password = e.code == "bad_credentials";
                            if e.code == "network" {
                                self.login.server_state = ServerState::Down;
                                self.login.error = Some("Сервер не отвечает. Проверьте интернет или адрес сервера.".into());
                            } else {
                                self.login.error = Some(e.message);
                            }
                        }
                    }
                }
                Msg::Health(url, ok) => {
                    if url == self.settings.server_url {
                        self.login.server_state = if ok { ServerState::Up } else { ServerState::Down };
                    }
                }
                Msg::AvatarPicked(res, shape) => match res {
                    Ok(Some(src)) => {
                        let title = if shape == crate::crop::Shape::Banner { "Баннер" } else { "Аватар" };
                        self.avatar_crop = Some(crate::crop::CropDialog::new_profile(&self.ctx, title, src, shape));
                    }
                    Ok(None) => self.profile_busy = false,
                    Err(e) => {
                        self.profile_busy = false;
                        self.profile_error = Some(e);
                    }
                },
                Msg::Preview(code, res) => {
                    self.login.preview = Some((code, res));
                }
                Msg::Rename(res) => match res {
                    Ok(path) => self.rename_installer = Some(path),
                    Err(e) => log::warn!("переустановка как murmur.exe: установщик не скачан: {e} — попробую при следующем запуске"),
                },
                Msg::Legacy(res) => {
                    self.legacy_busy = false;
                    match res {
                        Ok(path) if desktop::run_file(&path) => {
                            log::info!("установщик Murmur запущен — выхожу");
                            <Self as eframe::App>::on_exit(self);
                        }
                        Ok(_) => self.legacy_error = Some("не удалось запустить установщик".into()),
                        Err(e) => self.legacy_error = Some(e),
                    }
                }
            }
        }
        while let Ok(vk) = self.ptt_rx.try_recv() {
            if vk != 0x1B {
                self.settings.ptt_key = vk;
                self.ptt.set_key(vk);
                self.voice.send(Cmd::RcPttKey(vk));
                self.settings.save();
            }
        }
    }

    /// Новое сообщение (канал сервера или ЛС): непрочитанные, звук ЛС.
    fn on_message(&mut self, m: &proto::Message) {
        let me = self.me.as_ref().map(|u| u.id);
        let dm = self.dms.on_message(m);
        let server = if dm {
            None
        } else {
            self.model.servers.iter().map(|s| s.id).find(|s| self.model.channels(*s).iter().any(|c| c.id == m.channel_id))
        };
        if !dm && server.is_none() {
            // новый диалог (мне написали впервые) — перечитать список
            self.dms.fetch_list();
        }
        self.unread.on_message(m, server, me);
        if server.is_none() && Some(m.author.id) != me {
            let viewing = self.view == View::Dm(m.channel_id) && self.ctx.input(|i| i.focused);
            if !viewing {
                crate::sounds::play(crate::sounds::Sound::DirectMessage);
            }
        }
    }

    /// Открытый канал на виду — прочитан; прочитанное — на сервер.
    fn sync_read(&mut self, focused: bool) {
        let open = match self.view {
            View::Text(id) | View::Dm(id) => Some(id),
            View::Voice | View::Friends => None,
        };
        if focused
            && let Some(id) = open
            && self.unread.count(id) > 0
        {
            self.unread.mark_read(id);
        }
        for (ch, mid) in self.unread.take_to_send() {
            let api = self.api.clone();
            self.rt.spawn(async move {
                if let Err(e) = api.mark_read(ch, mid).await {
                    log::warn!("канал {ch}: не удалось отметить прочитанным: {e}");
                }
            });
        }
    }

    /// Звонки в ЛС: окно входящего, звонок/гудки, выход из звонка, где больше никого нет.
    fn ui_calls(&mut self, ui: &mut Ui, vs: &VoiceState) {
        use crate::sounds::Sound;
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return };
        let in_call = vs.channel.as_ref().filter(|c| crate::dms::is_dm(c) && vs.conn != Conn::Idle).map(|c| c.id);
        // звонок кончился, а на экране пустая сцена — показать диалог
        if let Some(prev) = self.last_call
            && in_call.is_none()
            && self.view == View::Voice
            && vs.channel.is_none()
        {
            self.view = View::Dm(prev);
        }
        self.last_call = in_call;
        let ringing_out = in_call.is_some_and(|id| {
            vs.peers.len() <= 1 && self.dms.get(id).and_then(|d| d.call.as_ref()).is_some_and(|c| c.ringing && c.caller_id == me)
        });
        if let Some(dm) = self.dms.incoming(me).filter(|d| Some(d.id) != in_call).cloned() {
            self.ring(Sound::Ring, Duration::from_millis(2600));
            match crate::dms::incoming_window(ui.ctx(), &self.people, &dm) {
                Some(true) => {
                    self.voice.send(Cmd::Join(crate::dms::call_channel(&dm)));
                    self.view = View::Dm(dm.id);
                }
                Some(false) => {
                    self.dms.dismiss_call(dm.id);
                    let api = self.api.clone();
                    self.rt.spawn(async move {
                        if let Err(e) = api.dm_hang_up(dm.id).await {
                            log::warn!("звонок {}: не удалось отклонить: {e}", dm.id);
                        }
                    });
                }
                None => {}
            }
        } else if ringing_out {
            self.ring(Sound::RingBack, Duration::from_millis(3000));
        } else {
            self.ring_at = None;
        }

        // Звонок в ЛС, а собеседника нет и никто не звонит (не ответили, отклонили, положил трубку) — через 5 с выйти.
        // Пауза нужна: ответивший заходит в комнату не мгновенно.
        let alone = in_call.filter(|_| vs.conn == Conn::Connected && vs.peers.len() <= 1).filter(|id| {
            !self.dms.get(*id).and_then(|d| d.call.as_ref()).is_some_and(|c| c.ringing)
        });
        match alone {
            Some(id) => {
                let since = *self.call_alone_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(5) {
                    log::info!("звонок {id}: собеседника нет — выхожу");
                    self.call_alone_since = None;
                    self.voice.send(Cmd::Leave);
                    if self.view == View::Voice {
                        self.view = View::Dm(id);
                    }
                } else {
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
            }
            None => self.call_alone_since = None,
        }
    }

    /// Повторяющийся звук звонка: не чаще `every`.
    fn ring(&mut self, s: crate::sounds::Sound, every: Duration) {
        if self.ring_at.is_none_or(|t| t.elapsed() >= every) {
            crate::sounds::play(s);
            self.ring_at = Some(Instant::now());
        }
        self.ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn logout(&mut self) {
        self.voice.send(Cmd::Leave);
        // дослать несохранённое в профиле, пока токен ещё действует
        self.profile_flush();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let _ = api.logout().await;
        });
        self.settings.session_token = None;
        self.settings.save();
        self.set_api(self.api.with_token(None));
        self.me = None;
        self.model = Default::default();
        self.channels.clear();
        self.settings_open = false;
        self.ptt.cancel_capture();
        self.set_query.clear();
        self.stop_mic_test();
        self.popup = None;
        self.login.tab = Tab::Login;
        self.login.name = self.settings.last_name.clone();
        self.screen = Screen::Login;
    }

    fn submit_login(&mut self) {
        let f = &mut self.login;
        let name = f.name.trim().to_string();
        let pass = f.password.clone();
        let code = api::invite_from_input(&f.invite);
        f.error = None;
        f.bad_password = false;
        if name.chars().count() < proto::limits::NAME_MIN {
            f.error = Some("Введите ник.".into());
            return;
        }
        if f.tab == Tab::Invite {
            if code.len() != 12 {
                f.error = Some("Вставьте ссылку или код приглашения (12 знаков, XXXX-XXXX-XXXX).".into());
                return;
            }
            if pass.chars().count() < proto::limits::PASSWORD_MIN {
                f.error = Some(format!("Пароль — минимум {} символов.", proto::limits::PASSWORD_MIN));
                return;
            }
        } else if pass.is_empty() {
            f.error = Some("Введите пароль.".into());
            return;
        }
        f.busy = true;
        let api = self.api.with_token(None);
        let tab = f.tab;
        self.spawn(
            async move {
                if tab == Tab::Invite {
                    api.register(&api::format_invite(&code), &name, &pass).await
                } else {
                    api.login(&name, &pass).await
                }
            },
            Msg::Auth,
        );
    }

    fn apply_server(&mut self, raw: &str) {
        let mut url = raw.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            url = crate::config::DEFAULT_SERVER.to_string();
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            url = format!("https://{url}");
        }
        self.settings.server_url = url.clone();
        self.settings.save();
        self.set_api(Api::new(&url, None));
    }

    // ------------------------------------------------------------ экран входа

    fn ui_boot(&mut self, ui: &mut Ui) {
        if self.boot_error.is_some()
            && let Some(at) = self.boot_retry
        {
            if Instant::now() >= at {
                self.fetch_me();
            } else {
                ui.ctx().request_repaint_after(at - Instant::now());
            }
        }
        egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg)).show(ui, |ui| {
            let r = ui.max_rect();
            let area = Rect::from_center_size(r.center(), vec2(460.0, 320.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| match &self.boot_error {
                None => {
                    kit::cat_state(ui, kit::Cat::Loading, "Подключаемся…", "Ещё секунду", None);
                }
                Some(err) => {
                    let err = err.clone();
                    kit::cat_state(ui, kit::Cat::Offline, "Связь пропала", "Пробуем переподключиться", None);
                    ui.add_space(12.0);
                    kit::notice(ui, kit::Notice::Error, &err, false);
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(130.0);
                        if kit::button(ui, kit::ButtonKind::Primary, "Повторить", None).clicked() {
                            self.fetch_me();
                        }
                        if kit::button(ui, kit::ButtonKind::Ghost, "Выйти", None).clicked() {
                            self.logout();
                        }
                    });
                }
            });
        });
    }

    fn ui_login(&mut self, ui: &mut Ui) {
        self.check_server_health();
        // Отладка (скриншоты): AMADEUS_LOGIN_SUBMIT="ник|пароль" — через 2 с заполнить форму и нажать «Войти».
        #[cfg(debug_assertions)]
        if ui.input(|i| i.time) > 2.0
            && let Some(v) = std::env::var("AMADEUS_LOGIN_SUBMIT").ok()
            && !self.login.busy
            && self.login.error.is_none()
            && let Some((n, pw)) = v.split_once('|')
        {
            self.login.name = n.into();
            self.login.password = pw.into();
            self.submit_login();
        }
        egui::Panel::right("login-form")
            .exact_size(420.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t::p().surface).inner_margin(Margin::symmetric(40, 32)))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().vline(r.left() - 40.0, ui.clip_rect().y_range(), Stroke::new(1.0, t::p().border));
                // форма целиком по центру по вертикали: высота — с прошлого кадра
                let hid = Id::new("login-form-h");
                let h: f32 = ui.data(|d| d.get_temp(hid)).unwrap_or(0.0);
                ui.add_space(((ui.available_height() - h) / 2.0).max(0.0));
                // мерить содержимое (scope), а не min_rect панели — он во всю высоту
                let nh = ui.scope(|ui| self.ui_login_form(ui)).response.rect.height();
                if (nh - h).abs() > 0.5 {
                    ui.data_mut(|d| d.insert_temp(hid, nh));
                    ui.ctx().request_repaint();
                }
            });
        egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg).inner_margin(Margin::symmetric(64, 48))).show(ui, |ui| {
            let r = ui.max_rect();
            let p = ui.painter().clone();
            login_waves(&p, r.expand2(vec2(64.0, 48.0)));
            t::wordmark(ui, 40.0);
            // кот, заголовок и подпись — одним блоком по центру по вертикали
            let wrap = r.width().min(560.0);
            let title = p.layout("Созвоны, игры и учёба —\nсо своими.".to_string(), t::xbold(34.0), t::p().text, wrap);
            let sub = p.layout(
                "Голос, трансляции экрана и чат. Попасть сюда можно только по приглашению.".to_string(),
                t::font(15.0),
                t::p().text_muted,
                wrap,
            );
            let cat = vec2(124.0, 100.0);
            let h = cat.y + 28.0 + title.size().y + 14.0 + sub.size().y;
            let mut y = r.center().y - h / 2.0;
            kit::cat_figure(&p, Rect::from_min_size(pos2(r.left() + 4.0, y), cat), kit::Cat::Hello);
            y += cat.y + 28.0;
            let th = title.size().y;
            p.galley(pos2(r.left(), y), title, t::p().text);
            y += th + 14.0;
            p.galley(pos2(r.left(), y), sub, t::p().text_muted);
            // строка сервера: точка статуса, адрес, «сменить сервер»
            let bottom = Rect::from_min_max(pos2(r.left(), r.bottom() - 28.0), r.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(bottom), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match self.login.server_edit.take() {
                        None => {
                            let (dot, word) = match self.login.server_state {
                                ServerState::Up => (t::p().success, "доступен"),
                                ServerState::Down => (t::p().danger, "не отвечает"),
                                ServerState::Checking => (t::p().text_faint, "проверяем…"),
                            };
                            let (d, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            ui.painter().circle_filled(d.center(), 4.0, dot);
                            let host = self.settings.server_url.trim_start_matches("https://").trim_start_matches("http://").to_string();
                            ui.label(RichText::new(format!("{} {host}", ph::HARD_DRIVES)).color(t::p().text_muted).size(13.0));
                            ui.label(RichText::new(format!("· {word} ·")).color(t::p().text_faint).size(13.0));
                            let link = ui.add(egui::Label::new(RichText::new("сменить сервер").size(13.0).color(t::p().accent_text).underline()).sense(Sense::click()));
                            if link.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                self.login.server_edit = Some(self.settings.server_url.clone());
                            }
                        }
                        Some(mut s) => {
                            ui.label(RichText::new(ph::HARD_DRIVES).color(t::p().text_muted));
                            let r = ui.add(kit::text_field(&mut s, 280.0).hint_text("https://api.example.ru"));
                            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if kit::button(ui, kit::ButtonKind::Primary, "OK", None).clicked() || enter {
                                self.apply_server(&s);
                            } else if kit::button(ui, kit::ButtonKind::Ghost, "Отмена", None).clicked() {
                            } else {
                                self.login.server_edit = Some(s);
                            }
                        }
                    }
                });
            });
        });
    }

    /// Проверка сервера для строки статуса на входе: при открытии экрана, смене адреса и раз в 10 с, пока недоступен.
    fn check_server_health(&mut self) {
        let url = self.settings.server_url.clone();
        let stale = self.login.server_checked.as_ref().is_none_or(|(u, at)| {
            *u != url || (self.login.server_state == ServerState::Down && at.elapsed() > Duration::from_secs(10))
        });
        if !stale {
            return;
        }
        if self.login.server_checked.as_ref().is_none_or(|(u, _)| *u != url) {
            self.login.server_state = ServerState::Checking;
        }
        self.login.server_checked = Some((url.clone(), Instant::now()));
        let api = self.api.with_token(None);
        self.spawn(async move { api.health().await.is_ok() }, move |ok| Msg::Health(url, ok));
        self.ctx.request_repaint_after(Duration::from_secs(10));
    }

    fn ui_login_form(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let p = t::p();
        // вкладки
        let (tabs_rect, _) = ui.allocate_exact_size(vec2(w, 42.0), Sense::hover());
        ui.painter().rect_filled(tabs_rect, CornerRadius::same(10), p.rail);
        let half = (tabs_rect.width() - 8.0) / 2.0;
        for (i, (tab, label)) in [(Tab::Login, "Вход"), (Tab::Invite, "У меня приглашение")].into_iter().enumerate() {
            let rr = Rect::from_min_size(tabs_rect.min + vec2(4.0 + half * i as f32, 4.0), vec2(half, 34.0));
            let resp = ui.interact(rr, Id::new(("tab", i)), Sense::click());
            let on = self.login.tab == tab;
            if on {
                // тёмная тема — светлее подложки (row_active), светлая — белая вкладка
                let fill = if p.bg.r() < 128 { p.row_active } else { p.bg };
                ui.painter().rect_filled(rr, CornerRadius::same(8), fill);
            }
            let color = if on || resp.hovered() { p.text } else { p.text_muted };
            ui.painter().text(rr.center(), Align2::CENTER_CENTER, label, t::bold(14.0), color);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                self.login.tab = tab;
                self.login.error = None;
                self.login.bad_password = false;
                self.login.focus_pending = true;
            }
        }
        ui.add_space(26.0);
        let invite = self.login.tab == Tab::Invite;
        let (head, sub) = if invite { ("Вас позвали", "Вставьте ссылку или код приглашения") } else { ("С возвращением", "Войдите, чтобы продолжить") };
        ui.label(RichText::new(head).font(t::xbold(22.0)).color(p.text));
        ui.add_space(2.0);
        ui.label(RichText::new(sub).size(14.0).color(p.text_muted));
        ui.add_space(20.0);

        let ids = [Id::new("login-invite"), Id::new("login-name"), Id::new("login-pass")];
        let mut submit = false;
        let enter = |ui: &Ui, r: &egui::Response| r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if invite {
            label(ui, "Приглашение");
            let r = ui.add(login_field(&mut self.login.invite, w, ids[0]).hint_text("ссылка или код XXXX-XXXX-XXXX"));
            submit |= enter(ui, &r);
            let code = api::invite_from_input(&self.login.invite);
            if code.len() == 12 && self.login.preview.as_ref().is_none_or(|(c, _)| *c != code) {
                self.login.preview = Some((code.clone(), Err(String::new())));
                let api = self.api.with_token(None);
                let (c, key) = (code.clone(), code.clone());
                self.spawn(async move { api.invite_preview(&c).await.map_err(|e| e.message) }, move |r| Msg::Preview(key, r));
            }
            ui.add_space(4.0);
            match &self.login.preview {
                Some((c, Ok(pv))) if *c == code => {
                    ui.label(RichText::new(format!("Вас пригласили на «{}»", pv.server_name)).font(t::semibold(13.0)).color(p.accent_text));
                }
                Some((c, Err(e))) if *c == code && !e.is_empty() => {
                    ui.label(RichText::new(e).size(12.5).color(p.danger_text));
                }
                _ => {
                    ui.label(RichText::new("Регистр и дефисы не важны.").size(12.5).color(p.text_muted));
                }
            }
            ui.add_space(12.0);
        }
        label(ui, "Ник");
        let r = ui.add(login_field(&mut self.login.name, w, ids[1]).hint_text("например, okabe"));
        submit |= enter(ui, &r);
        ui.add_space(14.0);
        label(ui, "Пароль");
        let show = self.login.show_password;
        let r = ui.add(
            TextEdit::singleline(&mut self.login.password)
                .id(ids[2])
                .password(!show)
                .desired_width(w)
                .margin(Margin { left: 14, right: 42, top: 12, bottom: 12 })
                .font(t::font(15.0)),
        );
        submit |= enter(ui, &r);
        if self.login.bad_password {
            ui.painter().rect_stroke(r.rect, ui.visuals().widgets.inactive.corner_radius, Stroke::new(1.5, p.danger), StrokeKind::Inside);
        }
        // глаз — показать/скрыть пароль
        let eye = Rect::from_center_size(pos2(r.rect.right() - 22.0, r.rect.center().y), vec2(30.0, 30.0));
        let er = ui.interact(eye, Id::new("login-eye"), Sense::click());
        let ec = if er.hovered() { p.text } else { p.text_muted };
        ui.painter().text(eye.center(), Align2::CENTER_CENTER, if show { ph::EYE_SLASH } else { ph::EYE }, t::font(18.0), ec);
        if er.on_hover_text(if show { "Скрыть пароль" } else { "Показать пароль" }).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.login.show_password = !show;
        }
        if self.login.bad_password
            && let Some(err) = &self.login.error
        {
            ui.add_space(6.0);
            ui.label(RichText::new(err).size(13.0).color(p.danger_text));
        }
        ui.add_space(14.0);
        // «Не выходить» и «Забыли пароль?»
        ui.horizontal(|ui| {
            let mut on = self.settings.stay_signed_in;
            if login_checkbox(ui, &mut on, "Не выходить") {
                self.settings.stay_signed_in = on;
                self.settings.save();
            }
            if !invite {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let l = ui.add(egui::Label::new(RichText::new("Забыли пароль?").size(13.0).color(p.accent_text).underline()).sense(Sense::click()));
                    if l.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.login.forgot_hint = !self.login.forgot_hint;
                    }
                });
            }
        });
        if self.login.forgot_hint && !invite {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{}  Пароль сбрасывает администратор сервера — напишите ему.", ph::INFO)).size(12.5).color(p.text_muted));
        }
        ui.add_space(20.0);
        let text = match (self.login.busy, invite) {
            (true, false) => "Входим…",
            (true, true) => "Создаём аккаунт…",
            (false, false) => "Войти",
            (false, true) => "Принять приглашение",
        };
        if login_button(ui, text, w, !self.login.busy) {
            submit = true;
        }
        if !self.login.bad_password
            && let Some(err) = &self.login.error
        {
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}  {err}", ph::WARNING_CIRCLE)).size(13.0).color(p.danger_text));
            });
        }
        if self.login.focus_pending {
            self.login.focus_pending = false;
            let target = if invite && self.login.invite.trim().is_empty() {
                ids[0]
            } else if self.login.name.trim().is_empty() {
                ids[1]
            } else {
                ids[2]
            };
            ui.memory_mut(|m| m.request_focus(target));
        }
        if submit && !self.login.busy {
            self.submit_login();
        }
    }

    // ------------------------------------------------------------ главный экран

    fn ui_main(&mut self, ui: &mut Ui) {
        if self.channels_at.elapsed() > CHANNELS_REFRESH && !self.demo {
            self.fetch_channels();
        }
        let mut vs = self.voice.snapshot();
        // Своя волна — по уровню микрофона прямо из захвата (как в проверке микрофона), а не по «говорящим» от
        // сервера: те приходят с задержкой в доли секунды.
        crate::denoise::set_repaint(ui.ctx().clone());
        if let Some(me) = vs.peers.iter_mut().find(|p| p.is_local) {
            let (level, speaking) = crate::denoise::local_voice();
            me.level = level;
            me.speaking = speaking && !me.mic_muted;
        }
        // Стримим окно — своё окно «почти непрозрачное», чтобы браузер под ним не считал себя перекрытым и не замирал.
        crate::frame::keep_layered(vs.stream.as_ref().is_some_and(|s| s.source.is_window()));
        #[cfg(debug_assertions)]
        if std::env::var_os("AMADEUS_DEMO").is_some() {
            let watching = std::mem::take(&mut vs.watching);
            vs = demo_state(ui.input(|i| i.time));
            vs.watching = watching;
            // AMADEUS_DEMO_WATCH=1 — «смотрим» все трансляции демо; =2 — ещё и мини-плеер включён вручную (скриншоты окна в окне)
            if let Ok(v) = std::env::var("AMADEUS_DEMO_WATCH") {
                vs.watching = vs.peers.iter().filter(|p| p.streaming).map(|p| p.identity.clone()).collect();
                if v == "2" {
                    self.pip.force();
                }
            }
            self.demo_frames(ui, &vs);
        }
        #[cfg(debug_assertions)]
        self.debug_autostream(&vs);
        #[cfg(debug_assertions)]
        self.debug_autowatch(ui, &vs);
        #[cfg(debug_assertions)]
        self.debug_rc(&vs);
        self.chat.ensure_stream();
        self.leave_if_gone(&vs);
        self.sync_focus(ui, &vs);
        self.speech_repaint(ui, &vs);
        self.ui_remote(ui.ctx(), &vs);
        if self.fullscreen {
            self.ui_fullscreen(ui, &vs);
            self.report_views(&vs);
            return;
        }
        self.ui_sidebar(ui, &vs);
        self.ui_header(ui, &vs);
        self.ui_dock(ui, &vs);
        let text_channel = match self.view {
            View::Text(id) => self.channels.iter().find(|c| c.id == id).cloned(),
            View::Dm(id) => self.dms.get(id).map(crate::dms::chat_channel),
            View::Voice | View::Friends => None,
        };
        // список участников справа — на сервере (голос или текстовый канал), если включён
        let members_server = match self.view {
            View::Text(_) => self.model.active,
            View::Voice if vs.channel.as_ref().is_none_or(|c| !crate::dms::is_dm(c)) => self.model.active,
            _ => None,
        };
        if let Some(sid) = members_server.filter(|_| self.settings.members_open && !self.demo) {
            let speaking: Vec<i64> = vs.peers.iter().filter(|p| p.speaking).filter_map(|p| p.identity.parse().ok()).collect();
            egui::Panel::right("members")
                .exact_size(crate::members_ui::WIDTH)
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().fill(t::p().surface).inner_margin(Margin::symmetric(6, 0)))
                .show(ui, |ui| {
                    let r = ui.max_rect();
                    ui.painter().vline(r.left() - 6.0, r.y_range(), Stroke::new(1.0, t::p().border));
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        self.members.panel(ui, &self.model, &self.people, sid, &speaking);
                    });
                });
        }
        if self.view == View::Friends {
            egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg).inner_margin(Margin::symmetric(28, 20))).show(ui, |ui| {
                self.ui_friends_screen(ui, &vs);
            });
        } else {
        match text_channel {
            Some(ch) => {
                if let View::Dm(id) = self.view
                    && self.dm_call_here(&vs, id)
                {
                    // звонок другу — сцена над перепиской, во вкладке самого друга
                    let h = (ui.available_height() * 0.58).clamp(300.0, 680.0);
                    egui::Panel::top("dm-call")
                        .exact_size(h)
                        .resizable(false)
                        .show_separator_line(false)
                        .frame(Frame::new().fill(t::p().bg))
                        .show(ui, |ui| {
                            let r = ui.max_rect();
                            ui.painter().hline(r.x_range(), r.bottom(), Stroke::new(1.0, t::p().border));
                            self.dock_panel(ui, &vs);
                            egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg).inner_margin(Margin::same(16))).show(ui, |ui| {
                                self.ui_stage(ui, &vs);
                            });
                        });
                }
                let modal = self.settings_open || self.picker.is_some() || self.popup.is_some() || self.update_open || self.servers.modal_open() || self.channel_list.modal_open() || self.server_settings.is_open();
                egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg)).show(ui, |ui| {
                    self.chat.ui(ui, &ch, modal, &self.people);
                });
            }
            None => {
                egui::CentralPanel::no_frame().frame(Frame::new().fill(t::p().bg).inner_margin(Margin::same(28))).show(ui, |ui| {
                    self.ui_stage(ui, &vs);
                });
            }
        }
        }
        self.ui_banner(ui, &vs);
        self.ui_volume_popup(ui, &vs);
        self.servers.dialogs(ui.ctx());
        self.ui_profile_card(ui.ctx(), &vs);
        self.ui_summon(ui, &vs);
        self.ui_status_edit(ui.ctx());
        self.ui_calls(ui, &vs);
        self.channel_list.dialogs(ui.ctx());
        self.server_settings.ui(ui.ctx(), &self.model, &self.people);
        self.ui_avatar_crop(ui.ctx());
        if self.settings_open {
            self.ui_settings(ui, &vs);
        }
        if self.picker.is_some() {
            self.ui_picker(ui, &vs);
        }
        self.ui_pip(ui, &vs);
        self.report_views(&vs);
    }

    /// Перерисовка ради волны речи: до 30 к/с и только пока кто-то говорит (или волна ещё затухает), окно на виду
    /// и анимации не выключены. Никто не говорит — лишних кадров нет. Плюс: кто-то зашёл в канал — уши кота в заголовке.
    fn speech_repaint(&mut self, ui: &Ui, vs: &VoiceState) {
        let ch = vs.channel.as_ref().filter(|_| vs.conn == Conn::Connected).map(|c| c.id);
        let ids: Vec<String> = if ch.is_some() { vs.peers.iter().map(|p| p.identity.clone()).collect() } else { Vec::new() };
        if ch.is_some() && ch == self.prev_peers.0 && ids.iter().any(|id| !self.prev_peers.1.contains(id)) {
            crate::frame::perk_ears();
        }
        self.prev_peers = (ch, ids);
        if ch.is_none() || !crate::anim::important(ui.ctx()) {
            return;
        }
        let ctx = ui.ctx();
        let live = vs.peers.iter().any(|p| {
            let fading = ctx.data(|d| d.get_temp::<(f32, f64)>(Id::new(("speech-amp", p.identity.as_str())))).is_some_and(|(v, _)| v > 0.0);
            (p.speaking && !p.mic_muted) || fading
        });
        if live {
            ctx.request_repaint_after(crate::anim::SPEECH_FRAME);
        }
    }

    /// Отладка управления (debug): AMADEUS_RC_ASK=pointer|control — зритель сам просит доступ у первого стримера, которого смотрит;
    /// AMADEUS_RC_GRANT=pointer|control|deny — стример сам отвечает; AMADEUS_RC_DRIVE=1 — зритель берёт управление и водит
    /// указку по кругу, раз в 3 с кликает и печатает «a». С AMADEUS_RC_DRYRUN=1 у стримера ввод идёт в журнал, курсор не двигается.
    #[cfg(debug_assertions)]
    fn debug_rc(&mut self, vs: &VoiceState) {
        use crate::remote::proto::{Btn, InputEv, Level};
        use std::sync::Mutex;
        static STATE: Mutex<Option<(String, Instant, u32)>> = Mutex::new(None);
        let level = |v: &str| match v {
            "control" => Some(Level::Control),
            "pointer" => Some(Level::Pointer),
            _ => None,
        };
        if let Ok(g) = std::env::var("AMADEUS_RC_GRANT")
            && let Some((who, _)) = vs.rc_host.pending.first()
        {
            self.voice.send(Cmd::RcRespond { identity: who.clone(), grant: level(&g) });
        }
        // AMADEUS_RC_REVOKE_AFTER=<с> — стример сам жмёт «Забрать» через столько секунд после начала управления (один раз).
        {
            static SINCE: Mutex<Option<Instant>> = Mutex::new(None);
            static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Some(secs) = std::env::var("AMADEUS_RC_REVOKE_AFTER").ok().and_then(|v| v.parse::<u64>().ok())
                && vs.rc_host.controller.is_some()
                && !DONE.load(Ordering::Relaxed)
            {
                let mut s = SINCE.lock().unwrap();
                let at = *s.get_or_insert_with(Instant::now);
                if at.elapsed() >= Duration::from_secs(secs) {
                    DONE.store(true, Ordering::Relaxed);
                    self.voice.send(Cmd::RcRevokeAll);
                }
            }
        }
        let mut st = STATE.lock().unwrap();
        if let Ok(a) = std::env::var("AMADEUS_RC_ASK")
            && st.is_none()
            && let Some(w) = vs.watching.first()
            && self.video.frame(w).is_some()
            && let Some(l) = level(&a)
        {
            self.voice.send(Cmd::RcRequest { identity: w.clone(), level: l });
            *st = Some((w.clone(), Instant::now(), 0));
        }
        if std::env::var_os("AMADEUS_RC_DRIVE").is_none() {
            return;
        }
        let Some((w, at, n)) = st.as_mut() else { return };
        let Some(g) = vs.rc_guest.get(w.as_str()) else { return };
        if !matches!(g.phase, crate::remote::guest::GuestPhase::Granted { .. }) {
            return;
        }
        if !matches!(g.phase, crate::remote::guest::GuestPhase::Granted { controlling: true, .. }) && !g.upgrade_pending {
            self.voice.send(Cmd::RcTake { identity: w.clone(), on: true });
        }
        let t = at.elapsed().as_secs_f32();
        let (x, y) = (0.5 + 0.3 * (t * 0.8).cos(), 0.5 + 0.3 * (t * 0.8).sin());
        self.voice.send(Cmd::RcPointer { identity: w.clone(), x, y });
        let tick = (t / 3.0) as u32;
        if tick > *n {
            *n = tick;
            for down in [true, false] {
                self.voice.send(Cmd::RcInput { identity: w.clone(), ev: InputEv::Button { b: Btn::L, down, x, y } });
            }
            for down in [true, false] {
                self.voice.send(Cmd::RcInput { identity: w.clone(), ev: InputEv::Key { sc: 0x1E, ext: false, down } });
            }
        }
        self.ctx.request_repaint_after(Duration::from_millis(30));
    }

    /// Отладка: AMADEUS_AUTOWATCH=1 — сразу смотреть каждую трансляцию в канале
    /// (+ AMADEUS_AUTOFULLSCREEN=1 — развернуть на весь экран, когда придёт первый кадр).
    #[cfg(debug_assertions)]
    fn debug_autowatch(&mut self, ui: &Ui, vs: &VoiceState) {
        use std::sync::Mutex;
        static ASKED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("AMADEUS_AUTOWATCH").is_none() || vs.conn != Conn::Connected {
            return;
        }
        let mut asked = ASKED.lock().unwrap();
        for p in vs.peers.iter().filter(|p| p.streaming && !p.is_local) {
            if !asked.contains(&p.identity) {
                asked.push(p.identity.clone());
                self.watch(&p.identity);
                // AMADEUS_QUALITY=low|high — сразу этот слой; =cycle — через 12 с «Экономное», 27 с «Исходное», 42 с «Авто».
                let fixed = match std::env::var("AMADEUS_QUALITY").as_deref() {
                    Ok("low") => Some(Quality::Low),
                    Ok("high") => Some(Quality::High),
                    _ => None,
                };
                if let Some(quality) = fixed {
                    self.voice.send(Cmd::SetStreamQuality { identity: p.identity.clone(), quality });
                }
                if std::env::var("AMADEUS_QUALITY").as_deref() == Ok("cycle") {
                    let (tx, identity) = (self.voice.sender(), p.identity.clone());
                    std::thread::spawn(move || {
                        for (at, quality) in [(12, Quality::Low), (15, Quality::High), (15, Quality::Auto)] {
                            std::thread::sleep(Duration::from_secs(at));
                            log::info!("AMADEUS_QUALITY=cycle: {}", quality.label());
                            let _ = tx.send(Cmd::SetStreamQuality { identity: identity.clone(), quality });
                        }
                    });
                }
            }
        }
        // AMADEUS_AUTOMAX=1 — сначала развернуть окно, полный экран — через 2 с (как у пользователя: из развёрнутого).
        static MAXED: Mutex<Option<Instant>> = Mutex::new(None);
        if std::env::var_os("AMADEUS_AUTOMAX").is_some() {
            let mut m = MAXED.lock().unwrap();
            if m.is_none() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                *m = Some(Instant::now());
            }
            if m.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
                return;
            }
        }
        if std::env::var_os("AMADEUS_AUTOFULLSCREEN").is_some()
            && let Some(f) = self.focus.clone()
            && self.video.frame(&f).is_some()
            && !FULL.swap(true, Ordering::Relaxed)
        {
            log::info!("AMADEUS_AUTOFULLSCREEN: полный экран");
            self.set_fullscreen(ui.ctx(), true);
        }
    }

    /// Демо-кадры для просматриваемых «трансляций» (AMADEUS_DEMO), ~10 к/с.
    #[cfg(debug_assertions)]
    fn demo_frames(&mut self, ui: &Ui, vs: &VoiceState) {
        use std::sync::Mutex;
        static AT: Mutex<Option<Instant>> = Mutex::new(None);
        let mut at = AT.lock().unwrap();
        if at.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
            return;
        }
        *at = Some(Instant::now());
        let time = ui.input(|i| i.time) as f32;
        for id in &vs.watching {
            self.video.put_demo(id, time);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    /// Отладка: AMADEUS_AUTOSTREAM=screen | <часть заголовка окна> [+ AMADEUS_STREAM_PRESET=720|1080|1080p60|native] —
    /// начать трансляцию сразу после входа в канал (один раз).
    #[cfg(debug_assertions)]
    fn debug_autostream(&mut self, vs: &VoiceState) {
        use std::sync::atomic::AtomicBool;
        static DONE: AtomicBool = AtomicBool::new(false);
        static PICKER: AtomicBool = AtomicBool::new(false);
        // AMADEUS_OPEN_PICKER=1 — открыть пикер сразу после входа (скриншоты).
        if vs.conn == Conn::Connected && std::env::var_os("AMADEUS_OPEN_PICKER").is_some() && !PICKER.swap(true, Ordering::Relaxed) {
            self.open_picker(vs);
        }
        let Ok(want) = std::env::var("AMADEUS_AUTOSTREAM") else { return };
        if vs.conn != Conn::Connected || DONE.swap(true, Ordering::Relaxed) {
            return;
        }
        let preset = match std::env::var("AMADEUS_STREAM_PRESET").as_deref() {
            Ok("720") => crate::stream::Preset::P720_30,
            Ok("1080p60") => crate::stream::Preset::P1080_60,
            Ok("1440") => crate::stream::Preset::P1440_60,
            Ok("native") => crate::stream::Preset::Native,
            _ => crate::stream::Preset::P1080_30,
        };
        // AMADEUS_STREAM_AUDIO=0 — без звука.
        let audio = std::env::var("AMADEUS_STREAM_AUDIO").map(|v| v != "0").unwrap_or(self.settings.stream_audio);
        let tx = self.voice.sender();
        std::thread::spawn(move || {
            let list = capture_win::list_sources(64, 36);
            let found = if want == "screen" {
                list.into_iter().find(|s| !s.id.is_window())
            } else {
                list.into_iter().find(|s| s.title.to_lowercase().contains(&want.to_lowercase()))
            };
            match found {
                Some(s) => {
                    let _ = tx.send(Cmd::StartStream { source: s.id, title: s.title, width: s.width, height: s.height, preset, audio });
                }
                None => log::warn!("AMADEUS_AUTOSTREAM: источник «{want}» не найден"),
            }
            // AMADEUS_AUTOSTREAM_THEN=<часть заголовка> — через 15 с сменить источник на лету.
            if let Ok(next) = std::env::var("AMADEUS_AUTOSTREAM_THEN") {
                std::thread::sleep(Duration::from_secs(15));
                let list = capture_win::list_sources(64, 36);
                if let Some(s) = list.into_iter().find(|s| s.title.to_lowercase().contains(&next.to_lowercase())) {
                    let _ = tx.send(Cmd::StartStream { source: s.id, title: s.title, width: s.width, height: s.height, preset, audio });
                }
            }
        });
    }

    fn open_picker(&mut self, vs: &VoiceState) {
        let current = vs.stream.as_ref().map(|s| s.source);
        let preset = vs.stream.as_ref().map(|s| s.preset).unwrap_or(self.settings.stream_preset);
        let audio = vs.stream.as_ref().map(|s| s.audio != voice::AudioStatus::Off).unwrap_or(self.settings.stream_audio);
        self.picker = Some(Picker::open(&self.ctx, preset, audio, current));
    }

    fn ui_picker(&mut self, ui: &mut Ui, vs: &VoiceState) {
        if vs.channel.is_none() {
            self.picker = None;
            return;
        }
        let Some(p) = &mut self.picker else { return };
        match p.ui(ui.ctx()) {
            picker::Action::None => {}
            picker::Action::Cancel => self.picker = None,
            picker::Action::Start { source, title, width, height, preset, audio } => {
                self.settings.stream_preset = preset;
                self.settings.stream_audio = audio;
                self.settings.save();
                self.voice.send(Cmd::StartStream { source, title, width, height, preset, audio });
                self.picker = None;
            }
        }
    }

    fn ui_sidebar(&mut self, ui: &mut Ui, vs: &VoiceState) {
        egui::Panel::left("servers")
            .exact_size(servers_ui::COLUMN_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t::p().rail))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(12.0);
                        let badge = self.friends.incoming() + self.unread.dm_total() as usize;
                        if crate::friends::rail_button(ui, badge, self.view == View::Friends) {
                            self.view = View::Friends;
                        }
                        let open_dm = match self.view {
                            View::Dm(id) => Some(id),
                            _ => None,
                        };
                        if let Some(id) = self.dms.rail(ui, &self.people, &self.unread, open_dm) {
                            self.view = View::Dm(id);
                        }
                        ui.add_space(6.0);
                        let (r, _) = ui.allocate_exact_size(vec2(32.0, 2.0), Sense::hover());
                        ui.painter().hline(r.x_range(), r.center().y, Stroke::new(2.0, t::p().border));
                    });
                    if let Some(id) = self.servers.column(ui, &self.model, &self.people, &self.unread, matches!(self.view, View::Dm(_) | View::Friends)) {
                        self.select_server(id);
                    }
                });
            });
        egui::Panel::left("side")
            .exact_size(260.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t::p().surface))
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(full.right(), full.y_range(), Stroke::new(1.0, t::p().border));

                if matches!(self.view, View::Friends | View::Dm(_)) {
                    egui::Panel::bottom("side-me")
                        .show_separator_line(false)
                        .frame(Frame::new().inner_margin(Margin::same(8)))
                        .show(ui, |ui| {
                            self.ui_call_strip(ui, vs);
                            Frame::new()
                                .fill(t::p().surface_2)
                                .stroke(Stroke::new(1.0, t::p().border))
                                .corner_radius(CornerRadius::same(kit::R_CARD))
                                .inner_margin(Margin::symmetric(10, 8))
                                .show(ui, |ui| self.ui_me(ui, vs));
                        });
                    egui::CentralPanel::no_frame().frame(Frame::new().inner_margin(Margin::symmetric(10, 12))).show(ui, |ui| self.ui_friends_side(ui));
                    return;
                }
                egui::Panel::top("side-top")
                    .show_separator_line(false)
                    .frame(Frame::new().inner_margin(Margin { left: 0, right: 1, top: 0, bottom: 8 }))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match self.servers.header(ui, &self.model, &self.people) {
                            Some(servers_ui::Action::OpenSettings(id)) => self.server_settings.open(&self.model, id),
                            None => {}
                        }
                        let r = ui.min_rect();
                        ui.painter().hline(full.x_range(), r.bottom() + 8.0, Stroke::new(1.0, t::p().border));
                    });
                egui::Panel::bottom("side-me")
                    .show_separator_line(false)
                    .frame(Frame::new().inner_margin(Margin::same(8)))
                    .show(ui, |ui| {
                        self.ui_call_strip(ui, vs);
                        Frame::new()
                            .fill(t::p().surface_2)
                            .stroke(Stroke::new(1.0, t::p().border))
                            .corner_radius(CornerRadius::same(kit::R_CARD))
                            .inner_margin(Margin::symmetric(10, 8))
                            .show(ui, |ui| self.ui_me(ui, vs));
                    });
                egui::CentralPanel::no_frame().frame(Frame::new().inner_margin(Margin::symmetric(8, 0))).show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.ui_channel_list(ui, vs));
                });
            });
    }

    fn ui_channel_list(&mut self, ui: &mut Ui, vs: &VoiceState) {
        if self.demo || self.model.active.is_none() {
            self.ui_channel_list_plain(ui, vs);
            return;
        }
        let my_voice = match (&vs.channel, &vs.conn) {
            (Some(c), Conn::Connected) => Some((c.id, vs.peers.as_slice())),
            _ => None,
        };
        let before = self.settings.collapsed_categories.clone();
        let open_text = match self.view {
            View::Text(id) => Some(id),
            View::Voice | View::Dm(_) | View::Friends => None,
        };
        let cx = channels_ui::ListCtx {
            model: &self.model,
            people: &self.people,
            my_voice,
            open_text,
            unread: &self.unread,
            collapsed: &mut self.settings.collapsed_categories,
        };
        let (pick, moved) = self.channel_list.list(ui, cx);
        if self.settings.collapsed_categories != before {
            self.settings.save();
        }
        if let Some(m) = moved {
            self.model.set_layout(m.server, m.layout);
            self.sync_channels();
        }
        match pick {
            Some(channels_ui::Pick::Voice(ch)) => {
                let here = vs.channel.as_ref().is_some_and(|c| c.id == ch.id) && vs.conn == Conn::Connected;
                // уже в этом канале — просто вернуться к сцене (из чата)
                if !here {
                    self.voice.send(Cmd::Join(ch));
                }
                self.view = View::Voice;
            }
            Some(channels_ui::Pick::Text(id)) => self.view = View::Text(id),
            Some(channels_ui::Pick::Profile(uid, at)) => self.members.open_card(uid, at, None),
            Some(channels_ui::Pick::PeerVolume(identity, at)) => {
                if let Some(p) = vs.peers.iter().find(|p| p.identity == identity).cloned() {
                    self.open_popup(&p, at);
                }
            }
            None => {}
        }
    }

    /// Без серверов (или демо): плоский список, как до фазы 9.1.
    fn ui_channel_list_plain(&mut self, ui: &mut Ui, vs: &VoiceState) {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add_space(6.0);
        group_label(ui, "Голосовые каналы");
        let voice: Vec<Channel> = self.channels.iter().filter(|c| c.kind == ChannelKind::Voice).cloned().collect();
        if voice.is_empty() {
            let text = match (&self.servers.load_error, self.model.active) {
                (Some(e), _) => e.clone(),
                (None, None) if !self.model.servers.is_empty() || self.channels_at.elapsed() > Duration::from_secs(3) => {
                    "Нет серверов — нажмите «+» слева".into()
                }
                (None, Some(_)) => "Нет голосовых каналов".into(),
                _ => "Загрузка…".into(),
            };
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new(text).size(13.0).color(t::p().text_muted));
            });
        }
        for ch in voice {
            let active = vs.channel.as_ref().is_some_and(|c| c.id == ch.id);
            let count = if active && vs.conn == Conn::Connected { Some(vs.peers.len().to_string()) } else { None };
            if kit::row(ui, ph::SPEAKER_HIGH, &ch.name, active, count.as_deref(), true).clicked() {
                // уже в этом канале — просто вернуться к сцене (из чата)
                if !(active && vs.conn == Conn::Connected) {
                    self.voice.send(Cmd::Join(ch.clone()));
                }
                self.view = View::Voice;
            }
            if active {
                for p in &vs.peers {
                    let r = member_row(ui, p, &self.people);
                    self.peer_row_click(&r, p);
                }
            }
        }
        let text: Vec<Channel> = self.channels.iter().filter(|c| c.kind == ChannelKind::Text).cloned().collect();
        if !text.is_empty() {
            ui.add_space(6.0);
            group_label(ui, "Текстовые каналы");
            for ch in text {
                let open = self.view == View::Text(ch.id);
                if kit::row(ui, ph::HASH, &ch.name, open, None, true).clicked() {
                    self.view = View::Text(ch.id);
                }
            }
        }
    }

    /// Мини-плеер трансляции: плавающая панель внутри Murmur, а когда окно Murmur неактивно (свёрнуто, в трее, вы в другой
    /// программе) дольше `PIP_AWAY` — отдельное окно ОС. Нужен, пока выбранный стрим не виден на сцене (или включён вручную).
    fn ui_pip(&mut self, ui: &mut Ui, vs: &VoiceState) {
        use crate::pip::PeerInfo;
        let ch = vs.channel.as_ref().filter(|_| vs.conn == Conn::Connected).cloned();
        let ch_id = ch.as_ref().map(|c| c.id);
        if ch_id != self.pip_chan {
            self.pip_chan = ch_id;
            self.pip.reset();
        }
        let Some(ch) = ch else {
            self.pip_away = None;
            return;
        };
        let ctx = ui.ctx().clone();
        // Murmur неактивен: свёрнут, скрыт в трей или не в фокусе
        let inactive = ctx.input(|i| i.viewport().minimized == Some(true) || i.viewport().focused == Some(false)) || !desktop::is_visible(self.window);
        if inactive {
            let since = *self.pip_away.get_or_insert_with(Instant::now);
            if since.elapsed() < PIP_AWAY {
                ctx.request_repaint_after(PIP_AWAY - since.elapsed() + Duration::from_millis(20));
            }
        } else {
            self.pip_away = None;
        }
        let as_window = self.pip_away.is_some_and(|t| t.elapsed() >= PIP_AWAY);
        let mut watching = vs.watching.clone();
        watching.sort();
        let infos: Vec<PeerInfo> = vs.peers.iter().map(|p| PeerInfo { identity: &p.identity, speaking: p.speaking && !p.mic_muted, streaming: p.streaming }).collect();
        let Some(id) = self.pip.pick(ctx.input(|i| i.time), &watching, &infos) else { return };
        // в окне ОС сцену никто не видит — «нарисовано на сцене» не считается
        let drawn: std::collections::HashSet<String> = if as_window { Default::default() } else { self.views_frame.keys().cloned().collect() };
        if !self.pip.wanted(&id, &drawn) || (!self.settings.pip_auto && !self.pip.forced()) || (self.settings_open && !as_window) {
            return;
        }
        let frame = self.video.frame(&id);
        let aspect = frame.as_ref().map(|f| f.size[1] as f32 / f.size[0].max(1) as f32).filter(|a| *a > 0.1 && *a < 3.0).unwrap_or(9.0 / 16.0);
        let name = vs.peers.iter().find(|p| p.identity == id).map(|p| peer_name(&self.people, p)).unwrap_or_default();
        let pinned = self.pip.pinned() == Some(id.as_str());
        let svol = self.settings.stream_volume_of(&id);
        let ppp = ctx.pixels_per_point();
        let mut act = None::<PipAct>;

        if as_window {
            let w = self.pip_win_w.clamp(240.0, 960.0);
            let size = vec2(w, (w * aspect).round());
            let mut builder = egui::ViewportBuilder::default()
                .with_title("Murmur — трансляция")
                .with_inner_size(size)
                .with_min_inner_size([240.0, 135.0])
                .with_decorations(false)
                .with_active(false)
                .with_taskbar(false);
            if self.settings.pip_topmost {
                builder = builder.with_always_on_top();
            }
            // где открыть: прошлое место, иначе правый нижний угол экрана (над панелью задач)
            let monitor = ctx.input(|i| i.viewport().monitor_size);
            if let Some((x, y, _)) = self.settings.pip_window {
                builder = builder.with_position([x, y]);
            } else if let Some(m) = monitor {
                builder = builder.with_position([m.x - size.x - 24.0, m.y - size.y - 72.0]);
            }
            let mut drawn_px = None;
            let mut win_geom = None::<(f32, f32, f32)>;
            let mut new_w = w;
            let vid = egui::ViewportId::from_hash_of("pip-window");
            ctx.show_viewport_immediate(vid, builder, |ui, _class| {
                let rect = ui.max_rect();
                drawn_px = Some(rect.size());
                let (a, resp) = pip_player(ui, rect, frame.as_ref(), &name, svol, pinned);
                act = a;
                if resp.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                let grip = Rect::from_min_size(rect.right_bottom() - vec2(22.0, 22.0), vec2(22.0, 22.0));
                let gr = ui.interact(grip, Id::new("pip-win-grip"), Sense::drag());
                if gr.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(egui::ResizeDirection::SouthEast));
                }
                gr.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
                // пропорции кадра: ширина решает, высота подгоняется
                let cur = ui.ctx().input(|i| i.viewport().inner_rect.map(|r| r.size()));
                if let Some(cur) = cur {
                    new_w = cur.x;
                    let want_h = (cur.x * aspect).round();
                    if (cur.y - want_h).abs() > 1.5 {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(cur.x, want_h)));
                    }
                }
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    act = Some(PipAct::Close);
                }
                // запомнить, где окно оставили (когда отпустили мышь)
                let (outer, down) = ui.ctx().input(|i| (i.viewport().outer_rect, i.pointer.any_down()));
                if let Some(o) = outer
                    && !down
                {
                    win_geom = Some((o.min.x, o.min.y, new_w));
                }
            });
            if let Some(g) = win_geom
                && self.settings.pip_window.is_none_or(|p| (p.0 - g.0).abs() > 1.0 || (p.1 - g.1).abs() > 1.0 || (p.2 - g.2).abs() > 1.0)
            {
                self.settings.pip_window = Some(g);
                self.settings.save();
            }
            self.pip_win_w = new_w;
            if let Some(sz) = drawn_px {
                // размер в физических пикселях — SFU по нему выбирает качество
                self.views_frame.insert(id.clone(), ((sz.x * ppp) as u32, (sz.y * ppp) as u32));
            }
        } else {
            let area_rect = ctx.content_rect();
            let w = self.pip_w.clamp(200.0, 720.0).min(area_rect.width() - 24.0);
            let size = vec2(w, w * aspect);
            let default_pos = pos2(area_rect.right() - size.x - 16.0, area_rect.top() + crate::frame::HEIGHT + 76.0);
            let pos = self.pip_pos.unwrap_or(default_pos);
            let pos = pos2(
                pos.x.clamp(area_rect.left() + 4.0, (area_rect.right() - size.x - 4.0).max(area_rect.left() + 4.0)),
                pos.y.clamp(area_rect.top() + 4.0, (area_rect.bottom() - size.y - 4.0).max(area_rect.top() + 4.0)),
            );
            let mut new_pos = pos;
            let mut new_w = w;
            let mut drawn_rect = None;
            let mut moved = false;
            egui::Area::new(Id::new("pip-panel")).order(egui::Order::Foreground).fixed_pos(pos).show(&ctx, |ui| {
                let rect = Rect::from_min_size(ui.cursor().min, size);
                ui.allocate_rect(rect, Sense::hover());
                drawn_rect = Some(rect);
                let (a, resp) = pip_player(ui, rect, frame.as_ref(), &name, svol, pinned);
                act = a;
                if resp.dragged() {
                    new_pos = pos + resp.drag_delta();
                }
                let grip = Rect::from_min_size(rect.right_bottom() - vec2(22.0, 22.0), vec2(22.0, 22.0));
                let gr = ui.interact(grip, Id::new("pip-grip"), Sense::drag());
                if gr.dragged() {
                    new_w = (w + gr.drag_delta().x).clamp(200.0, 720.0);
                }
                moved = resp.drag_stopped() || gr.drag_stopped();
                gr.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
            });
            self.pip_pos = Some(new_pos);
            self.pip_w = new_w;
            if moved {
                self.settings.pip_panel = Some((new_pos.x, new_pos.y, new_w));
                self.settings.save();
            }
            if let Some(r) = drawn_rect {
                self.mark_view(&id, r, ppp);
            }
        }

        match act {
            Some(PipAct::Close) => self.pip.dismiss(),
            Some(PipAct::Pin) => self.pip.pin(if pinned { None } else { Some(id.clone()) }),
            Some(PipAct::Mute) => {
                let nv = if svol <= 0.001 { 1.0 } else { 0.0 };
                self.settings.stream_volume.insert(id.clone(), nv);
                self.settings.save();
                self.voice.send(Cmd::SetStreamVolume(id.clone(), nv));
            }
            Some(PipAct::Expand) => {
                if as_window {
                    // из окна ОС — поднять главное окно
                    desktop::show(self.window);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.pip_away = None;
                }
                self.focus = Some(id.clone());
                if crate::dms::is_dm(&ch) {
                    self.view = View::Dm(ch.id);
                } else {
                    self.select_server(ch.server_id);
                    self.view = View::Voice;
                }
                self.pip.force_off();
            }
            None => {}
        }
    }

    /// Плашка «вы в звонке» над панелью пользователя: где вы, перейти туда по клику и крупная кнопка «Отключиться».
    /// Нет на самом экране звонка (там есть своя капсула с кнопками).
    fn ui_call_strip(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let Some(ch) = vs.channel.clone().filter(|_| vs.conn != Conn::Idle) else { return };
        if self.on_call_screen(vs) {
            return;
        }
        let p = t::p();
        let is_dm = crate::dms::is_dm(&ch);
        let head = match &vs.conn {
            Conn::Connected => "Голос подключён",
            Conn::Connecting => "Подключение…",
            _ => "Нет связи…",
        };
        let sub = if is_dm {
            format!("Звонок: {}", ch.name)
        } else {
            match self.model.servers.iter().find(|s| s.id == ch.server_id) {
                Some(s) => format!("{} / {}", ch.name, s.name),
                None => ch.name.clone(),
            }
        };
        let mut leave = false;
        let mut go = false;
        Frame::new()
            .fill(p.surface_2)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(kit::R_CARD))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = ui.available_width();
                    let (text_rect, resp) = ui.allocate_exact_size(vec2(w - 36.0 - 8.0, 36.0), Sense::click());
                    let hot = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), kit::HOVER_S);
                    let color = if vs.conn == Conn::Connected { p.status_online } else { p.text_muted };
                    ui.painter().text(text_rect.left_top() + vec2(0.0, 2.0), Align2::LEFT_TOP, head, t::semibold(13.0), color);
                    let g = ui.painter().layout_no_wrap(sub.clone(), t::font(12.5), p.text_muted.lerp_to_gamma(p.text, hot));
                    let clipped = ui.painter().with_clip_rect(text_rect);
                    clipped.galley(text_rect.left_top() + vec2(0.0, 19.0), g, p.text_muted);
                    if resp.on_hover_text("Вернуться к звонку").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        go = true;
                    }
                    ui.add_space(8.0);
                    let (rect, r) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
                    let hover = ui.ctx().animate_bool_with_time(r.id, r.hovered(), kit::HOVER_S);
                    ui.painter().circle_filled(rect.center(), 18.0, p.danger.lerp_to_gamma(p.danger_hover, hover));
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, egui_phosphor::fill::PHONE_X, t::icon_fill(18.0), Color32::WHITE);
                    if r.on_hover_text("Отключиться").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        leave = true;
                    }
                });
            });
        ui.add_space(6.0);
        if leave {
            self.voice.send(Cmd::Leave);
        } else if go {
            if is_dm {
                self.view = View::Dm(ch.id);
            } else {
                self.select_server(ch.server_id);
                self.view = View::Voice;
            }
        }
    }

    fn ui_me(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let me = self.me.as_ref().map(people::public);
        let name = me.as_ref().map(|u| u.shown_name().to_string()).unwrap_or_default();
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
            let glow = if vs.transmitting && vs.peers.first().is_some_and(|p| p.speaking) { 0.6 } else { 0.0 };
            match &me {
                Some(u) => self.people.paint(ui.painter(), rect.center(), 17.0, u, glow, false),
                None => kit::avatar(ui.painter(), rect.center(), 17.0, &kit::AvatarInput { name: &name, texture: None }, glow, kit::PeerBadge::None, false),
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let status = self.me.as_ref().and_then(|m| m.status.clone());
                let r = ui.add(egui::Label::new(RichText::new(&name).font(t::semibold(14.0))).sense(Sense::click()).selectable(false));
                if r.on_hover_text("Задать статус").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.status_edit = Some(status.clone().unwrap_or_default());
                }
                let sub = match (&vs.conn, &vs.channel) {
                    (Conn::Connected, Some(c)) if crate::dms::is_dm(c) => format!("звонок: {}", c.name),
                    (Conn::Connected, Some(c)) => format!("в {}", c.name),
                    (Conn::Connecting, Some(_)) => "подключение…".into(),
                    (Conn::Reconnecting | Conn::Rejoining { .. }, _) => "нет связи…".into(),
                    _ => status.unwrap_or_else(|| "не в канале".into()),
                };
                // не ui.horizontal: он берёт высоту interact_size, и подпись уезжала вниз от имени
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 0.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(sub).size(12.0).color(t::p().text_muted));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if kit::icon_button(ui, ph::GEAR_SIX, kit::IconState::Normal, "Настройки").clicked() {
                    self.open_settings();
                }
                let (icon, tip) = if vs.deafened { (ph::SPEAKER_SLASH, "Включить звук") } else { (ph::HEADPHONES, "Выключить звук") };
                if kit::icon_button(ui, icon, if vs.deafened { kit::IconState::Off } else { kit::IconState::Normal }, tip).clicked() {
                    self.voice.send(Cmd::SetDeafened(!vs.deafened));
                }
                let mic_off = vs.self_muted || vs.deafened;
                let (icon, tip) = if mic_off { (ph::MICROPHONE_SLASH, "Включить микрофон") } else { (ph::MICROPHONE, "Выключить микрофон") };
                if kit::icon_button(ui, icon, if mic_off { kit::IconState::Off } else { kit::IconState::Normal }, tip).clicked() {
                    self.voice.send(Cmd::SetSelfMuted(!mic_off));
                }
            });
        });
    }

    /// Левая колонка экрана «Друзья»: поиск, пункт «Друзья», личные сообщения (или друзья в сети, пока ЛС нет).
    fn ui_friends_side(&mut self, ui: &mut Ui) {
        let p = t::p();
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add(kit::text_field(&mut self.friend_filter, ui.available_width()).hint_text("Найти друга или беседу"));
        ui.add_space(10.0);
        let badge = self.friends.incoming();
        let trailing = (badge > 0).then(|| badge.to_string());
        if kit::row(ui, ph::USERS, "Друзья", self.view == View::Friends, trailing.as_deref(), true).clicked() {
            self.view = View::Friends;
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("ЛИЧНЫЕ СООБЩЕНИЯ").font(t::bold(12.0)).color(p.text_muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let r = kit::icon_button(ui, ph::PLUS, kit::IconState::Normal, "Написать другу");
                egui::Popup::menu(&r).width(240.0).show(|ui| {
                    if self.friends.list().is_empty() {
                        ui.label(RichText::new("Сначала добавьте друзей").size(13.0).color(p.text_muted));
                    }
                    for f in self.friends.list().to_vec() {
                        let u = self.people.resolve(f.user.clone());
                        if kit::menu_item(ui, egui_phosphor::fill::CHAT_CIRCLE, u.shown_name(), false).clicked() {
                            self.dms.open_with(u.id);
                        }
                    }
                });
            });
        });
        let filter = self.friend_filter.trim().to_lowercase();
        let hit = |u: &proto::PublicUser| filter.is_empty() || u.shown_name().to_lowercase().contains(&filter) || u.name.to_lowercase().contains(&filter);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let dms: Vec<proto::DmChannel> = self.dms.list().iter().filter(|d| hit(&d.user)).cloned().collect();
            let rows: Vec<(proto::PublicUser, Option<i64>)> = if self.dms.list().is_empty() {
                // ЛС пока нет — друзья в сети
                self.friends.list().iter().filter(|f| f.online && hit(&f.user)).map(|f| (f.user.clone(), None)).collect()
            } else {
                dms.iter().map(|d| (d.user.clone(), Some(d.id))).collect()
            };
            if rows.is_empty() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(if filter.is_empty() { "Пока пусто" } else { "Никого не нашли" }).size(13.0).color(p.text_muted));
                });
            }
            for (u, dm) in rows {
                let u = self.people.resolve(u);
                let online = self.friends.online(u.id).unwrap_or(false);
                let activity = self.friends.list().iter().find(|f| f.user.id == u.id).and_then(|f| f.activity.clone());
                let voice = if online { self.model.voice_of(u.id).map(|(_, c)| c.clone()) } else { None };
                let unread = dm.map(|id| self.unread.count(id)).unwrap_or(0);
                let active = dm.is_some_and(|id| self.view == View::Dm(id));
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 44.0), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), kit::HOVER_S);
                let bg = if active { p.row_active } else { Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, hover) };
                ui.painter().rect_filled(rect, CornerRadius::same(kit::R_ROW), bg);
                let av = pos2(rect.left() + 22.0, rect.center().y);
                self.people.paint(ui.painter(), av, 15.0, &u, 0.0, !online);
                crate::friends::status_dot(ui.painter(), av + vec2(11.0, 11.0), online, p.surface);
                let x = rect.left() + 44.0;
                let right = if unread > 0 { 34.0 } else { 6.0 };
                let sub = crate::members_ui::sub_line(voice.as_ref(), activity.as_ref(), online);
                let name_c = if unread > 0 || active { p.text } else { p.text_muted };
                let name = crate::banner::truncate_galley(ui.painter(), u.shown_name(), if unread > 0 { t::bold(14.0) } else { t::semibold(14.0) }, name_c, rect.right() - x - right);
                let top = if sub.is_some() { rect.center().y - 16.0 } else { rect.center().y - name.size().y / 2.0 };
                ui.painter().galley(pos2(x, top), name, name_c);
                if let Some((kind, text)) = sub {
                    let y = rect.center().y + 9.0;
                    let tx = if kind == crate::members_ui::Sub::Voice {
                        kit::wave(ui.painter(), pos2(x, y), 14.0, 1.4, ui.input(|i| i.time) as f32 * 14.0, p.speaking, kit::WaveStyle::Solid);
                        x + 18.0
                    } else {
                        x
                    };
                    crate::emoji::paint::line(ui.painter(), pos2(tx, y), &text, t::font(11.5), p.text_muted, rect.right() - tx - right);
                }
                if unread > 0 {
                    kit::count_badge(ui.painter(), pos2(rect.right() - 18.0, rect.center().y), &crate::unread::badge_text(unread), bg);
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    match dm {
                        Some(id) => self.view = View::Dm(id),
                        None => self.dms.open_with(u.id),
                    }
                }
            }
        });
    }

    /// Центр экрана «Друзья».
    fn ui_friends_screen(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let my_voice = vs.channel.as_ref().filter(|c| !crate::dms::is_dm(c) && vs.conn == Conn::Connected);
        let speaking: Vec<i64> = vs.peers.iter().filter(|p| p.speaking).filter_map(|p| p.identity.parse().ok()).collect();
        let mut offline_open = self.settings.friends_offline_open;
        let cx = crate::friends::ScreenCx {
            model: &self.model,
            people: &self.people,
            my_voice,
            speaking: &speaking,
            filter: &self.friend_filter,
            offline_open: &mut offline_open,
        };
        for a in self.friends.screen(ui, cx) {
            match a {
                crate::friends::Act::Join(ch) => self.join_server_channel(ch, vs),
                crate::friends::Act::Summon { channel_id, user_id } => self.send_summon(channel_id, user_id),
                crate::friends::Act::Message(uid) => self.dms.open_with(uid),
                crate::friends::Act::Profile(uid, pos) => self.members.open_card(uid, pos, None),
            }
        }
    }

    /// Войти в голосовой канал сервера (из друзей, карточки, приглашения «позвать»).
    fn join_server_channel(&mut self, ch: Channel, vs: &VoiceState) {
        if self.model.server(ch.server_id).is_some() {
            self.select_server(ch.server_id);
        }
        let here = vs.channel.as_ref().is_some_and(|c| c.id == ch.id) && vs.conn == Conn::Connected;
        if !here {
            self.voice.send(Cmd::Join(ch));
        }
        self.view = View::Voice;
    }

    fn send_summon(&mut self, channel_id: i64, user_id: i64) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.summon(channel_id, user_id).await {
                log::warn!("позвать {user_id} в канал {channel_id}: {e}");
            }
        });
    }

    /// Карточка профиля (поповер).
    fn ui_profile_card(&mut self, ctx: &egui::Context, vs: &VoiceState) {
        let my_voice = vs.channel.as_ref().filter(|_| vs.conn == Conn::Connected).map(|c| c.id);
        let me = self.me.as_ref().map(|u| u.id).unwrap_or(0);
        let friends = &self.friends;
        let act = self.members.card_ui(ctx, &self.model, &self.people, my_voice, me, &|id| friends.is_friend(id));
        match act {
            Some(crate::members_ui::Act::Join(ch)) => self.join_server_channel(ch, vs),
            Some(crate::members_ui::Act::Message(uid)) => self.dms.open_with(uid),
            Some(crate::members_ui::Act::AddFriend(login)) => {
                self.friends.add_by_login(&login);
                self.view = View::Friends;
            }
            None => {}
        }
    }

    /// «X зовёт в канал»: плашка сверху с «Присоединиться» (сама прячется через минуту).
    fn ui_summon(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let Some((from, ch_id, at)) = self.summon.clone() else { return };
        let here = vs.channel.as_ref().is_some_and(|c| c.id == ch_id);
        let place = self.model.channel_place(ch_id).map(|(s, c)| (s.name.clone(), c.clone()));
        if at.elapsed() > Duration::from_secs(60) || here || place.is_none() {
            self.summon = None;
            return;
        }
        let Some((server, ch)) = place else { return };
        let p = t::p();
        let screen = ui.ctx().content_rect();
        let mut close = false;
        let mut join = false;
        egui::Area::new(Id::new("summon")).order(egui::Order::Foreground).anchor(Align2::CENTER_TOP, vec2(0.0, 54.0)).show(ui.ctx(), |ui| {
            Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(kit::R_CARD))
                .shadow(egui::epaint::Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(60) })
                .inner_margin(Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_max_width((screen.width() - 40.0).min(560.0));
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                        self.people.paint(ui.painter(), r.center(), 17.0, &self.people.resolve(from.clone()), 0.0, false);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new(format!("{} зовёт в «{}»", from.shown_name(), ch.name)).font(t::semibold(14.5)));
                            ui.label(RichText::new(server).size(12.5).color(p.text_muted));
                        });
                        ui.add_space(12.0);
                        join = kit::button(ui, kit::ButtonKind::Primary, "Присоединиться", None).clicked();
                        close = kit::icon_button(ui, ph::X, kit::IconState::Normal, "Скрыть").clicked();
                    });
                });
        });
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if join {
            self.summon = None;
            self.join_server_channel(ch, vs);
        } else if close {
            self.summon = None;
        }
    }

    /// Окошко «Задать статус».
    fn ui_status_edit(&mut self, ctx: &egui::Context) {
        let Some(mut text) = self.status_edit.take() else { return };
        let mut save: Option<String> = None;
        let res = kit::modal(ctx, "status-edit", "Задать статус", 400.0, |ui| {
            ui.label(RichText::new("Его видят друзья и соседи по серверам вместо «играет в …».").size(13.0).color(t::p().text_muted));
            ui.add_space(8.0);
            let r = ui.add(kit::text_field(&mut text, ui.available_width()).hint_text("например, чиню сервер").char_limit(proto::limits::STATUS_MAX));
            r.request_focus();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if kit::button(ui, kit::ButtonKind::Primary, "Сохранить", None).clicked() || enter {
                    save = Some(text.trim().to_string());
                }
                let has = self.me.as_ref().is_some_and(|m| m.status.is_some());
                if has && kit::button(ui, kit::ButtonKind::Ghost, "Убрать статус", None).clicked() {
                    save = Some(String::new());
                }
            });
        });
        if let Some(s) = save {
            let api = self.api.clone();
            self.spawn(async move { api.update_profile(None, Some(s)).await.map(Some).map_err(|e| e.message) }, Msg::Profile);
            return;
        }
        if res != kit::ModalResult::Closed {
            self.status_edit = Some(text);
        }
    }

    /// Активность: фоновая проверка игр (если разрешено) и отправка изменений на сервер.
    fn sync_activity(&mut self) {
        if self.screen != Screen::Main || self.demo {
            return;
        }
        if self.settings.share_activity {
            if self.activity.is_none() {
                self.activity = Some(crate::activity::start(self.ctx.clone()));
            }
        } else if self.activity.take().is_some() {
            self.my_game = None;
        }
        if let Some(g) = self.activity.as_ref().and_then(|d| d.poll()) {
            self.my_game = g;
        }
        if self.activity_failed.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.game_sent = None; // прошлая отправка не дошла — повторить
        }
        let want = if self.settings.share_activity { self.my_game.clone() } else { None };
        if self.game_sent.as_ref() == Some(&want) {
            return;
        }
        self.game_sent = Some(want.clone());
        let api = self.api.clone();
        let failed = self.activity_failed.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.set_activity(want.as_ref().map(|g| g.0.clone()), want.map(|g| g.1)).await {
                log::warn!("активность не отправлена: {e}");
                // повтор — через несколько секунд, не в плотном цикле
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                failed.store(true, std::sync::atomic::Ordering::Relaxed);
                ctx.request_repaint();
            }
        });
    }

    fn ui_header(&mut self, ui: &mut Ui, vs: &VoiceState) {
        egui::Panel::top("head")
            .exact_size(64.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t::p().bg).inner_margin(Margin::symmetric(24, 0)))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(r.x_range().expand(24.0), r.bottom(), Stroke::new(1.0, t::p().border));
                let text_ch = match self.view {
                    View::Text(id) => self.channels.iter().find(|c| c.id == id).map(|c| c.name.clone()),
                    View::Voice | View::Dm(_) | View::Friends => None,
                };
                if self.view == View::Friends {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        ui.label(RichText::new(egui_phosphor::fill::USERS).size(20.0).color(t::p().text_muted));
                        ui.label(RichText::new("Друзья").font(t::xbold(18.0)));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.allocate_ui(vec2(320.0, 40.0), |ui| self.friends.header_tabs(ui));
                        });
                    });
                    return;
                }
                if let View::Dm(id) = self.view
                    && let Some(dm) = self.dms.get(id).cloned()
                {
                    let me = self.me.as_ref().map(|u| u.id).unwrap_or(0);
                    let in_call = vs.channel.as_ref().is_some_and(|c| crate::dms::is_dm(c) && c.id == id) && vs.conn != Conn::Idle;
                    if let Some(crate::dms::HeaderAct::Call) = crate::dms::header(ui, &self.people, &dm, self.friends.online(dm.user.id), me, in_call) {
                        self.voice.send(Cmd::Join(crate::dms::call_channel(&dm)));
                    }
                    return;
                }
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    // имя канала не шире экрана: справа остаётся место для кнопок шапки
                    let name_max = (ui.available_width() - 340.0).max(160.0);
                    if let Some(name) = text_ch {
                        ui.label(RichText::new(ph::HASH).size(20.0).color(t::p().text_muted));
                        ui.scope(|ui| {
                            ui.set_max_width(name_max);
                            ui.add(egui::Label::new(RichText::new(name).font(t::xbold(18.0))).truncate());
                        });
                    } else {
                    match &vs.channel {
                        Some(ch) => {
                            let icon = if crate::dms::is_dm(ch) { ph::PHONE } else { ph::SPEAKER_HIGH };
                            ui.label(RichText::new(icon).size(20.0).color(t::p().text_muted));
                            ui.scope(|ui| {
                                ui.set_max_width(name_max);
                                ui.add(egui::Label::new(RichText::new(&ch.name).font(t::xbold(18.0))).truncate());
                            });
                            if vs.conn == Conn::Connected {
                                ui.label(
                                    RichText::new(t::plural(vs.peers.len(), "участник", "участника", "участников"))
                                        .size(13.5)
                                        .color(t::p().text_muted),
                                );
                            }
                        }
                        None => {
                            ui.label(RichText::new("Не в канале").font(t::xbold(18.0)).color(t::p().text_muted));
                        }
                    }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.model.active.is_some() && !self.demo {
                            let on = self.settings.members_open;
                            let state = if on { kit::IconState::Active } else { kit::IconState::Normal };
                            if kit::icon_button(ui, ph::USERS, state, if on { "Скрыть участников" } else { "Участники" }).clicked() {
                                self.settings.members_open = !on;
                                self.settings.save();
                            }
                        }
                        if let Some(rtt) = vs.rtt {
                            ping_box(ui, rtt, vs.jitter, vs.loss, vs.link);
                        }
                        if let Some(s) = &vs.stream {
                            ui.add_space(4.0);
                            stream_pill(ui, s);
                        }
                    });
                });
            });
    }

    /// Нижняя панель управления — плавающая капсула по центру: круглые кнопки-значки (микрофон, звук, экран),
    /// разделитель и «Отключиться» (единственная красная кнопка).
    /// Экран звонка: голосовой канал сервера либо диалог, где идёт этот звонок (сцена — над перепиской).
    fn on_call_screen(&self, vs: &VoiceState) -> bool {
        match self.view {
            View::Voice => true,
            View::Dm(id) => self.dm_call_here(vs, id),
            _ => false,
        }
    }

    /// Идёт (или набирается) звонок именно в этом диалоге.
    fn dm_call_here(&self, vs: &VoiceState, id: i64) -> bool {
        vs.channel.as_ref().is_some_and(|c| crate::dms::is_dm(c) && c.id == id) && vs.conn != Conn::Idle
    }

    fn ui_dock(&mut self, ui: &mut Ui, vs: &VoiceState) {
        // капсула — только в голосовом канале и на его экране; в чатах управление — в панели пользователя слева
        if vs.channel.is_none() || self.view != View::Voice {
            return;
        }
        self.dock_panel(ui, vs);
    }

    fn dock_panel(&mut self, ui: &mut Ui, vs: &VoiceState) {
        egui::Panel::bottom("dock")
            .exact_size(84.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t::p().bg))
            .show(ui, |ui| {
                let r = ui.max_rect();
                let in_room = vs.channel.is_some();
                let mic_off = vs.self_muted || vs.deafened;
                let (mic_icon, mic_style, mic_tip) = if mic_off {
                    (ph::MICROPHONE_SLASH, DockStyle::Off, "Включить микрофон")
                } else if vs.transmitting {
                    (ph::MICROPHONE, DockStyle::Live, "Выключить микрофон")
                } else {
                    (ph::MICROPHONE, DockStyle::Normal, "Выключить микрофон")
                };
                let (snd_icon, snd_style, snd_tip) =
                    if vs.deafened { (ph::SPEAKER_SLASH, DockStyle::Off, "Включить звук") } else { (ph::HEADPHONES, DockStyle::Normal, "Выключить звук") };
                let key = ptt::key_name(self.settings.ptt_key);
                let can_stream = in_room && vs.conn == Conn::Connected;
                let streaming = vs.stream.is_some();

                // ширина содержимого — чтобы капсула облегала кнопки
                const GAP: f32 = 10.0;
                let measure = |text: &str, font: egui::FontId| ui.painter().layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER).size().x;
                let icons = 2 + usize::from(streaming || can_stream) + usize::from(streaming);
                let mut w = icons as f32 * 46.0 + (icons - 1) as f32 * GAP;
                if vs.ptt_enabled {
                    w += GAP + measure("Рация", t::font(13.0)) + 6.0 + measure(&key, t::bold(13.0)) + 18.0;
                }
                if in_room {
                    w += GAP + 1.0 + GAP + 18.0 + 20.0 + 9.0 + measure("Отключиться", t::bold(14.0)) + 20.0;
                }
                let cap = Rect::from_center_size(pos2(r.center().x, r.center().y - 4.0), vec2(w + 16.0, 62.0));
                let p = t::p();
                let painter = ui.painter();
                painter.add(egui::epaint::Shadow { offset: [0, 4], blur: 18, spread: 0, color: Color32::from_black_alpha(36) }.as_shape(cap, CornerRadius::same(30)));
                painter.rect(cap, CornerRadius::same(30), p.bg, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                ui.scope_builder(egui::UiBuilder::new().max_rect(cap.shrink2(vec2(8.0, 8.0))), |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = GAP;
                        if kit::dock_icon(ui, mic_icon, mic_style, mic_tip).clicked() {
                            self.voice.send(Cmd::SetSelfMuted(!mic_off));
                        }
                        if kit::dock_icon(ui, snd_icon, snd_style, snd_tip).clicked() {
                            self.voice.send(Cmd::SetDeafened(!vs.deafened));
                        }
                        if streaming {
                            if kit::dock_icon(ui, ph::MONITOR_ARROW_UP, DockStyle::Live, "Остановить трансляцию").clicked() {
                                self.voice.send(Cmd::StopStream);
                            }
                            if kit::dock_icon(ui, ph::SWAP, DockStyle::Normal, "Сменить окно или экран без остановки").clicked() {
                                self.open_picker(vs);
                            }
                        } else if can_stream && kit::dock_icon(ui, ph::MONITOR_ARROW_UP, DockStyle::Normal, "Показать окно или экран").clicked() {
                            self.open_picker(vs);
                        }
                        if vs.ptt_enabled {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(RichText::new("Рация").size(13.0).color(p.text_muted));
                            kit::keycap(ui, &key).on_hover_text(format!("Режим рации: микрофон включён, пока держите {key}"));
                            ui.spacing_mut().item_spacing.x = GAP;
                        }
                        if in_room {
                            let (sep, _) = ui.allocate_exact_size(vec2(1.0, 30.0), Sense::hover());
                            ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, p.border));
                            if kit::dock_button(ui, ph::PHONE_X, "Отключиться", DockStyle::Leave).clicked() {
                                self.voice.send(Cmd::Leave);
                            }
                        }
                    });
                });
            });
    }

    fn ui_stage(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let r = ui.max_rect();
        match (&vs.conn, &vs.channel) {
            (Conn::Failed(msg), _) => {
                let msg = msg.clone();
                let area = Rect::from_center_size(r.center(), vec2(520.0, 260.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                    let retry = vs.channel.is_some().then_some("Повторить");
                    if kit::cat_state(ui, kit::Cat::Offline, "Связь пропала", &msg, retry)
                        && let Some(ch) = vs.channel.clone()
                    {
                        self.voice.send(Cmd::Join(ch));
                    }
                });
            }
            // Ближайшее состояние кота — «выглядывает» (отдельного «выберите канал» в пакете нет).
            (_, None) => {
                let area = Rect::from_center_size(r.center(), vec2(420.0, 200.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                    kit::cat_state(ui, kit::Cat::Peek, "Выберите голосовой канал", "Список каналов — слева", None);
                });
            }
            // звоним другу — ждём ответа
            (Conn::Connecting | Conn::Connected, Some(ch))
                if crate::dms::is_dm(ch) && vs.peers.len() <= 1 && self.dms.get(ch.id).is_some_and(|d| d.call.as_ref().is_some_and(|c| c.ringing)) =>
            {
                let (id, name) = (ch.id, ch.name.clone());
                let area = Rect::from_center_size(r.center(), vec2(420.0, 220.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                    if kit::cat_state(ui, kit::Cat::Loading, &format!("Звоним: {name}…"), "Ждём ответа", Some("Отменить")) {
                        self.voice.send(Cmd::Leave);
                        self.view = View::Dm(id);
                    }
                });
            }
            (Conn::Connecting, Some(_)) if vs.peers.is_empty() => {
                let area = Rect::from_center_size(r.center(), vec2(420.0, 200.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                    kit::cat_state(ui, kit::Cat::Loading, "Подключаемся…", "Ещё секунду", None);
                });
            }
            // В канале только я и никто не стримит — «Тут тихо» (или «нет микрофона», если в системе нет ни одного).
            (Conn::Connected, Some(_)) if vs.peers.len() <= 1 && !vs.peers.iter().any(|p| p.streaming) => {
                let area = Rect::from_center_size(r.center(), vec2(420.0, 200.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                    if vs.inputs.is_empty() {
                        kit::cat_state(ui, kit::Cat::NoMic, "Не слышим микрофон", "Проверь подключение и доступ в Windows", None);
                    } else {
                        kit::cat_state(ui, kit::Cat::Sleep, "Тут тихо", "Позови своих — они появятся здесь", None);
                    }
                });
            }
            _ => self.ui_room(ui, vs),
        }
    }

    /// Комната: сетка (участники + трансляции) или «большой вид» трансляции с лентой остальных.
    fn ui_room(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let r = ui.max_rect();
        // Трансляции — первыми (как в Discord), затем участники.
        let mut items: Vec<Item> = vs.peers.iter().filter(|p| p.streaming).map(Item::Stream).collect();
        items.extend(vs.peers.iter().map(Item::Person));
        let focused = self.focus.as_ref().and_then(|f| items.iter().position(|i| matches!(i, Item::Stream(p) if &p.identity == f)));
        let Some(fi) = focused else {
            if crate::anim::compact_tiles() {
                // Компактно: трансляции сеткой сверху, люди — плитками 200×68 рядами снизу (или по центру, если трансляций нет).
                let (streams, people): (Vec<Item>, Vec<Item>) = items.iter().partition(|i| matches!(i, Item::Stream(_)));
                let rects = compact_rects(r, people.len(), 12.0);
                let block_top = rects.iter().map(|x| x.top()).fold(r.bottom(), f32::min);
                let rects = if streams.is_empty() {
                    let dy = (r.top() + r.bottom() - block_top - r.bottom()) / 2.0;
                    rects.into_iter().map(|x| x.translate(vec2(0.0, dy))).collect()
                } else {
                    rects
                };
                let stream_area = Rect::from_min_max(r.min, pos2(r.right(), block_top - 16.0));
                for (item, rect) in streams.iter().zip(grid_rects(stream_area, streams.len(), 9.0 / 16.0, 16.0)) {
                    self.ui_item(ui, rect, *item, vs, false);
                }
                for (item, rect) in people.iter().zip(rects) {
                    self.ui_item(ui, rect, *item, vs, false);
                }
                return;
            }
            let aspect = if items.iter().any(|i| matches!(i, Item::Stream(_))) { 9.0 / 16.0 } else { 0.8 };
            for (item, rect) in items.iter().zip(grid_rects(r, items.len(), aspect, 16.0)) {
                self.ui_item(ui, rect, *item, vs, false);
            }
            return;
        };
        let big_item = items.remove(fi);
        let gap = 14.0;
        let strip_h = if items.is_empty() { 0.0 } else { (r.height() * 0.2).clamp(120.0, 160.0) };
        let big = Rect::from_min_max(r.min, pos2(r.right(), r.bottom() - if strip_h > 0.0 { strip_h + gap } else { 0.0 }));
        self.ui_item(ui, big, big_item, vs, true);
        if strip_h > 0.0 {
            // 16:9, а если не помещаются все — уже (но не меньше 130 px), остальные за краем.
            let n = items.len() as f32;
            let tw = (strip_h * 16.0 / 9.0).min((r.width() - gap * (n - 1.0)) / n).max(130.0);
            let fit = (((r.width() + gap) / (tw + gap)).floor() as usize).max(1).min(items.len());
            let row_w = tw * fit as f32 + gap * (fit - 1) as f32;
            let left = r.center().x - row_w / 2.0;
            for (i, item) in items.iter().take(fit).enumerate() {
                let rect = Rect::from_min_size(pos2(left + i as f32 * (tw + gap), r.bottom() - strip_h), vec2(tw, strip_h));
                self.ui_item(ui, rect, *item, vs, false);
            }
        }
    }

    fn ui_item(&mut self, ui: &mut Ui, rect: Rect, item: Item, vs: &VoiceState, big: bool) {
        match item {
            Item::Person(p) => {
                let resp = ui.interact(rect, Id::new(("tile", &p.identity)), Sense::click());
                let amp = speech_amp(ui, p);
                tile(ui, rect, p, amp, &self.people);
                let at = resp.interact_pointer_pos().unwrap_or(rect.center());
                // ЛКМ — карточка профиля, ПКМ — громкость (решение владельца 22.11)
                if resp.clicked()
                    && let Ok(uid) = p.identity.parse::<i64>()
                {
                    self.members.open_card(uid, at, None);
                } else if resp.secondary_clicked() && !p.is_local {
                    self.open_popup(p, at);
                }
                resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
            Item::Stream(p) => self.stream_tile(ui, rect, p, vs, big),
        }
    }

    /// ЛКМ по строке участника — карточка, ПКМ — громкость.
    fn peer_row_click(&mut self, r: &egui::Response, p: &Peer) {
        let at = r.interact_pointer_pos().unwrap_or(r.rect.center());
        if r.clicked()
            && let Ok(uid) = p.identity.parse::<i64>()
        {
            self.members.open_card(uid, at, None);
        } else if r.secondary_clicked() && !p.is_local {
            self.open_popup(p, at);
        }
    }

    fn open_popup(&mut self, p: &Peer, pos: Pos2) {
        self.popup = Some(VolumePopup { identity: p.identity.clone(), name: p.name.clone(), pos, opened_at: Instant::now() });
    }

    /// Плитка трансляции: «Смотреть» → видео (вписано с полями) → по клику большой вид, двойной клик — полный экран.
    fn stream_tile(&mut self, ui: &mut Ui, rect: Rect, p: &Peer, vs: &VoiceState, big: bool) {
        let watching = vs.watching.contains(&p.identity);
        let resp = ui.interact(rect, Id::new(("stream", &p.identity)), Sense::click());
        let hovered = ui.rect_contains_pointer(rect);
        let painter = ui.painter().clone();
        let radius = CornerRadius::same(kit::R_CARD);
        kit::stream_tile_frame(&painter, rect, false);
        let small = rect.height() < 200.0;
        // мышь над видео управляет ПК друга — плитка на клики не реагирует
        let mut rc_captured = false;

        if p.is_local {
            // Своё видео не декодируем (лишняя нагрузка) — только подпись.
            let title = vs.stream.as_ref().map(|s| s.title.clone()).unwrap_or_default();
            let c = rect.center();
            painter.text(c - vec2(0.0, if small { 14.0 } else { 26.0 }), Align2::CENTER_CENTER, egui_phosphor::fill::MONITOR_ARROW_UP, t::icon_fill(if small { 26.0 } else { 40.0 }), t::p().accent);
            painter.text(c + vec2(0.0, if small { 12.0 } else { 14.0 }), Align2::CENTER_CENTER, "Вы транслируете", t::bold(if small { 13.0 } else { 15.0 }), ON_VIDEO);
            if !small {
                painter.text(c + vec2(0.0, 36.0), Align2::CENTER_CENTER, ellipsize(&title, 40), t::font(13.0), ON_VIDEO_MUTED);
            }
        } else if watching {
            match self.video.frame(&p.identity) {
                Some(f) => {
                    let vr = fit_rect(rect, f.size);
                    egui::Image::new(egui::load::SizedTexture::new(f.texture, vr.size())).corner_radius(radius).paint_at(ui, vr);
                    self.mark_view(&p.identity, vr, ui.ctx().pixels_per_point());
                    rc_captured = crate::remote_ui::guest_layer(ui, rect, vr, &p.identity, vs, &self.voice, hovered);
                    // Стример сам сообщает, почему картинка стоит; иначе — догадка по отсутствию кадров.
                    match vs.stream_paused.get(&p.identity) {
                        Some(voice::StreamPause::Minimized) => {
                            chip(&painter, rect.center(), Align2::CENTER_CENTER, &format!("{}  Стример свернул окно — картинка на паузе", ph::EYE_SLASH), ON_VIDEO);
                        }
                        Some(voice::StreamPause::Hidden) => {
                            chip(&painter, rect.center(), Align2::CENTER_CENTER, &format!("{}  Окно стримера закрыто другим окном — картинка на паузе", ph::EYE_SLASH), ON_VIDEO);
                        }
                        None if f.stale => {
                            chip(&painter, rect.center(), Align2::CENTER_CENTER, &format!("{}  Нет новых кадров — пропала связь со стримером", ph::HOURGLASS), ON_VIDEO_MUTED);
                        }
                        None => {}
                    }
                    // Экономный слой (720p30) — пометка, чтобы не гадать, почему картинка мягче.
                    let low = vs.stream_layers.get(&p.identity).is_some_and(|i| i.low);
                    if big && hovered && f.fps > 0.0 {
                        let tag = if low { " · экономное" } else { "" };
                        chip(&painter, rect.left_top() + vec2(12.0, 12.0), Align2::LEFT_TOP, &format!("{}×{} · {:.0} к/с{tag}", f.size[0], f.size[1], f.fps), ON_VIDEO_MUTED);
                    } else if low && rect.height() >= 140.0 {
                        chip(&painter, rect.left_top() + vec2(12.0, 12.0), Align2::LEFT_TOP, "Экономное качество", ON_VIDEO_MUTED);
                    }
                }
                None => {
                    self.mark_view(&p.identity, rect, ui.ctx().pixels_per_point());
                    egui::Spinner::new().size(30.0).color(t::p().accent).paint_at(ui, Rect::from_center_size(rect.center() - vec2(0.0, 12.0), vec2(30.0, 30.0)));
                    painter.text(rect.center() + vec2(0.0, 22.0), Align2::CENTER_CENTER, "Подключаемся к трансляции…", t::font(13.0), ON_VIDEO_MUTED);
                    ui.ctx().request_repaint_after(Duration::from_millis(50));
                }
            }
        } else {
            // Не смотрим: аватар стримера и «Смотреть» (канал не тратится, пока не нажали).
            // Совсем маленькая плитка (лента, много трансляций) — только кнопка, без аватара.
            let tiny = rect.height() < 140.0;
            let ar = (rect.height() * 0.16).clamp(16.0, 34.0);
            let ac = rect.center() - vec2(0.0, ar + if small { 4.0 } else { 14.0 });
            if !tiny {
                self.people.paint(&painter, ac, ar, &peer_user(&self.people, p), 0.0, false);
            }
            if !small {
                painter.text(ac + vec2(0.0, ar + 18.0), Align2::CENTER_CENTER, format!("{} транслирует экран", peer_name(&self.people, p)), t::semibold(14.0), ON_VIDEO);
            }
            let bw = if small { 116.0 } else { 150.0 };
            let by = if tiny { rect.center().y - 4.0 } else { ac.y + ar + if small { 22.0 } else { 52.0 } };
            let br = Rect::from_center_size(pos2(rect.center().x, by), vec2(bw, if small { 30.0 } else { 38.0 }));
            painter.rect_filled(br, CornerRadius::same(255), if hovered { t::p().accent_strong_hover } else { t::p().accent_strong });
            painter.text(br.center(), Align2::CENTER_CENTER, format!("{}  Смотреть", ph::PLAY), t::bold(if small { 13.0 } else { 14.5 }), t::p().on_accent);
        }

        if big || hovered {
            painter.rect_stroke(rect, radius, Stroke::new(2.0, t::p().accent.gamma_multiply(if big { 0.6 } else { 1.0 })), StrokeKind::Inside);
        }
        kit::live_badge(&painter, pos2(rect.right() - 12.0, rect.top() + 12.0));
        chip(&painter, rect.left_bottom() + vec2(12.0, -12.0), Align2::LEFT_BOTTOM, &format!("{}  {}", ph::MONITOR, peer_name(&self.people, p)), ON_VIDEO);

        if p.is_local {
            return;
        }
        // Кнопки поверх видео (появляются при наведении).
        let mut act = None;
        if rc_captured {
            return;
        }
        if watching && hovered {
            let mut c = pos2(rect.right() - 30.0, rect.bottom() - 30.0);
            if overlay_button(ui, c, ph::X, "Перестать смотреть", Id::new(("s-stop", &p.identity))).clicked() {
                act = Some(StreamAct::Unwatch);
            }
            c.x -= 44.0;
            if overlay_button(ui, c, ph::CORNERS_OUT, "Полный экран (F11)", Id::new(("s-full", &p.identity))).clicked() {
                act = Some(StreamAct::Fullscreen);
            }
            c.x -= 44.0;
            if overlay_button(ui, c, ph::PICTURE_IN_PICTURE, "Окно в окне", Id::new(("s-pip", &p.identity))).clicked() {
                act = Some(StreamAct::Pip);
            }
            if big {
                c.x -= 44.0;
                if overlay_button(ui, c, ph::SQUARES_FOUR, "Все участники (Esc)", Id::new(("s-grid", &p.identity))).clicked() {
                    act = Some(StreamAct::Grid);
                }
            }
        }
        if act.is_none() {
            if resp.double_clicked() && watching {
                act = Some(StreamAct::Fullscreen);
            } else if resp.clicked() {
                act = Some(if watching { StreamAct::Focus } else { StreamAct::Watch });
            } else if resp.secondary_clicked() {
                self.open_popup(p, resp.interact_pointer_pos().unwrap_or(rect.center()));
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        match act {
            Some(StreamAct::Watch) => {
                // Как в Discord: «Смотреть» сразу открывает трансляцию крупно.
                self.watch(&p.identity);
            }
            Some(StreamAct::Focus) => self.focus = Some(p.identity.clone()),
            Some(StreamAct::Fullscreen) => {
                self.focus = Some(p.identity.clone());
                self.set_fullscreen(ui.ctx(), true);
            }
            Some(StreamAct::Grid) => self.focus = None,
            Some(StreamAct::Pip) => {
                self.pip.pin(Some(p.identity.clone()));
                self.pip.force();
            }
            Some(StreamAct::Unwatch) => self.unwatch(ui.ctx(), &p.identity),
            None => {}
        }
    }

    fn watch(&mut self, identity: &str) {
        self.pip.reset();
        self.voice.send(Cmd::Watch { identity: identity.to_string(), on: true });
        self.watch_pending = Some((identity.to_string(), Instant::now()));
        self.focus = Some(identity.to_string());
    }

    fn unwatch(&mut self, ctx: &egui::Context, identity: &str) {
        self.voice.send(Cmd::Watch { identity: identity.to_string(), on: false });
        if self.focus.as_deref() == Some(identity) {
            self.focus = None;
            self.set_fullscreen(ctx, false);
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            self.pointer_moved_at = Instant::now();
            crate::frame::set_fullscreen_style(on);
            // Из развёрнутого окна winit оставляет клиентскую область размером рабочей области (экран минус панель
            // задач) — снизу была чёрная полоса высотой с панель задач. Поэтому сначала «восстановить», потом полный экран.
            if on {
                self.fs_was_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                if self.fs_was_maximized {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                if std::mem::take(&mut self.fs_was_maximized) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                }
            }
        }
    }

    /// Трансляция нарисована в `rect` в этом кадре (для отчёта движку о видимости и размере).
    fn mark_view(&mut self, identity: &str, rect: Rect, ppp: f32) {
        self.views_frame.insert(identity.to_string(), ((rect.width() * ppp) as u32, (rect.height() * ppp) as u32));
    }

    /// Сообщает движку, какие трансляции видны и какого они размера; невидимые SFU ставит на паузу.
    fn report_views(&mut self, vs: &VoiceState) {
        let seen = std::mem::take(&mut self.views_frame);
        for id in &vs.watching {
            let (visible, w, h) = match seen.get(id) {
                Some(&(w, h)) => (true, w, h),
                None => (false, 0, 0),
            };
            let changed = |a: u32, b: u32| (a as f32 - b as f32).abs() > 0.2 * a.max(b).max(1) as f32;
            let send = match self.views_sent.get(id) {
                None => true,
                Some(&(v, pw, ph)) => v != visible || (visible && (changed(pw, w) || changed(ph, h))),
            };
            if send {
                self.views_sent.insert(id.clone(), (visible, w, h));
                self.voice.send(Cmd::StreamView { identity: id.clone(), visible, width: w, height: h });
            }
        }
        self.views_sent.retain(|id, _| vs.watching.contains(id));
    }

    /// Фокус/полный экран: сброс, если трансляция закончилась; клавиши F11 и Esc.
    fn sync_focus(&mut self, ui: &Ui, vs: &VoiceState) {
        let ctx = ui.ctx().clone();
        if self.watch_pending.as_ref().is_some_and(|(id, at)| vs.watching.contains(id) || at.elapsed() > Duration::from_secs(2)) {
            self.watch_pending = None;
        }
        let pending = self.watch_pending.as_ref().map(|(id, _)| id.clone());
        let alive = |id: &str| {
            (vs.watching.iter().any(|w| w == id) || pending.as_deref() == Some(id)) && vs.peers.iter().any(|p| p.identity == id && p.streaming)
        };
        if let Some(f) = &self.focus
            && !alive(f)
        {
            self.focus = None;
        }
        if self.fullscreen && self.focus.is_none() {
            self.set_fullscreen(&ctx, false);
        }
        let modal_open =
            self.settings_open || self.picker.is_some() || self.popup.is_some() || self.update_open || self.servers.modal_open() || self.channel_list.modal_open() || self.server_settings.is_open() || !self.on_call_screen(vs);
        let (f11, esc) = ui.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if f11 && !modal_open {
            if self.fullscreen {
                self.set_fullscreen(&ctx, false);
            } else {
                if self.focus.is_none() {
                    // Смотрим ровно одну трансляцию — F11 сразу разворачивает её.
                    let watched: Vec<&String> = vs.watching.iter().filter(|w| alive(w)).collect();
                    if let [one] = watched.as_slice() {
                        self.focus = Some((*one).clone());
                    }
                }
                if self.focus.is_some() {
                    self.set_fullscreen(&ctx, true);
                }
            }
        }
        if esc && !modal_open {
            if self.fullscreen {
                self.set_fullscreen(&ctx, false);
            } else if self.focus.is_some() {
                self.focus = None;
            }
        }
    }

    /// Полноэкранный просмотр: только видео; панель и курсор прячутся, пока мышь не двигается.
    fn ui_fullscreen(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let Some(id) = self.focus.clone() else { return };
        let Some(p) = vs.peers.iter().find(|p| p.identity == id) else { return };
        if ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_down()) {
            self.pointer_moved_at = Instant::now();
        }
        let idle = self.pointer_moved_at.elapsed() > Duration::from_millis(2500);
        // Чёрным — всё окно, а не только панель: при дробном масштабе (125 %, 150 %) и пока окно меняет размер
        // край панели может не дойти до края экрана, и снизу оставалась полоса фона.
        let ctx = ui.ctx().clone();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(ctx.viewport_rect().expand(4.0), 0.0, Color32::BLACK);
        egui::CentralPanel::no_frame().frame(Frame::new().fill(Color32::BLACK)).show(ui, |ui| {
            let r = ui.max_rect();
            let resp = ui.interact(r, Id::new("fs-video"), Sense::click());
            let mut rc_captured = false;
            match self.video.frame(&id) {
                Some(f) => {
                    let vr = fit_rect(r, f.size);
                    egui::Image::new(egui::load::SizedTexture::new(f.texture, vr.size())).paint_at(ui, vr);
                    self.mark_view(&id, vr, ui.ctx().pixels_per_point());
                    rc_captured = crate::remote_ui::guest_layer(ui, r, vr, &id, vs, &self.voice, !idle);
                    if !idle {
                        chip(ui.painter(), r.left_top() + vec2(20.0, 20.0), Align2::LEFT_TOP, &format!("{}  {} · {}×{} · {:.0} к/с", ph::MONITOR, p.name, f.size[0], f.size[1], f.fps), ON_VIDEO);
                    }
                }
                None => {
                    self.mark_view(&id, r, ui.ctx().pixels_per_point());
                    egui::Spinner::new().size(36.0).color(t::p().accent).paint_at(ui, Rect::from_center_size(r.center(), vec2(36.0, 36.0)));
                }
            }
            if idle {
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                // Проверить ещё раз, когда пора прятать/показывать.
                ui.ctx().request_repaint_after(Duration::from_millis(250));
                if resp.double_clicked() && !rc_captured {
                    self.set_fullscreen(ui.ctx(), false);
                }
                return;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            chip(ui.painter(), pos2(r.center().x, r.top() + 20.0), Align2::CENTER_TOP, "Esc или двойной клик — выйти из полного экрана", ON_VIDEO_MUTED);
            // громкость звука трансляции — слева внизу, та же настройка, что в окошке участника
            let mut svol = self.settings.stream_volume_of(&id);
            let pill = Rect::from_min_size(pos2(r.left() + 24.0, r.bottom() - 54.0), vec2(250.0, 36.0));
            ui.painter().rect_filled(pill, 18.0, t::overlay_dark(190));
            let icon = if svol <= 0.001 { ph::SPEAKER_SLASH } else if svol < 0.5 { ph::SPEAKER_LOW } else { ph::SPEAKER_HIGH };
            let mut changed = false;
            let mute = ui.interact(Rect::from_center_size(pill.left_center() + vec2(20.0, 0.0), vec2(32.0, 32.0)), Id::new("fs-mute"), Sense::click());
            ui.painter().text(pill.left_center() + vec2(20.0, 0.0), Align2::CENTER_CENTER, icon, t::font(18.0), if mute.hovered() { t::p().accent } else { ON_VIDEO });
            if mute.on_hover_text(if svol <= 0.001 { "Включить звук трансляции" } else { "Выключить звук трансляции" }).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                svol = if svol <= 0.001 { 1.0 } else { 0.0 };
                changed = true;
            }
            ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_min_size(pill.left_top() + vec2(40.0, 7.0), vec2(204.0, 22.0))), |ui| {
                if kit::slider(ui, &mut svol, 0.0..=2.0, 204.0).changed() {
                    changed = true;
                }
            });
            if changed {
                if (svol - 1.0).abs() < 0.04 {
                    svol = 1.0;
                }
                self.settings.stream_volume.insert(id.clone(), svol);
                self.settings.save();
                self.voice.send(Cmd::SetStreamVolume(id.clone(), svol));
            }
            let mut c = pos2(r.right() - 36.0, r.bottom() - 36.0);
            if overlay_button(ui, c, ph::X, "Перестать смотреть", Id::new("fs-stop")).clicked() {
                self.unwatch(ui.ctx(), &id);
                return;
            }
            c.x -= 44.0;
            if overlay_button(ui, c, ph::CORNERS_IN, "Выйти из полного экрана (Esc)", Id::new("fs-exit")).clicked() || (resp.double_clicked() && !rc_captured) {
                self.set_fullscreen(ui.ctx(), false);
            }
        });
    }

    /// Указка и управление: запрос, плашка «Забрать», уведомления; запрос поднимает окно из трея.
    fn ui_remote(&mut self, ctx: &egui::Context, vs: &VoiceState) {
        let n = vs.rc_host.pending.len();
        if n > self.rc_asked && !desktop::is_visible(self.window) {
            desktop::show(self.window);
        }
        self.rc_asked = n;
        crate::remote_ui::host_dialog(ctx, vs, &self.voice);
        crate::remote_ui::host_bar(ctx, vs, &self.voice);
        crate::remote_ui::notice(ctx, vs);
        let focused = ctx.input(|i| i.focused);
        if self.rc_focus != Some(focused) {
            self.rc_focus = Some(focused);
            self.voice.send(Cmd::RcGuestFocus(focused));
        }
    }

    fn ui_banner(&mut self, ui: &mut Ui, vs: &VoiceState) {
        if let Some(n) = &vs.stream_notice {
            egui::Area::new(Id::new("stream-notice"))
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_BOTTOM, vec2(130.0, -96.0))
                .show(ui.ctx(), |ui| {
                    let r = Frame::new()
                        .fill(t::p().error_bg)
                        .stroke(Stroke::new(1.0, t::p().error_border))
                        .corner_radius(CornerRadius::same(255))
                        .inner_margin(Margin::symmetric(16, 8))
                        .show(ui, |ui| {
                            ui.label(RichText::new(format!("{}  {n}   {}", ph::MONITOR, ph::X)).color(t::p().danger_text).size(13.5));
                        })
                        .response
                        .interact(Sense::click());
                    if r.on_hover_text("Скрыть").clicked() {
                        self.voice.send(Cmd::DismissStreamNotice);
                    }
                });
        }
        let text = match vs.conn {
            Conn::Reconnecting => "Связь пропала — переподключаемся…".to_string(),
            Conn::Rejoining { attempt } => format!("Связь пропала — возвращаемся в канал (попытка {attempt})…"),
            _ => return,
        };
        let screen = ui.ctx().content_rect();
        egui::Area::new(Id::new("banner"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, vec2(130.0, 76.0))
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(t::p().warn_bg)
                    .stroke(Stroke::new(1.0, t::p().warn_border))
                    .corner_radius(CornerRadius::same(255))
                    .inner_margin(Margin::symmetric(16, 8))
                    .show(ui, |ui| {
                        ui.label(RichText::new(format!("{}  {text}", ph::ARROWS_CLOCKWISE)).color(t::p().text).size(13.5));
                    });
            });
        let _ = screen;
    }

    /// Отладочный оверлей (F3): путь до SFU, RTT и строка на каждый поток. `AMADEUS_METRICS=1` — открыт при старте.
    fn ui_metrics(&mut self, ui: &mut Ui) {
        let f3 = ui.input(|i| i.key_pressed(egui::Key::F3));
        if f3 || (!self.metrics_init && std::env::var_os("AMADEUS_METRICS").is_some()) {
            self.metrics_open = !self.metrics_open;
            self.voice.send(Cmd::Metrics(self.metrics_open));
        }
        self.metrics_init = true;
        if !self.metrics_open {
            return;
        }
        use crate::metrics::{Dir, Path};
        let m = self.voice.state.lock().unwrap().metrics.clone();
        let mono = |s: String, c: Color32| RichText::new(s).monospace().size(12.0).color(c);
        let loss_color = |p: Option<f64>| match p {
            Some(p) if p >= 5.0 => t::p().danger,
            Some(p) if p >= 2.0 => t::p().accent,
            _ => t::p().text,
        };
        let mut dump = false;
        egui::Area::new(Id::new("metrics-overlay"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::RIGHT_TOP, vec2(-12.0, if self.fullscreen { 12.0 } else { 52.0 }))
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(Color32::from_black_alpha(217))
                    .stroke(Stroke::new(1.0, t::p().border_strong))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(mono("F3 · метрики".into(), t::p().text_muted));
                        let Some(m) = &m else {
                            ui.label(mono("не в канале".into(), t::p().text));
                            return;
                        };
                        let c = &m.conn;
                        let path_color = match c.path {
                            Path::Relay { .. } => t::p().accent,
                            Path::Unknown => t::p().danger,
                            Path::Direct { .. } => t::p().text,
                        };
                        ui.horizontal(|ui| {
                            ui.label(mono(format!("путь  {}", c.path), path_color));
                            ui.label(mono(format!("SFU {}", c.remote), t::p().text_muted));
                            if !c.turn.is_empty() {
                                ui.label(mono(format!("через {}", c.turn), t::p().text_muted));
                            }
                            if m.forced_relay {
                                ui.label(mono("FORCE RELAY".into(), t::p().danger));
                            }
                        });
                        let rtt = c.rtt_ms.map_or("—".into(), |r| format!("{r:.0} мс"));
                        let avail = c.out_kbps_avail.map_or("—".into(), |k| format!("{:.1} Мбит/с", k / 1000.0));
                        ui.label(mono(format!("RTT   {rtt}   канал ↑ {avail}"), t::p().text));
                        ui.add_space(4.0);
                        egui::Grid::new("metrics-rows").spacing(vec2(10.0, 1.0)).show(ui, |ui| {
                            for h in ["", "поток", "кбит/с", "потери", "джиттер", "кадры", ""] {
                                ui.label(mono(h.into(), t::p().text_muted));
                            }
                            ui.end_row();
                            for r in &m.rows {
                                let arrow = if r.dir == Dir::Out { "↑" } else { "↓" };
                                ui.label(mono(format!("{arrow} {}", if r.kind == "video" { "V" } else { "A" }), t::p().text_muted));
                                ui.label(mono(format!("{} {}", r.label, r.codec), t::p().text));
                                ui.label(mono(format!("{:>6.0}", r.kbps), t::p().text));
                                ui.label(mono(r.loss_pct.map_or("—".into(), |p| format!("{p:.1}%")), loss_color(r.loss_pct)));
                                ui.label(mono(r.jitter_ms.map_or("—".into(), |j| format!("{j:.0} мс")), t::p().text));
                                let frames = match (r.size, r.fps) {
                                    (Some((w, h)), Some(f)) => format!("{w}×{h} {f:.0}"),
                                    (None, Some(f)) => format!("{f:.0} к/с"),
                                    _ => String::new(),
                                };
                                ui.label(mono(frames, t::p().text));
                                ui.label(mono(r.detail.clone(), t::p().text_muted));
                                ui.end_row();
                            }
                        });
                        if m.rows.is_empty() {
                            ui.label(mono("потоков нет".into(), t::p().text_muted));
                        }
                        ui.add_space(4.0);
                        if ui.small_button("Снимок статистики в лог").clicked() {
                            dump = true;
                        }
                    });
            });
        if dump {
            self.voice.send(Cmd::DumpStats);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn ui_volume_popup(&mut self, ui: &mut Ui, vs: &VoiceState) {
        let Some(pop) = &self.popup else { return };
        let (identity, name, pos, opened) = (pop.identity.clone(), pop.name.clone(), pop.pos, pop.opened_at);
        let mut vol = self.settings.volume_of(&identity);
        let mut muted = self.settings.is_user_muted(&identity);
        let mut changed = false;
        let streaming = vs.peers.iter().any(|p| p.identity == identity && p.streaming);
        let mut svol = self.settings.stream_volume_of(&identity);
        let mut s_changed = false;
        let mut new_quality = None;
        let area = egui::Area::new(Id::new("vol-popup")).order(egui::Order::Foreground).fixed_pos(pos + vec2(8.0, 8.0)).show(ui.ctx(), |ui| {
            Frame::new()
                .fill(t::p().surface)
                .stroke(Stroke::new(1.0, t::p().border))
                .corner_radius(CornerRadius::same(kit::R_CARD))
                .inner_margin(Margin::symmetric(14, 12))
                .shadow(ui.visuals().popup_shadow)
                .show(ui, |ui| {
                    ui.set_width(250.0);
                    ui.label(RichText::new(&name).font(t::bold(14.0)));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Громкость").size(13.0).color(t::p().text_muted));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(format!("{:.0}%", vol * 100.0)).size(13.0));
                        });
                    });
                    if kit::slider(ui, &mut vol, 0.0..=2.0, 250.0).changed() {
                        changed = true;
                    }
                    ui.add_space(4.0);
                    let (icon, text) = if muted {
                        (egui_phosphor::fill::SPEAKER_SIMPLE_HIGH, "Включить для себя")
                    } else {
                        (egui_phosphor::fill::SPEAKER_SLASH, "Заглушить для себя")
                    };
                    if kit::menu_item(ui, icon, text, !muted).clicked() {
                        muted = !muted;
                        changed = true;
                    }
                    if (vol - 1.0).abs() > 0.01 && kit::menu_item(ui, egui_phosphor::fill::ARROW_COUNTER_CLOCKWISE, "Сбросить (100%)", false).clicked() {
                        vol = 1.0;
                        changed = true;
                    }
                    // Звук трансляции — отдельно от голоса (как в Discord).
                    if streaming {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}  Звук трансляции", ph::MONITOR_PLAY)).size(13.0).color(t::p().text_muted));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(format!("{:.0}%", svol * 100.0)).size(13.0));
                            });
                        });
                        if kit::slider(ui, &mut svol, 0.0..=2.0, 250.0).changed() {
                            s_changed = true;
                        }
                    }
                    // Качество — только если смотрим и стример публикует экономный слой.
                    if let Some(info) = vs.stream_layers.get(&identity).filter(|i| i.has_low) {
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("{}  Качество трансляции", ph::SLIDERS_HORIZONTAL)).size(13.0).color(t::p().text_muted));
                        ui.add_space(2.0);
                        let mut q = info.quality;
                        let items = [(Quality::Auto, Quality::Auto.label()), (Quality::High, Quality::High.label()), (Quality::Low, Quality::Low.label())];
                        if kit::segmented(ui, &mut q, &items) {
                            new_quality = Some(q);
                        }
                        if info.quality == Quality::Auto && info.low {
                            ui.add_space(2.0);
                            ui.label(RichText::new("Сейчас экономное: слабый приём").size(12.0).color(t::p().text_muted));
                        }
                    }
                });
        });
        if changed {
            // «прилипание» к 100%
            if (vol - 1.0).abs() < 0.04 {
                vol = 1.0;
            }
            self.settings.user_volume.insert(identity.clone(), vol);
            self.settings.user_muted.insert(identity.clone(), muted);
            self.settings.save();
            self.voice.send(Cmd::SetUserVolume(identity.clone(), self.settings.effective_volume(&identity)));
        }
        if s_changed {
            if (svol - 1.0).abs() < 0.04 {
                svol = 1.0;
            }
            self.settings.stream_volume.insert(identity.clone(), svol);
            self.settings.save();
            self.voice.send(Cmd::SetStreamVolume(identity.clone(), svol));
        }
        if let Some(quality) = new_quality {
            self.voice.send(Cmd::SetStreamQuality { identity: identity.clone(), quality });
        }
        let clicked_outside = ui.input(|i| i.pointer.any_pressed())
            && !area.response.contains_pointer()
            && opened.elapsed() > Duration::from_millis(150);
        if clicked_outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.popup = None;
        }
    }

    // ------------------------------------------------------------ настройки

    /// Держит проверку микрофона в актуальном состоянии, пока открыты настройки.
    fn sync_mic_test(&mut self, vs: &VoiceState) {
        let name_of = |id: &Option<String>, list: &[voice::Device]| {
            id.as_ref().and_then(|id| list.iter().find(|d| &d.id == id).map(|d| d.name.clone()))
        };
        let key = (
            name_of(&self.settings.input_device, &vs.inputs),
            name_of(&self.settings.output_device, &vs.outputs),
            self.settings.noise,
            self.settings.auto_gain,
            self.mic_playback,
        );
        // Ошибка запуска приходит из фонового потока.
        if let Some(e) = self.mic_test.as_ref().and_then(|t| t.take_error()) {
            log::warn!("проверка микрофона: {e}");
            self.mic_test_error = Some(e);
            self.mic_playback = false; // ключ изменится — проверка перезапустится без «слышать себя»
        }
        if self.mic_test_key.as_ref() == Some(&key) {
            return;
        }
        self.mic_test = None; // сначала закрыть старые потоки
        self.mic_level.store(0, Ordering::Relaxed);
        self.mic_test_error = None;
        self.mic_test = Some(mictest::start(key.0.as_deref(), key.1.as_deref(), key.2, key.3, key.4, self.mic_level.clone()));
        self.mic_test_key = Some(key);
    }

    fn stop_mic_test(&mut self) {
        self.mic_test = None;
        self.mic_test_key = None;
        self.mic_playback = false;
        self.mic_level.store(0, Ordering::Relaxed);
    }

    // ------------------------------------------------------------ трей, выход, обновление

    /// «Закрыть» (лампа, Alt+F4) — спрятать в трей; «Выйти» из трея и готовое обновление — выйти по-настоящему.
    fn handle_close(&mut self, ctx: &egui::Context) {
        self.rename_tick();
        if self.tray.as_ref().is_some_and(|t| t.quit_requested()) && !self.quitting {
            self.quit(ctx);
        }
        if self.updater.ready && !self.quitting {
            match self.updater.restart() {
                Ok(()) => self.quit(ctx),
                Err(e) => {
                    self.updater.ready = false;
                    self.updater.error = Some(e);
                }
            }
        }
        let close = ctx.input(|i| i.viewport().close_requested());
        if close && !self.quitting && self.settings.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.fullscreen {
                self.set_fullscreen(ctx, false);
            }
            desktop::hide(self.window);
            log::info!("окно спрятано в трей");
        }
    }

    /// Отладка (только debug): AMADEUS_AUTO_UPDATE=1 — сразу ставить найденное обновление (кроме копии, только что
    /// перезапущенной после обновления); AMADEUS_TRAY_QUIT_IN=<с> — «Выйти» из трея через N секунд (из другого потока).
    #[cfg(debug_assertions)]
    fn debug_update_hooks(&mut self) {
        let after_update = std::env::args().any(|a| a == desktop::ARG_AFTER_UPDATE);
        if std::env::var_os("AMADEUS_AUTO_UPDATE").is_some() && !after_update && self.updater.available.is_some() && !self.update_open {
            log::info!("отладка: ставлю обновление");
            self.update_open = true;
            self.updater.install();
        }
        static ARMED: std::sync::Once = std::sync::Once::new();
        if let (Some(secs), Some(tray)) = (std::env::var("AMADEUS_TRAY_QUIT_IN").ok().and_then(|s| s.parse::<u64>().ok()), &self.tray) {
            ARMED.call_once(|| tray.debug_quit_in(Duration::from_secs(secs), self.ctx.clone()));
        }
    }

    /// Переход amadeus.exe → murmur.exe у тех, кто получил 0.3.4+ автообновлением (файл остался со старым именем):
    /// скачать установщик и, когда не в звонке, тихо переустановиться — установщик закроет нас, уберёт amadeus.exe,
    /// перенаправит автозапуск и ярлыки и запустит murmur.exe.
    fn rename_tick(&mut self) {
        if !self.rename_checked {
            self.rename_checked = true;
            if !self.legacy && crate::legacy::needs_rename() {
                let base = self.api.base().to_string();
                self.spawn(crate::legacy::download_installer(base), Msg::Rename);
            }
        }
        if self.quitting || self.voice.snapshot().channel.is_some() {
            return;
        }
        if let Some(path) = self.rename_installer.take() {
            if desktop::spawn_with_args(&path, &crate::legacy::SILENT_ARGS) {
                log::info!("тихая переустановка как murmur.exe запущена — выхожу");
                <Self as eframe::App>::on_exit(self);
            } else {
                log::warn!("переустановка как murmur.exe: не удалось запустить {}", path.display());
            }
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        log::info!("выход");
        self.quitting = true;
        self.voice.send(Cmd::Leave);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Окно перехода: старая установка Amadeus получила Murmur автообновлением — нужен новый установщик.
    fn ui_legacy(&mut self, ui: &mut Ui) {
        let mut later = false;
        let mut start = false;
        let res = kit::modal(ui.ctx(), "legacy", "Amadeus теперь Murmur", 440.0, |ui| {
            ui.label(RichText::new("Программа сменила название. Чтобы ярлык, «Пуск» и список программ тоже стали Murmur, поставьте новый установщик — он заменит старую версию.").size(14.5).color(t::p().text));
            ui.add_space(6.0);
            ui.label(RichText::new("Вход и настройки сохранятся.").size(13.5).color(t::p().text_muted));
            if let Some(e) = &self.legacy_error {
                ui.add_space(8.0);
                kit::notice(ui, kit::Notice::Error, &format!("Не получилось: {e}"), false);
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let text = if self.legacy_busy { "Скачиваю…" } else { "Скачать и установить" };
                if kit::button_ex(ui, kit::ButtonKind::Primary, text, Some(egui_phosphor::fill::DOWNLOAD_SIMPLE), !self.legacy_busy).clicked() {
                    start = true;
                }
                if kit::button(ui, kit::ButtonKind::Ghost, "Позже", None).clicked() {
                    later = true;
                }
            });
        });
        if start {
            self.legacy_busy = true;
            self.legacy_error = None;
            let base = self.api.base().to_string();
            self.spawn(crate::legacy::download_installer(base), Msg::Legacy);
        }
        if later || (res == kit::ModalResult::Closed && !self.legacy_busy) {
            self.legacy = false; // до следующего запуска
        }
    }

    /// Окно «Доступно обновление».
    fn ui_update(&mut self, ui: &mut Ui) {
        let Some(info) = self.updater.available.clone() else {
            self.update_open = false;
            return;
        };
        let in_call = self.voice.snapshot().channel.is_some();
        let mut close = false;
        let modal = egui::Modal::new(Id::new("update"))
            .backdrop_color(t::p().scrim)
            .frame(
                Frame::new()
                    .fill(t::p().surface)
                    .stroke(Stroke::new(1.0, t::p().border))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(Margin::symmetric(30, 26))
                    .shadow(ui.visuals().window_shadow),
            )
            .show(ui.ctx(), |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new(format!("Доступна версия {}", info.version)).font(t::xbold(20.0)));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("Сейчас у вас {} · загрузка {:.0} МБ", update::CURRENT, info.size as f64 / 1_048_576.0))
                        .size(13.0)
                        .color(t::p().text_muted),
                );
                if !info.notes.trim().is_empty() {
                    ui.add_space(12.0);
                    ui.label(RichText::new(info.notes.trim()).color(t::p().text));
                }
                if in_call {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("{}  Вы в голосовом канале — после перезапуска зайдите снова.", ph::INFO))
                            .size(13.0)
                            .color(t::p().text_muted),
                    );
                }
                if self.updater.installing {
                    ui.add_space(14.0);
                    ui.add(
                        egui::ProgressBar::new(self.updater.progress())
                            .fill(t::p().accent)
                            .desired_height(8.0)
                            .corner_radius(CornerRadius::same(4)),
                    );
                    ui.label(RichText::new("Скачиваю…").size(13.0).color(t::p().text_muted));
                }
                if let Some(err) = &self.updater.error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(format!("{}  Не получилось: {err}", ph::WARNING_CIRCLE)).size(13.0).color(t::p().danger_text));
                    if ui.link("Скачать установщик в браузере").clicked() {
                        desktop::open_url(&self.updater.installer_url());
                    }
                }
                ui.add_space(20.0);
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let busy = self.updater.installing || self.updater.ready;
                    if kit::button_wide(ui, kit::ButtonKind::Primary, "Обновить и перезапустить", 230.0, !busy).clicked() {
                        self.updater.install();
                    }
                    if !busy && kit::button(ui, kit::ButtonKind::Ghost, "Позже", None).clicked() {
                        close = true;
                    }
                });
            });
        if close || (modal.should_close() && !self.updater.installing) {
            self.update_open = false;
        }
    }

    /// Раздел настроек «Приложение»: автозапуск, трей, версия и обновления.
    fn ui_settings_app(&mut self, ui: &mut Ui, w: f32) {
        let installed = desktop::is_installed();
        let mut autostart = self.settings.autostart;
        toggle_row(ui, w, |ui| {
            let r = ui.add_enabled(installed, egui::Label::new("Запускать вместе с Windows (свёрнутым в трей)"));
            if !installed {
                r.on_disabled_hover_text("Только у программы, поставленной установщиком");
            }
        }, |ui| {
            ui.add_enabled_ui(installed, |ui| {
                if kit::switch(ui, &mut autostart).changed() {
                    self.settings.autostart = autostart;
                    self.settings.save();
                    self.autostart_error = desktop::set_autostart(autostart).err();
                }
            });
        });
        if let Some(e) = &self.autostart_error {
            ui.label(RichText::new(format!("Автозапуск: {e}")).size(12.5).color(t::p().danger_text));
        }
        let mut to_tray = self.settings.close_to_tray;
        toggle_row(ui, w, |ui| {
            ui.label("«Закрыть» сворачивает в трей (выйти — через значок в трее)");
        }, |ui| {
            if kit::switch(ui, &mut to_tray).changed() {
                self.settings.close_to_tray = to_tray;
                self.settings.save();
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Версия {}", update::CURRENT)).color(t::p().text));
            let u = &self.updater;
            let status = if u.checking {
                RichText::new("проверяю…").color(t::p().text_muted)
            } else if let Some(i) = &u.available {
                RichText::new(format!("доступна {}", i.version)).color(t::p().accent_text)
            } else if let Some(e) = &u.error {
                RichText::new(e.clone()).color(t::p().danger_text)
            } else if u.checked_once {
                RichText::new("последняя").color(t::p().text_muted)
            } else {
                RichText::new("")
            };
            ui.label(status.size(13.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.updater.available.is_some() {
                    if kit::button(ui, kit::ButtonKind::Primary, "Обновить", Some(egui_phosphor::fill::ARROW_CIRCLE_UP)).clicked() {
                        self.update_open = true;
                        self.close_settings();
                    }
                } else if kit::button_ex(ui, kit::ButtonKind::Secondary, "Проверить обновления", None, !self.updater.checking).clicked() {
                    self.updater.check();
                }
            });
        });
    }

    /// Настройки → «Внешний вид»: тема, анимации, индикатор речи, пинг, плитки, кот, звуки интерфейса.
    fn ui_settings_look(&mut self, ui: &mut Ui, w: f32) {
        use crate::config::{AnimPref, PingDisplay, SpeechIndicator};
        let mut changed = false;

        let mut theme = self.settings.theme;
        toggle_row(ui, w, |ui| {
            ui.label("Тема");
        }, |ui| {
            if kit::segmented(ui, &mut theme, &[(ThemePref::Light, "Светлая"), (ThemePref::Dark, "Тёмная"), (ThemePref::System, "Системная")]) {
                self.settings.theme = theme;
                changed = true;
                t::set_theme(ui.ctx(), theme);
                crate::frame::apply_border();
            }
        });
        let mut anim = self.settings.animations_effective();
        toggle_row(ui, w, |ui| {
            ui.label("Анимации").on_hover_text("«Важные» — только волна речи и анимация подключения. «Выкл.» — всё статично.");
        }, |ui| {
            if kit::segmented(ui, &mut anim, &[(AnimPref::All, "Все"), (AnimPref::Important, "Важные"), (AnimPref::Off, "Выкл.")]) {
                self.settings.animations = Some(anim);
                changed = true;
            }
        });
        for (text, tip, val) in [
            ("Останавливать анимации в фоне", "Окно свёрнуто или не в фокусе (например, вы в игре) — украшения не рисуются", &mut self.settings.pause_anim_in_background),
            ("Компактные плитки участников", "Небольшие плитки 200×68 вместо крупных", &mut self.settings.compact_tiles),
            ("Живой кот в заголовке окна", "Моргает и шевелит ушами, когда кто-то заходит в канал", &mut self.settings.live_cat),
        ] {
            toggle_row(ui, w, |ui| {
                ui.label(text).on_hover_text(tip);
            }, |ui| {
                changed |= kit::switch(ui, val).changed();
            });
        }
        let mut speech = self.settings.speech_indicator;
        toggle_row(ui, w, |ui| {
            ui.label("Индикатор речи");
        }, |ui| {
            if kit::segmented(ui, &mut speech, &[(SpeechIndicator::Wave, "Волна"), (SpeechIndicator::Ring, "Кольцо")]) {
                self.settings.speech_indicator = speech;
                changed = true;
            }
        });
        let mut ping = self.settings.ping_display;
        toggle_row(ui, w, |ui| {
            ui.label("Пинг в заголовке");
        }, |ui| {
            if kit::segmented(ui, &mut ping, &[(PingDisplay::WaveAndMs, "Волна и мс"), (PingDisplay::MsOnly, "Только мс"), (PingDisplay::Hidden, "Скрыть")]) {
                self.settings.ping_display = ping;
                changed = true;
            }
        });
        let mut sounds = self.settings.sounds;
        let mut volume = self.settings.sounds_volume as f32;
        let mut sound_changed = false;
        let mut sample = false;
        toggle_row(ui, w, |ui| {
            ui.label("Звуки интерфейса").on_hover_text("Вход и выход из канала, начало и конец трансляции, зрители");
        }, |ui| {
            sound_changed |= kit::switch(ui, &mut sounds).changed();
            ui.add_space(10.0);
            ui.label(RichText::new(format!("{:.0}", volume)).size(13.0).color(t::p().text_muted));
            ui.add_enabled_ui(sounds, |ui| {
                let r = kit::slider(ui, &mut volume, 0.0..=100.0, 150.0);
                if r.changed() {
                    sound_changed = true;
                }
                // при отпускании — образец звука новой громкости
                sample = r.drag_stopped() || r.clicked();
            });
        });
        if sound_changed {
            self.settings.sounds = sounds;
            self.settings.sounds_volume = volume.round() as u8;
            crate::sounds::set_volume(if sounds { self.settings.sounds_volume } else { 0 });
            self.voice.send(Cmd::SetSounds(sounds));
            changed = true;
        }
        if sample {
            crate::sounds::play(crate::sounds::Sound::JoinOther);
        }
        if changed {
            self.settings.save();
            crate::anim::apply(&self.settings);
        }
    }

    fn open_settings(&mut self) {
        self.settings_open = true;
        self.voice.send(Cmd::RefreshDevices);
    }

/// Настройки → Профиль: слева поля (сохраняются сами), справа предпросмотр карточки.
    fn ui_settings_profile(&mut self, ui: &mut Ui, w: f32) {
        let Some(me) = self.me.clone() else { return };
        let busy = self.profile_busy;
        let two = w >= 600.0;
        let left_w = if two { w - profile_card::CARD_W - 32.0 } else { w };
        let now = ui.input(|i| i.time);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(left_w);
                self.ui_profile_fields(ui, left_w, &me, busy, now);
            });
            if two {
                ui.add_space(32.0);
                ui.vertical(|ui| self.ui_profile_preview(ui, &me));
            }
        });
        if !two {
            ui.add_space(16.0);
            self.ui_profile_preview(ui, &me);
        }
        self.profile_autosave(now);
    }

    fn ui_profile_preview(&mut self, ui: &mut Ui, me: &User) {
        ui.label(RichText::new("ПРЕДПРОСМОТР").font(t::bold(12.0)).color(t::p().text_muted));
        ui.add_space(6.0);
        let name = self.profile_name.trim();
        let mut u = people::public(me);
        u.display_name = (!name.is_empty()).then(|| name.to_string());
        let d = profile_card::CardData { user: &u, banner: me.banner.as_deref(), status: Some(self.profile_status.trim()), online: true };
        let about = self.profile_about.trim().to_string();
        profile_card::preview(ui, &self.people, &d, Some(&about));
    }

    fn ui_profile_fields(&mut self, ui: &mut Ui, w: f32, me: &User, busy: bool, now: f64) {
        let p = t::p();
        let caps = |ui: &mut Ui, text: &str, count: Option<(usize, usize)>| {
            ui.horizontal(|ui| {
                ui.set_width(w);
                ui.label(RichText::new(text).font(t::bold(12.0)).color(p.text_muted));
                if let Some((n, max)) = count {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("{n}/{max}")).size(12.0).color(if n >= max { p.warning } else { p.text_faint }));
                    });
                }
            });
        };
        let hint = |ui: &mut Ui, text: &str| {
            ui.label(RichText::new(text).size(12.0).color(p.text_faint));
        };
        // аватар
        caps(ui, "АВАТАР", None);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            self.people.paint_playing(ui.painter(), rect.center(), 28.0, &people::public(me), 0.0, false);
            ui.add_space(12.0);
            if kit::button_ex(ui, kit::ButtonKind::Primary, "Сменить…", Some(egui_phosphor::fill::IMAGE), !busy).clicked() {
                self.pick_profile_image(crate::crop::Shape::Circle);
            }
            if me.avatar.is_some() && kit::button_ex(ui, kit::ButtonKind::Secondary, "Убрать", None, !busy).clicked() {
                self.profile_busy = true;
                self.profile_error = None;
                let api = self.api.clone();
                self.spawn(async move { api.delete_avatar().await.map(Some).map_err(|e| e.message) }, Msg::Profile);
            }
        });
        hint(ui, "PNG, JPG, GIF или видео · итог до 4 МБ · обрежется в круг 256 px");
        ui.add_space(12.0);
        // баннер
        caps(ui, "БАННЕР", None);
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(70.0, 28.0), Sense::hover());
            profile_card::paint_banner(ui.painter(), rect, CornerRadius::same(6), &self.people, &people::public(me), me.banner.as_deref(), false);
            ui.add_space(12.0);
            if kit::button_ex(ui, kit::ButtonKind::Primary, "Сменить…", Some(egui_phosphor::fill::IMAGE), !busy).clicked() {
                self.pick_profile_image(crate::crop::Shape::Banner);
            }
            if me.banner.is_some() && kit::button_ex(ui, kit::ButtonKind::Secondary, "Убрать", None, !busy).clicked() {
                self.profile_busy = true;
                self.profile_error = None;
                let api = self.api.clone();
                self.spawn(async move { api.delete_banner().await.map(Some).map_err(|e| e.message) }, Msg::Profile);
            }
        });
        hint(ui, "5:2, лучше 960×384 · GIF или видео · итог до 6 МБ");
        ui.add_space(14.0);
        // имя
        caps(ui, "ОТОБРАЖАЕМОЕ ИМЯ", Some((self.profile_name.chars().count(), proto::limits::DISPLAY_NAME_MAX)));
        let r = ui.add(kit::text_field(&mut self.profile_name, w).hint_text(&me.name).char_limit(proto::limits::DISPLAY_NAME_MAX));
        self.profile_field_events(0, &r, now);
        hint(ui, "Пусто — показывается логин");
        ui.add_space(12.0);
        // статус
        caps(ui, "СВОЙ СТАТУС", Some((self.profile_status.chars().count(), proto::limits::STATUS_MAX)));
        let r = ui.add(kit::text_field(&mut self.profile_status, w).hint_text("чем занят — видно в карточке и у друзей").char_limit(proto::limits::STATUS_MAX));
        self.profile_field_events(1, &r, now);
        ui.add_space(12.0);
        // о себе
        caps(ui, "О СЕБЕ", Some((self.profile_about.chars().count(), proto::limits::ABOUT_MAX)));
        let r = ui.add(kit::text_area(&mut self.profile_about, w).hint_text("пару слов — видно в карточке профиля").char_limit(proto::limits::ABOUT_MAX).desired_rows(3));
        self.profile_field_events(2, &r, now);
        ui.add_space(14.0);
        let mut share = self.settings.share_activity;
        toggle_row(ui, w, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Показывать, во что я играю").strong());
                ui.label(RichText::new("Выключите — друзья увидят только «в сети»").size(12.0).color(p.text_muted));
            });
        }, |ui| {
            kit::switch(ui, &mut share);
        });
        if share != self.settings.share_activity {
            self.settings.share_activity = share;
            self.settings.save();
        }
        let mut open = me.dm_from_servers;
        let mut toggled = false;
        toggle_row(ui, w, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("ЛС от людей с общих серверов").strong());
                ui.label(RichText::new("Выключите — писать и звонить смогут только друзья").size(12.0).color(p.text_muted));
            });
        }, |ui| {
            toggled = ui.add_enabled_ui(!busy, |ui| kit::switch(ui, &mut open).changed()).inner;
        });
        if toggled {
            self.profile_busy = true;
            self.profile_error = None;
            let api = self.api.clone();
            self.spawn(async move { api.set_dm_from_servers(open).await.map(Some).map_err(|e| e.message) }, Msg::Profile);
        }
        if busy {
            let text = if self.profile_video { "Собираю GIF из видео…" } else { "Сохраняю картинку…" };
            ui.label(RichText::new(text).size(13.0).color(p.text_muted));
        } else if let Some(n) = &self.profile_note {
            ui.label(RichText::new(format!("{}  {n}", ph::INFO)).size(13.0).color(p.text_muted));
        }
        if let Some(e) = &self.profile_error {
            ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).size(13.0).color(p.danger_text));
        }
    }

    fn profile_field_events(&mut self, i: usize, r: &egui::Response, now: f64) {
        if r.changed() {
            self.profile_saves[i].edited(now);
        }
        if r.lost_focus() {
            self.profile_saves[i].focus_lost();
        }
    }

    /// Отправить поля, которым пора; пока что-то ждёт паузы — перерисовать позже.
    fn profile_autosave(&mut self, now: f64) {
        for i in 0..3 {
            if self.profile_saves[i].due(now) {
                self.profile_send(i);
            }
        }
        if self.profile_saves.iter().any(|s| s.pending()) {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Отправить поле профиля сейчас.
    fn profile_send(&mut self, i: usize) {
        self.profile_saves[i].sent();
        self.profile_save_state = crate::autosave::SaveState::Saving;
        let api = self.api.clone();
        let (name, status, about) = (self.profile_name.trim().to_string(), self.profile_status.trim().to_string(), self.profile_about.trim().to_string());
        self.spawn(
            async move {
                let r = match i {
                    0 => api.update_me(&name).await,
                    1 => api.update_profile(None, Some(status)).await,
                    _ => api.update_profile(Some(about), None).await,
                };
                r.map_err(|e| e.message)
            },
            move |r| Msg::ProfileField(i, r),
        );
    }

    /// Закрытие настроек: дослать несохранённое.
    fn profile_flush(&mut self) {
        for i in 0..3 {
            if self.profile_saves[i].flush() {
                self.profile_saves[i].retry();
                self.profile_send(i);
            }
        }
    }

    /// Индикатор сохранения справа от заголовка «Профиль». «Повторить» — отправить несохранённое снова.
    fn ui_profile_save_indicator(&mut self, ui: &mut Ui, row: Rect) {
        use crate::autosave::SaveState;
        let p = t::p();
        let (text, color) = match self.profile_save_state {
            SaveState::Idle => return,
            SaveState::Saving => ("Сохраняем…".to_string(), p.text_muted),
            SaveState::Saved => (format!("{}  Сохранено", ph::CHECK), p.text_muted),
            SaveState::Failed => (format!("{}  Не удалось сохранить — повторить", ph::WARNING_CIRCLE), p.danger_text),
        };
        let galley = ui.painter().layout_no_wrap(text, t::font(12.5), color);
        let r = Rect::from_min_size(pos2(row.right() - galley.size().x, row.center().y - galley.size().y / 2.0), galley.size());
        ui.painter().galley(r.min, galley, color);
        if self.profile_save_state == SaveState::Failed {
            let resp = ui.interact(r, Id::new("profile-retry"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                for s in &mut self.profile_saves {
                    s.retry();
                }
                self.profile_save_state = SaveState::Saving;
            }
        }
    }

    /// Выбрать картинку → окно обрезки → в фоне собрать (GIF — все кадры) → `PUT /me/avatar` / `PUT /me/banner`.
    fn pick_profile_image(&mut self, shape: crate::crop::Shape) {
        self.profile_busy = true;
        self.profile_error = None;
        self.profile_note = None;
        let title = if shape == crate::crop::Shape::Banner { "Баннер" } else { "Аватар" };
        self.spawn(
            async move { tokio::task::spawn_blocking(move || servers_ui::pick_source(title)).await.map_err(|e| e.to_string())? },
            move |r| Msg::AvatarPicked(r, shape),
        );
    }

    fn ui_avatar_crop(&mut self, ctx: &egui::Context) {
        let Some(c) = self.avatar_crop.as_mut() else { return };
        match c.ui(ctx) {
            crate::crop::CropResult::Open => {}
            crate::crop::CropResult::Cancel => {
                self.avatar_crop = None;
                self.profile_busy = false;
            }
            crate::crop::CropResult::Done(_) => {
                self.avatar_crop = None;
                self.profile_busy = false;
            }
            crate::crop::CropResult::Render(job) => {
                self.avatar_crop = None;
                self.profile_video = job.src.video.is_some();
                let api = self.api.clone();
                self.spawn(
                    async move {
                        let shape = job.shape;
                        let r = match tokio::task::spawn_blocking(move || crate::crop::render(&job)).await.map_err(|e| e.to_string()).and_then(|r| r) {
                            Ok(r) => r,
                            Err(e) => return (Err(e), None),
                        };
                        let res = if shape == crate::crop::Shape::Banner { api.put_banner(r.bytes).await } else { api.put_avatar(r.bytes).await };
                        (res.map(Some).map_err(|e| e.message), r.note)
                    },
                    |(res, note)| Msg::ProfileImage(res, note),
                );
            }
        }
    }

    /// Настройки → «Голос и видео».
    fn ui_settings_voice(&mut self, ui: &mut Ui, w: f32, vs: &VoiceState) {
        if let Some(err) = &vs.audio_error {
            ui.label(RichText::new(format!("{}  {err}", ph::WARNING_CIRCLE)).size(13.0).color(t::p().danger_text));
        }

        label(ui, "Микрофон");
        let mut input = self.settings.input_device.clone();
        if device_combo(ui, "in", &mut input, &vs.inputs, w) {
            self.settings.input_device = input.clone();
            self.settings.save();
            self.voice.send(Cmd::SetInputDevice(input));
        }
        ui.add_space(8.0);
        label(ui, "Вывод звука");
        let mut output = self.settings.output_device.clone();
        if device_combo(ui, "out", &mut output, &vs.outputs, w) {
            self.settings.output_device = output.clone();
            self.settings.save();
            self.voice.send(Cmd::SetOutputDevice(output));
        }
        ui.add_space(6.0);

        // громкости 0–200 %
        let mut gate_changed = false;
        let mut mic_vol = self.settings.mic_volume;
        let mut out_vol = self.settings.output_volume;
        for (text, hint, val) in [
            ("Громкость микрофона", "Насколько громко вас слышат. Выше 100 % — усиление тихого микрофона", &mut mic_vol),
            ("Громкость звука", "Общая громкость собеседников и трансляций (звуки интерфейса — в «Внешнем виде»)", &mut out_vol),
        ] {
            toggle_row(ui, w, |ui| {
                ui.label(text).on_hover_text(hint);
            }, |ui| {
                ui.label(RichText::new(format!("{:.0} %", *val * 100.0)).size(13.0).color(t::p().text_muted));
                let r = kit::slider(ui, val, 0.0..=2.0, 220.0);
                if r.double_clicked() {
                    *val = 1.0; // двойной клик — 100 %
                }
                // прилипание к 100 %
                if (*val - 1.0).abs() < 0.03 {
                    *val = 1.0;
                }
            });
        }
        if mic_vol != self.settings.mic_volume {
            self.settings.mic_volume = mic_vol;
            gate_changed = true;
        }
        if out_vol != self.settings.output_volume {
            self.settings.output_volume = out_vol;
            self.settings.save();
            self.voice.send(Cmd::SetOutputVolume(out_vol));
        }

        // проверка микрофона
        group_label(ui, "ПРОВЕРКА МИКРОФОНА");
        let level = if self.mic_test.as_ref().is_some_and(|t| t.running()) {
            Some(f32::from_bits(self.mic_level.load(Ordering::Relaxed)))
        } else if vs.peers.iter().any(|p| p.is_local) {
            Some(crate::denoise::local_voice().0) // в звонке: уровень до порога
        } else {
            None
        };
        let voice_mode = !self.settings.ptt_enabled;
        let thr = voice_mode.then(|| (crate::gate::threshold(), !self.settings.vad_auto));
        if let Some(t) = level_meter(ui, level, w, thr) {
            self.settings.vad_threshold = t;
            gate_changed = true;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let clicked = if self.mic_playback {
                kit::button(ui, kit::ButtonKind::Primary, "Остановить проверку", Some(egui_phosphor::fill::STOP))
            } else {
                kit::button(ui, kit::ButtonKind::Secondary, "Послушать себя", Some(egui_phosphor::fill::HEADPHONES))
            }
            .clicked();
            if clicked {
                self.mic_playback = !self.mic_playback;
            }
            let hint = match &self.mic_test_error {
                Some(e) => RichText::new(format!("Проверка недоступна: {e}")).color(t::p().danger_text),
                None if self.mic_playback => RichText::new("Так вас слышат друзья. Наденьте наушники — на колонках будет заводка.").color(t::p().text_muted),
                None if voice_mode && self.settings.vad_auto => RichText::new("Белая метка — порог: тише неё микрофон молчит. Сейчас её ставит авточувствительность.").color(t::p().text_muted),
                None if voice_mode => RichText::new("Тяните белую метку: тише неё микрофон молчит.").color(t::p().text_muted),
                None => RichText::new("Голос с шумоподавлением, как в звонке. Лучше в наушниках.").color(t::p().text_muted),
            };
            ui.label(hint.size(12.5));
        });
        ui.add_space(6.0);

        // режим ввода
        group_label(ui, "РЕЖИМ ВВОДА");
        let mut ptt_mode = self.settings.ptt_enabled;
        let show_key = ptt_mode;
        toggle_row(ui, w, |ui| {
            kit::segmented(ui, &mut ptt_mode, &[(false, "Голосом"), (true, "Рацией")]);
        }, |ui| {
            if !show_key {
                return;
            }
            if self.ptt.capturing() {
                if kit::button(ui, kit::ButtonKind::Ghost, "Отмена", None).clicked() {
                    self.ptt.cancel_capture();
                }
                ui.label(RichText::new("нажмите клавишу…").color(t::p().accent_text).size(13.0));
            } else {
                let r = kit::keycap(ui, &ptt::key_name(self.settings.ptt_key)).interact(Sense::click());
                if r.on_hover_text("Нажмите, чтобы назначить другую клавишу (можно кнопку мыши)").clicked() {
                    self.ptt.begin_capture();
                }
                ui.label(RichText::new("Клавиша рации").size(13.0).color(t::p().text_muted));
            }
        });
        if ptt_mode != self.settings.ptt_enabled {
            self.settings.ptt_enabled = ptt_mode;
            self.ptt.set_enabled(ptt_mode);
            self.voice.send(Cmd::SetPttEnabled(ptt_mode));
            gate_changed = true;
        }
        if !self.settings.ptt_enabled {
            toggle_row(ui, w, |ui| {
                ui.label("Автоматическая чувствительность")
                    .on_hover_text("Порог сам подстраивается под шум в комнате. Выключите, чтобы поставить метку вручную");
            }, |ui| {
                let mut auto = self.settings.vad_auto;
                if kit::switch(ui, &mut auto).changed() {
                    if !auto {
                        // ручной порог начинается с того места, где была метка
                        self.settings.vad_threshold = crate::gate::threshold();
                    }
                    self.settings.vad_auto = auto;
                    gate_changed = true;
                }
            });
        }
        if gate_changed {
            self.settings.save();
            push_gate(&self.settings);
        }

        // обработка
        group_label(ui, "ОБРАБОТКА ЗВУКА");
        let mut proc_changed = false;
        toggle_row(ui, w, |ui| {
            ui.label("Шумоподавление").on_hover_text(
                "Нейросеть (DeepFilterNet) убирает шум и шипение (клавиатура, вентилятор, улица) и не режет высокие частоты.\n\
                 Лёгкая (RNNoise) — дешевле для слабых ПК, но шипение почти не убирает.\n\
                 Обычное — шумодав WebRTC: голос глуше. Выключено — для хорошего микрофона в тихой комнате.",
            );
        }, |ui| {
            let mut mode = self.settings.noise;
            egui::ComboBox::from_id_salt("noise-mode").selected_text(mode.label()).width(190.0).show_ui(ui, |ui| {
                for m in NoiseMode::ALL {
                    ui.selectable_value(&mut mode, m, m.label());
                }
            });
            if mode != self.settings.noise {
                self.settings.noise = mode;
                proc_changed = true;
            }
        });
        for (text, val) in [
            ("Эхоподавление (выключите в наушниках — звук чище)", &mut self.settings.echo_cancellation),
            ("Автоматическая громкость микрофона", &mut self.settings.auto_gain),
        ] {
            toggle_row(ui, w, |ui| {
                ui.label(text);
            }, |ui| {
                if kit::switch(ui, val).changed() {
                    proc_changed = true;
                }
            });
        }
        if proc_changed {
            self.settings.save();
            self.voice.send(Cmd::SetProcessing {
                aec: self.settings.echo_cancellation,
                noise: self.settings.noise,
                agc: self.settings.auto_gain,
            });
        }
    }

    /// Настройки → «Стрим экрана» (пока — то, что уже есть: качество и звук по умолчанию).
    fn ui_settings_stream(&mut self, ui: &mut Ui, w: f32, vs: &VoiceState) {
        let mut preset = self.settings.stream_preset;
        toggle_row(ui, w, |ui| {
            ui.label("Качество по умолчанию").on_hover_text("Разрешение и частота кадров; можно сменить при запуске трансляции");
        }, |ui| {
            egui::ComboBox::from_id_salt("set-preset").selected_text(preset.label()).width(200.0).show_ui(ui, |ui| {
                for pr in crate::stream::Preset::ALL {
                    ui.selectable_value(&mut preset, pr, pr.label());
                }
            });
        });
        if preset != self.settings.stream_preset {
            self.settings.stream_preset = preset;
            self.settings.save();
        }
        let mut audio = self.settings.stream_audio;
        toggle_row(ui, w, |ui| {
            ui.label("Звук в трансляции").on_hover_text("Звук показываемого окна или всего компьютера");
        }, |ui| {
            if kit::switch(ui, &mut audio).changed() {
                self.settings.stream_audio = audio;
                self.settings.save();
            }
        });
        // Кодировщик и битрейт — на следующий запуск трансляции
        let mut enc = self.settings.stream_encoder;
        let live = vs.stream.as_ref().map(|s| s.backend.clone());
        toggle_row(ui, w, |ui| {
            ui.label("Кодировщик").on_hover_text("Авто — видеокарта, а если не вышло — процессор. «Процессор» выручает, когда видеокарта занята игрой или глючит; «Видеокарта» не нагружает процессор.");
        }, |ui| {
            kit::segmented(ui, &mut enc, &crate::stream::EncoderPref::ALL);
        });
        if let Some(b) = &live {
            ui.label(RichText::new(format!("Сейчас кодирует: {b}")).size(12.5).color(t::p().text_muted));
        }
        let mut manual = self.settings.stream_bitrate_mbps.is_some();
        let mut mbps = self.settings.stream_bitrate_mbps.unwrap_or(8.0);
        toggle_row(ui, w, |ui| {
            ui.label("Битрейт").on_hover_text("Потолок скорости потока. «Авто» — по качеству (3–12 Мбит/с); реальную скорость всё равно подстраивает под ваш канал. Вручную — если канал стабильный и хочется чётче, или наоборот слабый.");
        }, |ui| {
            kit::segmented(ui, &mut manual, &[(false, "Авто"), (true, "Вручную")]);
        });
        if manual {
            ui.horizontal(|ui| {
                kit::slider(ui, &mut mbps, crate::stream::BITRATE_MIN_MBPS..=crate::stream::BITRATE_MAX_MBPS, 260.0);
                ui.label(RichText::new(format!("{:.0} Мбит/с", mbps.round())).size(13.0).color(t::p().text_muted));
            });
        }
        let new_mbps = manual.then(|| mbps.round().max(crate::stream::BITRATE_MIN_MBPS));
        if enc != self.settings.stream_encoder || new_mbps != self.settings.stream_bitrate_mbps {
            self.settings.stream_encoder = enc;
            self.settings.stream_bitrate_mbps = new_mbps;
            self.settings.save();
            crate::stream::set_prefs(enc, new_mbps);
        }
        if live.is_some() {
            ui.label(RichText::new("Применится при следующем запуске трансляции").size(12.5).color(t::p().text_muted));
        }
        let (mut pip_auto, mut topmost) = (self.settings.pip_auto, self.settings.pip_topmost);
        toggle_row(ui, w, |ui| {
            ui.label("Окно в окне для трансляций").on_hover_text("Смотрите стрим в маленьком окне, пока заняты другим: внутри Murmur, а когда Murmur свёрнут или вы в другой программе — отдельным окном");
        }, |ui| {
            if kit::switch(ui, &mut pip_auto).changed() {
                self.settings.pip_auto = pip_auto;
                self.settings.save();
            }
        });
        toggle_row(ui, w, |ui| {
            ui.label("Мини-окно поверх других окон").on_hover_text("Отдельное окно с трансляцией остаётся над играми и другими программами (применяется при следующем показе)");
        }, |ui| {
            if kit::switch(ui, &mut topmost).changed() {
                self.settings.pip_topmost = topmost;
                self.settings.save();
            }
        });
    }

    /// Настройки → «Горячие клавиши» (пока одна — рация).
    fn ui_settings_hotkeys(&mut self, ui: &mut Ui, w: f32) {
        toggle_row(ui, w, |ui| {
            ui.label("Рация — держать, чтобы говорить").on_hover_text("Работает, даже когда окно свёрнуто или вы в игре");
        }, |ui| {
            if self.ptt.capturing() {
                if kit::button(ui, kit::ButtonKind::Ghost, "Отмена", None).clicked() {
                    self.ptt.cancel_capture();
                }
                ui.label(RichText::new("нажмите клавишу…").color(t::p().accent_text).size(13.0));
            } else {
                if kit::button(ui, kit::ButtonKind::Secondary, "Изменить", None).clicked() {
                    self.ptt.begin_capture();
                }
                ui.add_space(8.0);
                kit::keycap(ui, &ptt::key_name(self.settings.ptt_key));
            }
        });
    }

    /// Настройки → «Аккаунт» (пока — логин и сервер).
    fn ui_settings_account(&mut self, ui: &mut Ui, w: f32) {
        let me = self.me.clone();
        let host = self.settings.server_url.trim_start_matches("https://").trim_start_matches("http://").to_string();
        let login = me.as_ref().map(|u| format!("{}{}", u.name, if u.is_admin { " · администратор" } else { "" })).unwrap_or_default();
        for (k, v) in [("Логин", login), ("Сервер", host)] {
            toggle_row(ui, w, |ui| {
                ui.label(k);
            }, |ui| {
                ui.label(RichText::new(v).color(t::p().text_muted));
            });
        }
    }

    fn ui_settings(&mut self, ui: &mut Ui, vs: &VoiceState) {
        self.sync_mic_test(vs);
        // живой индикатор уровня — только пока идёт проверка микрофона
        if self.mic_test.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        let mut close = ui.input(|i| i.key_pressed(egui::Key::Escape)) && !self.ptt.capturing();
        let full = ui.ctx().content_rect();
        let top = if self.fullscreen { full.top() } else { full.top() + crate::frame::HEIGHT };
        let r = Rect::from_min_max(pos2(full.left(), top), full.max);
        let p = t::p();
        egui::Area::new(Id::new("settings-screen")).order(egui::Order::Foreground).fixed_pos(r.min).show(ui.ctx(), |ui| {
            ui.set_min_size(r.size());
            ui.set_max_size(r.size());
            let painter = ui.painter().clone();
            painter.rect_filled(r, CornerRadius::ZERO, p.bg);
            // меню слева; на широком окне меню + раздел + крестик стоят группой по центру, а не прижаты к краям
            const MENU_W: f32 = 240.0;
            const BODY_W: f32 = 680.0;
            let group_w = MENU_W + 1.0 + 40.0 + BODY_W + 104.0;
            let off = ((r.width() - group_w) / 2.0).max(0.0);
            let menu = Rect::from_min_max(pos2(r.left() + off, r.top()), pos2(r.left() + off + MENU_W, r.bottom()));
            painter.rect_filled(Rect::from_min_max(r.min, pos2(menu.right(), r.bottom())), CornerRadius::ZERO, p.surface);
            painter.vline(menu.right(), menu.y_range(), Stroke::new(1.0, p.border));
            ui.scope_builder(egui::UiBuilder::new().max_rect(menu.shrink2(vec2(12.0, 16.0))), |ui| {
                if self.ui_settings_menu(ui) {
                    close = true;
                    self.logout();
                }
            });
            // раздел справа
            let content = Rect::from_min_max(pos2(menu.right() + 1.0, r.top()), r.max);
            let body_right = (content.right() - 96.0).min(content.left() + 40.0 + BODY_W);
            let x = Rect::from_center_size(pos2(body_right + 52.0, content.top() + 44.0), vec2(36.0, 36.0));
            let xr = ui.interact(x, Id::new("settings-close"), Sense::click());
            let lit = ui.ctx().animate_bool_with_time(xr.id, xr.hovered(), kit::HOVER_S);
            painter.circle(x.center(), 18.0, Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, lit), Stroke::new(1.0, p.border_strong), );
            painter.text(x.center(), Align2::CENTER_CENTER, ph::X, t::font(18.0), p.text_muted.lerp_to_gamma(p.text, lit));
            if xr.on_hover_text("Закрыть (Esc)").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                close = true;
            }
            let body = Rect::from_min_max(pos2(content.left() + 40.0, content.top() + 28.0), pos2(body_right, content.bottom()));
            ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
                let tab = self.set_tab;
                let (_, _, title, sub, _) = SET_TABS.iter().find(|t| t.0 == tab).copied().unwrap_or(SET_TABS[0]);
                let title_r = ui.label(RichText::new(title).font(t::xbold(24.0)).color(p.text)).rect;
                if tab == SetTab::Profile {
                    let w = ui.available_width().min(680.0);
                    let left_w = if w >= 600.0 { w - profile_card::CARD_W - 32.0 } else { w };
                    self.ui_profile_save_indicator(ui, Rect::from_min_size(title_r.min, vec2(left_w, title_r.height())));
                }
                ui.add_space(2.0);
                ui.label(RichText::new(sub).size(14.0).color(p.text_muted));
                ui.add_space(18.0);
                #[allow(unused_mut)]
                let mut area = egui::ScrollArea::vertical().id_salt(("settings-scroll", tab as u8)).auto_shrink([false, false]);
                // Отладка: AMADEUS_OPEN_SETTINGS=<px> — открыть настройки прокрученными (скриншоты).
                #[cfg(debug_assertions)]
                if let Some(px) = std::env::var("AMADEUS_OPEN_SETTINGS").ok().and_then(|v| v.parse::<f32>().ok()).filter(|px| *px > 1.0) {
                    area = area.vertical_scroll_offset(px);
                }
                area.show(ui, |ui| {
                    let w = ui.available_width().min(680.0);
                    ui.set_max_width(w);
                    match tab {
                        SetTab::Profile => self.ui_settings_profile(ui, w),
                        SetTab::Account => self.ui_settings_account(ui, w),
                        SetTab::Voice => self.ui_settings_voice(ui, w, vs),
                        SetTab::Stream => self.ui_settings_stream(ui, w, vs),
                        SetTab::Hotkeys => self.ui_settings_hotkeys(ui, w),
                        SetTab::Look => self.ui_settings_look(ui, w),
                        SetTab::Notify => self.ui_settings_app(ui, w),
                    }
                    ui.add_space(24.0);
                });
            });
        });
        if close {
            self.close_settings();
        }
    }

    /// Закрыть настройки (Esc, крестик, «Обновить», выход): дослать несохранённое в профиле.
    fn close_settings(&mut self) {
        self.profile_flush();
        self.ptt.cancel_capture();
        self.settings_open = false;
        self.set_query.clear();
        self.stop_mic_test();
    }

    /// Левое меню настроек: кто я, поиск, разделы, «Выйти» и версия. true — нажали «Выйти».
    fn ui_settings_menu(&mut self, ui: &mut Ui) -> bool {
        let p = t::p();
        let w = ui.available_width();
        let bottom = ui.max_rect().bottom();
        // кто я
        if let Some(me) = self.me.as_ref().map(people::public) {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                self.people.paint(ui.painter(), rect.center(), 20.0, &me, 0.0, false);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add_space(2.0);
                    ui.label(RichText::new(me.shown_name()).font(t::bold(15.0)).color(p.text));
                    let host = self.settings.server_url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("api.").to_string();
                    ui.label(RichText::new(host).size(12.5).color(p.text_muted));
                });
            });
            ui.add_space(14.0);
        }
        ui.add(kit::text_field(&mut self.set_query, w).hint_text(format!("{}  Поиск настроек", ph::MAGNIFYING_GLASS)).margin(vec2(12.0, 8.0)).font(t::font(14.0)));
        ui.add_space(12.0);
        let q = self.set_query.trim().to_lowercase();
        let visible: Vec<SetTab> = SET_TABS.iter().filter(|t| q.is_empty() || t.2.to_lowercase().contains(&q) || t.4.contains(&q)).map(|t| t.0).collect();
        if !visible.is_empty() && !visible.contains(&self.set_tab) {
            self.set_tab = visible[0];
        }
        let mut group = "";
        for &(tab, g, title, _, _) in SET_TABS.iter() {
            if !visible.contains(&tab) {
                continue;
            }
            if g != group {
                group = g;
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    kit::group_label(ui, g);
                });
                ui.add_space(4.0);
            }
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
            let on = self.set_tab == tab;
            let lit = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), kit::HOVER_S);
            let bg = if on { p.row_active } else { Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, lit) };
            ui.painter().rect_filled(rect, CornerRadius::same(kit::R_ROW), bg);
            let ic = if on { p.accent } else { p.text_muted };
            ui.painter().text(pos2(rect.left() + 20.0, rect.center().y), Align2::CENTER_CENTER, tab.icon(), t::font(17.0), ic);
            ui.painter().text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, title, t::semibold(14.0), if on { p.text } else { p.text_muted.lerp_to_gamma(p.text, lit) });
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.set_tab = tab;
            }
        }
        if visible.is_empty() {
            ui.label(RichText::new("Ничего не нашли").size(13.0).color(p.text_muted));
        }
        // внизу: «Выйти» и версия
        let out = Rect::from_min_size(pos2(ui.max_rect().left(), bottom - 62.0), vec2(w, 34.0));
        let resp = ui.interact(out, Id::new("settings-logout"), Sense::click());
        let lit = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), kit::HOVER_S);
        ui.painter().rect_filled(out, CornerRadius::same(kit::R_ROW), Color32::TRANSPARENT.lerp_to_gamma(p.danger_soft, lit));
        ui.painter().text(pos2(out.left() + 20.0, out.center().y), Align2::CENTER_CENTER, ph::SIGN_OUT, t::font(17.0), p.danger_text);
        ui.painter().text(pos2(out.left() + 40.0, out.center().y), Align2::LEFT_CENTER, "Выйти", t::semibold(14.0), p.danger_text);
        ui.painter().text(pos2(out.left() + 8.0, bottom - 10.0), Align2::LEFT_CENTER, format!("Murmur {} · AGPL-3.0", update::CURRENT), t::font(12.0), p.text_faint);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }
}

/// Метка последнего кадра UI (мс от старта) — по ней сторожок замечает зависание.
static UI_BEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static UI_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Сторожок: если UI-поток не отрисовывал кадр больше 3 с при видимом окне — пишет в лог (раз в 5 с).
/// Сам тоже просит кадр раз в секунду (пока окно видимо): простой окна (egui не рисует без событий) не должен выглядеть как зависание.
pub fn start_watchdog(ctx: egui::Context) {
    use std::sync::atomic::Ordering::Relaxed;
    now_ms();
    let _ = std::thread::Builder::new().name("amadeus-watchdog".into()).spawn(move || {
        let mut warned = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            // скрытое/свёрнутое окно не будим: кадров нет, и зависанием это не считается
            if UI_VISIBLE.load(Relaxed) {
                ctx.request_repaint();
            }
            let beat = UI_BEAT.load(Relaxed);
            let stalled = now_ms().saturating_sub(beat);
            if UI_VISIBLE.load(Relaxed) && beat > 0 && stalled > 3000 {
                if now_ms() - warned >= 5000 {
                    warned = now_ms();
                    log::error!("UI не отвечает уже {} с (последний кадр {} мс от старта)", stalled / 1000, beat);
                }
            } else if warned != 0 && stalled < 1000 {
                log::warn!("UI снова отвечает");
                warned = 0;
            }
        }
    });
}

impl eframe::App for App {
    /// Вызывается и когда окно спрятано/свёрнуто (тогда `ui` не вызывается): трей, выход, обновления.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        UI_BEAT.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        UI_VISIBLE.store(desktop::is_visible(self.window), std::sync::atomic::Ordering::Relaxed);
        self.updater.poll();
        #[cfg(debug_assertions)]
        self.debug_update_hooks();
        self.handle_close(ctx);
        if let Some(until) = self.hide_until {
            if desktop::is_visible(self.window) {
                desktop::hide(self.window);
                log::info!("запуск из автозагрузки — окно в трее");
            }
            if Instant::now() < until {
                ctx.request_repaint_after(Duration::from_millis(50));
            } else {
                self.hide_until = None;
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Отладка: AMADEUS_GALLERY=1 — только витрина набора деталей.
        #[cfg(debug_assertions)]
        if std::env::var_os("AMADEUS_GALLERY").is_some() {
            if !std::ptr::eq(t::resolve(self.settings.theme, ui.ctx().system_theme()), t::p()) {
                t::set_theme(ui.ctx(), self.settings.theme);
            }
            static SIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SIZED.swap(true, Ordering::Relaxed) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(1240.0, 960.0)));
            }
            crate::frame::title_bar(ui, None, true);
            let mut theme = self.settings.theme;
            crate::gallery::ui(ui, &mut self.gallery, &mut theme);
            self.settings.theme = theme;
            crate::frame::resize_edges(ui.ctx());
            return;
        }
        // Тема: «Как в Windows» следит за системой на лету.
        if !std::ptr::eq(t::resolve(self.settings.theme, ui.ctx().system_theme()), t::p()) {
            t::set_theme(ui.ctx(), self.settings.theme);
            if !self.fullscreen {
                crate::frame::apply_border();
            }
        }
        self.handle_msgs();
        self.chat.poll();
        if let Some(r) = self.chat.take_recent_emoji() {
            self.settings.recent_emoji = r;
            self.settings.save();
        }
        for u in self.chat.take_users() {
            if let Some(me) = self.me.as_mut().filter(|m| m.id == u.id) {
                me.display_name = u.display_name.clone();
                me.avatar = u.avatar.clone();
            }
            self.people.learn(u);
        }
        for e in self.chat.take_events() {
            self.friends.on_event(&e);
            self.dms.on_event(&e);
            match &e {
                proto::ServerEvent::Summon { from, channel_id, .. } => {
                    self.people.learn(from.clone());
                    self.summon = Some((from.clone(), *channel_id, Instant::now()));
                    crate::sounds::play(crate::sounds::Sound::DirectMessage);
                    continue;
                }
                proto::ServerEvent::Hello => self.game_sent = None, // поток (пере)открылся — сервер забыл игру
                proto::ServerEvent::UserUpdated { user } => self.members.forget(user.id),
                proto::ServerEvent::Activity { user_id, .. } => self.members.forget(*user_id),
                _ => {}
            }
            match &e {
                proto::ServerEvent::MessageCreated { message } => {
                    self.on_message(message);
                    continue;
                }
                proto::ServerEvent::ReadState { channel_id, last_read_id } => {
                    self.unread.on_read(*channel_id, *last_read_id);
                    continue;
                }
                _ => {}
            }
            if matches!(e, proto::ServerEvent::Hello | proto::ServerEvent::Resync) {
                // поток (пере)открылся — дочитать, что могли пропустить
                if self.screen == Screen::Main && !self.demo {
                    self.fetch_channels();
                    // сервер забывает игру, когда человек «не в сети»: после переподключения потока — отправить снова
                    self.game_sent = None;
                }
                continue;
            }
            self.model.apply(&e);
            // вступил на сервер не из этого окна (другое окно, повторный вход) — дочитать каналы и людей
            if let proto::ServerEvent::ServerUpserted { server } = &e
                && self.model.needs_load(server.id)
            {
                self.servers.reload_one(server.clone());
            }
        }
        if self.friends.poll(&mut self.people) {
            crate::sounds::play(crate::sounds::Sound::ViewerJoin);
        }
        if let Some(uid) = self.friends.take_message() {
            self.dms.open_with(uid);
        }
        if self.friends.take_toggle_offline() {
            self.settings.friends_offline_open = !self.settings.friends_offline_open;
            self.settings.save();
        }
        self.members.poll();
        self.sync_activity();
        if let Some(id) = self.dms.poll(&mut self.unread, &mut self.people) {
            self.view = View::Dm(id);
        }
        if let Some(e) = self.dms.error.take() {
            self.friends.show_error(e);
        }
        if std::mem::take(&mut self.friends.open) {
            self.view = View::Friends;
        }
        if self.screen == Screen::Main {
            self.sync_read(ui.input(|i| i.focused));
        }
        crate::frame::set_cat_asleep(self.friends.all_offline());
        self.channel_list.poll(&mut self.model);
        self.server_settings.poll();
        let out = self.servers.poll(&mut self.model, &mut self.people);
        if out.unauthorized {
            self.logout();
        }
        if out.first_load {
            self.on_servers_loaded();
        }
        if let Some(id) = out.select {
            self.select_server(id);
        }
        self.sync_channels();
        self.people.poll();
        if self.screen == Screen::Main && self.chat.unauthorized() {
            self.logout();
        }
        // В полноэкранном просмотре трансляции рамки нет.
        if !self.fullscreen {
            let badge = self.updater.available.as_ref().map(|i| format!("Обновление {}", i.version));
            // на экране входа логотип один — большой слева, в заголовке только кнопки окна
            if crate::frame::title_bar(ui, badge.as_deref(), self.screen != Screen::Login) {
                self.update_open = true;
            }
        }
        match self.screen {
            Screen::Boot => self.ui_boot(ui),
            Screen::Login => self.ui_login(ui),
            Screen::Main => self.ui_main(ui),
        }
        if self.update_open {
            self.ui_update(ui);
        }
        if self.legacy {
            self.ui_legacy(ui);
        }
        self.ui_metrics(ui);
        if !self.fullscreen {
            crate::frame::resize_edges(ui.ctx());
        }
    }

    /// Настоящий выход: выйти из канала (до 0,8 с) и завершить процесс сразу — иначе остановка tokio ждёт зависшие
    /// вызовы звукового движка, и программа «не закрывается».
    fn on_exit(&mut self) {
        log::info!("завершение: выход из канала и остановка");
        self.voice.send(Cmd::Leave);
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(800) && !matches!(self.voice.snapshot().conn, Conn::Idle) {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.tray = None;
        log::logger().flush();
        desktop::exit_now();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Полный экран — чёрный фон под видео (иначе непокрытый край окна виден полосой цвета интерфейса).
        if self.fullscreen {
            return Color32::BLACK.to_normalized_gamma_f32();
        }
        t::p().bg.to_normalized_gamma_f32()
    }
}

/// Отладка вёрстки без сервера голоса: AMADEUS_DEMO=1 (только debug-сборка).
#[cfg(debug_assertions)]
fn demo_state(time: f64) -> VoiceState {
    // AMADEUS_DEMO_TALK=0|1|2 — сколько человек говорит (по умолчанию 1: mayuri), для замеров CPU.
    let talk: usize = std::env::var("AMADEUS_DEMO_TALK").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let peer = |id: &str, local: bool, speaking: bool, muted: bool| Peer {
        identity: id.into(),
        name: id.into(),
        is_local: local,
        speaking,
        level: if speaking { (0.15 + 0.15 * (time * 7.0).sin().abs()) as f32 } else { 0.0 },
        mic_muted: muted,
        streaming: id == "kurisu" || (id == "daru" && std::env::var_os("AMADEUS_DEMO_STREAMS").is_some()),
    };
    VoiceState {
        conn: if std::env::var_os("AMADEUS_DEMO_RECONNECT").is_some() { Conn::Reconnecting } else { Conn::Connected },
        channel: Some(Channel {
            id: 1,
            name: "General".into(),
            kind: ChannelKind::Voice,
            server_id: 1,
            category_id: None,
            position: 0,
            user_limit: None,
        }),
        peers: vec![
            peer("tester", true, false, false),
            peer("mayuri", false, talk >= 1, false),
            peer("kurisu", false, talk >= 2, false),
            peer("daru", false, false, true),
        ],
        rtt: Some(0.037),
        transmitting: true,
        ..Default::default()
    }
}

// ------------------------------------------------------------ мелкие виджеты

/// Элемент сцены комнаты.
#[derive(Clone, Copy)]
enum Item<'a> {
    Person(&'a Peer),
    Stream(&'a Peer),
}

/// Сколько Murmur должен быть неактивен, чтобы плеер переехал в окно ОС.
const PIP_AWAY: Duration = Duration::from_millis(1500);

/// Содержимое мини-плеера (общее для панели и окна ОС): видео, имя и кнопки при наведении. Возвращает действие и отклик
/// на весь прямоугольник (перетаскивание обрабатывает хозяин: панель двигает себя, окно — StartDrag).
fn pip_player(ui: &mut Ui, rect: Rect, frame: Option<&crate::video::Frame>, name: &str, svol: f32, pinned: bool) -> (Option<PipAct>, egui::Response) {
    let resp = ui.interact(rect, Id::new("pip-body"), Sense::click_and_drag());
    let radius = CornerRadius::same(10);
    ui.painter().rect_filled(rect, radius, Color32::BLACK);
    match frame {
        Some(f) => egui::Image::new(egui::load::SizedTexture::new(f.texture, rect.size())).corner_radius(radius).paint_at(ui, rect),
        None => egui::Spinner::new().size(28.0).color(t::p().accent).paint_at(ui, Rect::from_center_size(rect.center(), vec2(28.0, 28.0))),
    }
    let hovered = ui.rect_contains_pointer(rect);
    ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, if hovered { t::p().accent } else { t::p().border_strong }), StrokeKind::Inside);
    let mut act = None;
    if resp.double_clicked() {
        act = Some(PipAct::Expand);
    }
    if hovered {
        chip(ui.painter(), rect.left_bottom() + vec2(10.0, -10.0), Align2::LEFT_BOTTOM, &format!("{}  {}", ph::MONITOR, name), ON_VIDEO);
        let btn = |ui: &Ui, c: Pos2, icon: &str, tip: &str, key: &str| {
            let r = Rect::from_center_size(c, vec2(32.0, 32.0));
            let rr = ui.interact(r, Id::new(("pip-btn", key)), Sense::click());
            ui.painter().circle_filled(c, 16.0, if rr.hovered() { t::p().accent } else { t::overlay_dark(190) });
            ui.painter().text(c, Align2::CENTER_CENTER, icon, t::font(16.0), ON_VIDEO);
            rr.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        };
        let mut c = pos2(rect.right() - 22.0, rect.top() + 22.0);
        if btn(ui, c, ph::X, "Закрыть", "close") {
            act = Some(PipAct::Close);
        }
        c.x -= 38.0;
        if btn(ui, c, ph::ARROWS_OUT, "Вернуться к звонку", "expand") {
            act = Some(PipAct::Expand);
        }
        c.x -= 38.0;
        let sicon = if svol <= 0.001 { ph::SPEAKER_SLASH } else { ph::SPEAKER_HIGH };
        if btn(ui, c, sicon, if svol <= 0.001 { "Включить звук" } else { "Выключить звук" }, "mute") {
            act = Some(PipAct::Mute);
        }
        c.x -= 38.0;
        if btn(ui, c, if pinned { ph::PUSH_PIN_SLASH } else { ph::PUSH_PIN }, if pinned { "Открепить" } else { "Закрепить этот стрим" }, "pin") {
            act = Some(PipAct::Pin);
        }
        ui.painter().text(rect.right_bottom() - vec2(11.0, 11.0), Align2::CENTER_CENTER, ph::CORNERS_OUT, t::font(12.0), ON_VIDEO_MUTED);
    }
    (act, resp)
}

/// Действие в мини-плеере.
enum PipAct {
    Close,
    Expand,
    Mute,
    Pin,
}

enum StreamAct {
    Watch,
    Focus,
    Fullscreen,
    Grid,
    Pip,
    Unwatch,
}

/// Раскладка n плиток сеткой по центру `r`; высота плитки не больше `aspect` × ширина.
fn grid_rects(r: Rect, n: usize, aspect: f32, gap: f32) -> Vec<Rect> {
    let n = n.max(1);
    let cols = match n {
        1 => 1,
        2 => 2,
        3 | 5 | 6 | 9 => 3,
        4 => 2,
        _ => 4,
    };
    let rows = n.div_ceil(cols);
    let tw = (r.width() - gap * (cols - 1) as f32) / cols as f32;
    let th = ((r.height() - gap * (rows - 1) as f32) / rows as f32).min(tw * aspect);
    let total_h = th * rows as f32 + gap * (rows - 1) as f32;
    let top = r.top() + (r.height() - total_h) / 2.0;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = if row == rows - 1 { n - row * cols } else { cols };
            let row_w = tw * in_row as f32 + gap * (in_row - 1) as f32;
            let left = r.left() + (r.width() - row_w) / 2.0;
            Rect::from_min_size(pos2(left + col as f32 * (tw + gap), top + row as f32 * (th + gap)), vec2(tw, th))
        })
        .collect()
}

/// Компактные плитки (200×68) рядами по центру, прижаты к низу `r`.
fn compact_rects(r: Rect, n: usize, gap: f32) -> Vec<Rect> {
    let size = kit::COMPACT_TILE;
    let cols = (((r.width() + gap) / (size.x + gap)).floor() as usize).max(1);
    let rows = n.div_ceil(cols);
    let top = r.bottom() - rows as f32 * size.y - rows.saturating_sub(1) as f32 * gap;
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = if row == rows - 1 { n - row * cols } else { cols };
            let row_w = size.x * in_row as f32 + gap * (in_row - 1) as f32;
            let left = r.center().x - row_w / 2.0;
            Rect::from_min_size(pos2(left + col as f32 * (size.x + gap), top + row as f32 * (size.y + gap)), size)
        })
        .collect()
}

/// Вписать видео `size` в `outer` с сохранением пропорций (поля по краям).
fn fit_rect(outer: Rect, size: [usize; 2]) -> Rect {
    let (w, h) = (size[0].max(1) as f32, size[1].max(1) as f32);
    let s = (outer.width() / w).min(outer.height() / h);
    Rect::from_center_size(outer.center(), vec2(w * s, h * s))
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() > max { s.chars().take(max - 1).chain(['…']).collect() } else { s.to_string() }
}

/// Текст поверх видео (видео тёмное в обеих темах).
const ON_VIDEO: Color32 = Color32::from_rgb(0xF2, 0xF4, 0xF8);
const ON_VIDEO_MUTED: Color32 = Color32::from_rgb(0xB8, 0xBE, 0xCA);

/// Полупрозрачная подпись поверх видео.
fn chip(painter: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_string(), t::semibold(13.0), color);
    let size = galley.size() + vec2(18.0, 10.0);
    let r = anchor.anchor_size(pos, size);
    painter.rect_filled(r, CornerRadius::same(255), t::overlay_dark(190));
    painter.galley(r.min + vec2(9.0, 5.0), galley, color);
}

/// Круглая кнопка поверх видео.
fn overlay_button(ui: &Ui, center: Pos2, icon: &str, tip: &str, id: Id) -> egui::Response {
    let rect = Rect::from_center_size(center, vec2(36.0, 36.0));
    let resp = ui.interact(rect, id, Sense::click());
    let hot = resp.hovered();
    let p = ui.painter();
    p.circle_filled(center, 18.0, if hot { t::p().accent } else { t::overlay_dark(190) });
    p.text(center, Align2::CENTER_CENTER, icon, t::font(18.0), ON_VIDEO);
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(13.0).color(t::p().text_muted));
}

fn group_label(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        kit::group_label(ui, text);
    });
    ui.add_space(2.0);
}

/// Человек за участником голоса (identity = id пользователя); нет в кэше — имя из LiveKit.
fn peer_user(people: &People, p: &Peer) -> proto::PublicUser {
    people.by_identity(&p.identity).cloned().unwrap_or_else(|| proto::PublicUser {
        id: 0,
        name: p.name.clone(),
        display_name: None,
        avatar: None,
    })
}

fn peer_name(people: &People, p: &Peer) -> String {
    peer_user(people, p).shown_name().to_string()
}

/// Строка участника моего звонка в списке слева. ЛКМ — карточка профиля, ПКМ — громкость (решение владельца 22.11).
pub(crate) fn member_row(ui: &mut Ui, p: &Peer, people: &People) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect.shrink2(vec2(28.0, 0.0)).translate(vec2(14.0, 0.0)), CornerRadius::same(kit::R_ROW), t::p().row_hover);
    }
    let amp = speech_amp(ui, p);
    let phase = wave_phase(ui);
    let painter = ui.painter();
    let c = pos2(rect.left() + 40.0 + 11.0, rect.center().y);
    let u = peer_user(people, p);
    // индикатор «Кольцо» (или анимации выключены) — обводка аватара; «Волна» — волна рядом с ником
    people.paint(painter, c, 11.0, &u, if phase.is_none() { amp * 0.7 } else { 0.0 }, false);
    let color = if amp > 0.02 { t::p().text } else { t::p().text_muted };
    let mut x = rect.right() - 12.0;
    let mut icons_w = 0.0;
    if p.mic_muted {
        painter.text(pos2(x, c.y), Align2::RIGHT_CENTER, egui_phosphor::fill::MICROPHONE_SLASH, t::icon_fill(14.0), t::p().text_faint);
        x -= 20.0;
        icons_w += 20.0;
    }
    if p.streaming {
        painter.text(pos2(x, c.y), Align2::RIGHT_CENTER, egui_phosphor::fill::MONITOR_ARROW_UP, t::icon_fill(14.0), t::p().accent);
        icons_w += 20.0;
    }
    let name_w = crate::banner::paint_truncated(painter, pos2(c.x + 19.0, c.y), Align2::LEFT_CENTER, u.shown_name(), t::semibold(14.0), color, rect.right() - 12.0 - icons_w - 8.0 - (c.x + 19.0));
    if let (Some(phase), true) = (phase, amp > 0.02) {
        let left = c.x + 19.0 + name_w + 8.0;
        if left + 28.0 <= rect.right() - 12.0 - icons_w {
            kit::wave(painter, pos2(left, c.y), 28.0, 1.0 + 4.0 * amp, phase, t::p().speaking, kit::WaveStyle::Solid);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Сглаженная громкость речи участника 0..1 (атака ~30 мс, спад ~200 мс); без анимаций — без сглаживания.
fn speech_amp(ui: &Ui, p: &Peer) -> f32 {
    let target = if p.speaking && !p.mic_muted { (0.35 + p.level.clamp(0.0, 1.0) * 2.5).min(1.0) } else { 0.0 };
    if !crate::anim::important(ui.ctx()) {
        return target;
    }
    crate::anim::smooth_level(ui.ctx(), &p.identity, target)
}

/// Сдвиг волны речи (px): Some — индикатор «Волна» (бежит, пока анимации идут), None — «Кольцо».
fn wave_phase(ui: &Ui) -> Option<f32> {
    if !crate::anim::speech_wave() {
        return None;
    }
    Some(if crate::anim::important(ui.ctx()) { (ui.input(|i| i.time) * 22.0 % 20.0) as f32 } else { 0.0 })
}

fn tile(ui: &Ui, rect: Rect, p: &Peer, amp: f32, people: &People) {
    let u = peer_user(people, p);
    let tile = kit::PeerTile {
        who: kit::AvatarInput { name: people::avatar_name(&u), texture: people.texture_of(&u) },
        name: u.shown_name(),
        speaking: amp,
        wave_phase: wave_phase(ui),
        mic_off: p.mic_muted,
        is_local: p.is_local,
        streaming: p.streaming,
    };
    if rect.height() <= kit::COMPACT_TILE.y + 4.0 {
        kit::peer_tile_compact(ui, rect, &tile);
    } else {
        kit::peer_tile(ui, rect, &tile);
    }
}

/// Статус своей трансляции в шапке: источник, качество, факт к/с и битрейт, энкодер.
fn stream_pill(ui: &mut Ui, s: &voice::StreamInfo) {
    let title: String = if s.title.chars().count() > 28 { s.title.chars().take(27).chain(['…']).collect() } else { s.title.clone() };
    let resp = Frame::new()
        .fill(t::p().accent_soft)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(14, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                // right_to_left родитель — порядок обратный
                if s.viewers > 0 {
                    ui.label(RichText::new(format!("{} {}", egui_phosphor::regular::EYE, s.viewers)).size(12.5).color(t::p().text_muted))
                        .on_hover_text("Сколько человек смотрят");
                }
                if s.minimized {
                    ui.label(RichText::new("окно свёрнуто").size(12.5).color(t::p().warning));
                } else if s.hidden {
                    ui.label(RichText::new("окно закрыто другим — у зрителей стоп-кадр").size(12.5).color(t::p().warning));
                } else if s.fps > 0.0 {
                    ui.label(RichText::new(format!("{:.0} к/с · {:.1} Мбит/с", s.fps, s.kbps / 1000.0)).size(12.5).color(t::p().text_muted));
                }
                // Чем кодируется — видно сразу, без наведения.
                let (tag, color) = if s.backend.contains("видеокарта") { ("видеокарта", t::p().accent_text) } else { ("процессор", t::p().text_muted) };
                ui.label(RichText::new(tag).size(12.0).color(color));
                match &s.audio {
                    voice::AudioStatus::On(_) => {
                        ui.label(RichText::new(egui_phosphor::regular::SPEAKER_HIGH).font(t::font(14.0)).color(t::p().accent));
                    }
                    voice::AudioStatus::Failed(_) => {
                        ui.label(RichText::new(egui_phosphor::regular::SPEAKER_X).font(t::font(14.0)).color(t::p().danger_text));
                    }
                    voice::AudioStatus::Off => {}
                }
                ui.label(RichText::new(&title).font(t::semibold(13.0)).color(t::p().accent_hover));
                ui.label(RichText::new(egui_phosphor::fill::MONITOR_ARROW_UP).font(t::icon_fill(15.0)).color(t::p().accent));
            });
        })
        .response;
    let size = s.size.map(|(w, h)| format!("{w}×{h}")).unwrap_or_else(|| "—".into());
    let enc = s.backend.clone();
    let lim = s.limitation.as_deref().map(|l| format!("
Ограничение качества: {l}")).unwrap_or_default();
    let audio = match &s.audio {
        voice::AudioStatus::Off => "выключен".to_string(),
        voice::AudioStatus::On(what) => what.to_string(),
        voice::AudioStatus::Failed(e) => format!("не захватывается — {e}"),
    };
    resp.on_hover_text(format!("В эфире: {}
Качество: {} · отправляется {size}
Энкодер: {enc}{lim}
Звук: {audio}", s.title, s.preset.label()));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingLevel {
    Good,
    Ok,
    Bad,
}

/// Качество пинга: до 80 мс — хорошо, до 200 — терпимо, дальше — плохо.
fn ping_level(rtt: f64) -> PingLevel {
    match rtt {
        r if r < 0.080 => PingLevel::Good,
        r if r < 0.200 => PingLevel::Ok,
        _ => PingLevel::Bad,
    }
}

/// Пинг в шапке. «Волна и мс»: в норме — статичная тихая линия акцентом; джиттер — дёрганая линия цветом предупреждения
/// (до 15 к/с, только при анимациях «Все»); короткие потери — пунктир цветом предупреждения; потери надолго — пунктир
/// цветом опасности и «потери» вместо мс (сглаживание — `link.rs`).
/// «Только мс» — точка качества и миллисекунды; «Скрыть» — ничего.
fn ping_box(ui: &mut Ui, rtt: f64, jitter: Option<f64>, loss: Option<f64>, state: LinkState) {
    use crate::config::PingDisplay;
    let mode = crate::anim::ping();
    if mode == PingDisplay::Hidden {
        return;
    }
    let p = t::p();
    let note = match state {
        LinkState::Fine => "",
        LinkState::Jitter => "\n\nСвязь неровная — голос может подрагивать.",
        LinkState::Brief => "\n\nБыли короткие потери пакетов — обычно незаметно.",
        LinkState::Loss => "\n\nПотери пакетов держатся — голос может прерываться.",
    };
    let tip = format!(
        "Время до сервера голоса и обратно: {:.0} мс\nДжиттер: {}\nПотери: {}{note}",
        rtt * 1000.0,
        jitter.map_or("—".into(), |j| format!("{:.0} мс", j * 1000.0)),
        loss.map_or("—".into(), |l| format!("{:.1} %", l * 100.0)),
    );
    Frame::new()
        .fill(p.surface_2)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            // Родитель — right_to_left, поэтому порядок обратный: подпись, затем значок.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let (text, color) = match (mode, state) {
                    (PingDisplay::WaveAndMs, LinkState::Loss) => ("потери".to_string(), p.danger_text),
                    _ => (format!("{:.0} мс", rtt * 1000.0), p.text),
                };
                ui.label(RichText::new(text).font(t::bold(13.5)).color(color));
                if mode == PingDisplay::MsOnly {
                    let dot = match ping_level(rtt) {
                        PingLevel::Good => p.speaking,
                        PingLevel::Ok => p.warning,
                        PingLevel::Bad => p.danger,
                    };
                    let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, dot);
                    return;
                }
                let (r, _) = ui.allocate_exact_size(vec2(34.0, 14.0), Sense::hover());
                let left = r.left_center();
                match state {
                    LinkState::Fine => kit::wave(ui.painter(), left, r.width(), 2.5, 0.0, p.accent, kit::WaveStyle::Solid),
                    LinkState::Brief => kit::wave(ui.painter(), left, r.width(), 3.0, 0.0, p.warning, kit::WaveStyle::Dashed),
                    LinkState::Loss => kit::wave(ui.painter(), left, r.width(), 3.5, 0.0, p.danger, kit::WaveStyle::Dashed),
                    LinkState::Jitter => {
                        // «дёрганая» линия: каждый кадр (до 15 к/с) — случайные сдвиг и размах
                        let (phase, amp) = if crate::anim::decor(ui.ctx()) {
                            ui.ctx().request_repaint_after(crate::anim::PING_FRAME);
                            let n = (ui.input(|i| i.time) * 15.0) as u64;
                            let h = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) >> 33;
                            ((h % 20) as f32, 2.5 + (h % 7) as f32 * 0.5)
                        } else {
                            (0.0, 4.0)
                        };
                        kit::wave(ui.painter(), left, r.width(), amp, phase, p.warning, kit::WaveStyle::Solid);
                    }
                }
            });
        })
        .response
        .on_hover_text(tip);
}

/// Выпадающий список устройств. Возвращает true, если выбор изменился.
fn device_combo(ui: &mut Ui, salt: &str, selected: &mut Option<String>, devices: &[voice::Device], w: f32) -> bool {
    let current = match selected {
        None => "По умолчанию (как в Windows)".to_string(),
        Some(id) => devices.iter().find(|d| &d.id == id).map(|d| d.name.clone()).unwrap_or_else(|| "Устройство отключено".into()),
    };
    let before = selected.clone();
    egui::ComboBox::from_id_salt(salt).selected_text(current).width(w).height(320.0).show_ui(ui, |ui| {
        ui.selectable_value(selected, None, "По умолчанию (как в Windows)");
        for d in devices {
            ui.selectable_value(selected, Some(d.id.clone()), &d.name);
        }
    });
    *selected != before
}

/// Громкость микрофона и порог — в колбэк захвата (порог только в режиме «Голосом»).
fn push_gate(s: &Settings) {
    crate::gate::set_params(s.mic_volume, !s.ptt_enabled, s.vad_auto, s.vad_threshold);
}

/// Индикатор уровня микрофона: сегменты (бирюза → жёлтый у пика); ниже порога — тусклые.
/// `thr` — (порог 0..1, можно тянуть): белая метка. Возвращает новый порог, если метку потянули.
fn level_meter(ui: &mut Ui, level: Option<f32>, w: f32, thr: Option<(f32, bool)>) -> Option<f32> {
    const H: f32 = 14.0;
    let sense = if thr.is_some_and(|(_, drag)| drag) { Sense::click_and_drag() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, H + 8.0), sense);
    let bar = Rect::from_min_size(rect.min + vec2(0.0, 4.0), vec2(w, H));
    // шкала индикатора — корень уровня (тихое видно лучше); порог переводим так же
    let to_x = |l: f32| bar.left() + l.clamp(0.0, 1.0).sqrt() * w;
    let mut new_thr = None;
    if let Some((_, true)) = thr
        && let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let k = ((pos.x - bar.left()) / w).clamp(0.0, 1.0);
        new_thr = Some((k * k).clamp(0.005, 1.0));
    }
    let n = 24;
    let gap = 3.0;
    let seg = (w - gap * (n - 1) as f32) / n as f32;
    let lit = level.map(|l| (l.clamp(0.0, 1.0).sqrt() * n as f32).round() as usize).unwrap_or(0);
    let thr_now = new_thr.or(thr.map(|(t, _)| t));
    let p = t::p();
    let painter = ui.painter();
    for i in 0..n {
        let r = Rect::from_min_size(bar.min + vec2(i as f32 * (seg + gap), 0.0), vec2(seg, H));
        let color = if i < lit {
            let c = if i as f32 >= n as f32 * 0.8 { p.warning } else { p.speaking };
            let below = thr_now.is_some_and(|t| r.right() <= to_x(t));
            if below { c.gamma_multiply(0.4) } else { c }
        } else {
            p.surface_3
        };
        painter.rect_filled(r, CornerRadius::same(255), color);
    }
    if let Some(t) = thr_now {
        let x = to_x(t);
        let m = Rect::from_center_size(pos2(x, bar.center().y), vec2(4.0, H + 8.0));
        // белая метка; в светлой теме белое не видно — цветом текста
        let mark = if std::ptr::eq(p, &t::LIGHT) { p.text } else { Color32::WHITE };
        painter.rect_filled(m.translate(vec2(0.0, 1.0)), CornerRadius::same(2), Color32::from_black_alpha(60));
        painter.rect_filled(m, CornerRadius::same(2), mark);
        if thr.is_some_and(|(_, drag)| drag) {
            resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        }
    }
    new_thr
}

fn toggle_row(ui: &mut Ui, w: f32, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    let top = ui.cursor().top();
    ui.painter().hline(ui.max_rect().left()..=ui.max_rect().left() + w, top, Stroke::new(1.0, t::p().border));
    ui.add_space(4.0);
    ui.allocate_ui_with_layout(vec2(w, 36.0), Layout::left_to_right(Align::Center), |ui| {
        left(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(4.0);
}

/// Маленькая красная трубка «Отключиться» (в панели пользователя, когда капсулы не видно).
/// Поле формы входа: 44 px высотой.
fn login_field(text: &mut String, width: f32, id: Id) -> TextEdit<'_> {
    TextEdit::singleline(text).id(id).desired_width(width).margin(Margin::symmetric(14, 12)).font(t::font(15.0))
}

/// Галочка с подписью; true — переключили.
fn login_checkbox(ui: &mut Ui, on: &mut bool, text: &str) -> bool {
    let p = t::p();
    let g = ui.painter().layout_no_wrap(text.to_string(), t::font(13.5), p.text_muted);
    let (rect, resp) = ui.allocate_exact_size(vec2(18.0 + 8.0 + g.size().x, 20.0), Sense::click());
    let b = Rect::from_min_size(pos2(rect.left(), rect.center().y - 9.0), vec2(18.0, 18.0));
    if *on {
        ui.painter().rect_filled(b, CornerRadius::same(5), p.accent_strong);
        let c = b.center();
        ui.painter().add(egui::Shape::line(vec![c + vec2(-4.5, 0.2), c + vec2(-1.3, 3.4), c + vec2(4.6, -3.4)], Stroke::new(2.0, p.on_accent)));
    } else {
        let stroke = if resp.hovered() { p.text_muted } else { p.border_strong };
        ui.painter().rect_stroke(b, CornerRadius::same(5), Stroke::new(1.5, stroke), StrokeKind::Inside);
    }
    ui.painter().galley(pos2(b.right() + 8.0, rect.center().y - g.size().y / 2.0), g, p.text_muted);
    let clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    if clicked {
        *on = !*on;
    }
    clicked
}

/// Большая кнопка формы входа (46 px, accent_strong); true — нажали.
fn login_button(ui: &mut Ui, text: &str, w: f32, enabled: bool) -> bool {
    let p = t::p();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 46.0), if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), kit::HOVER_S);
    let mut fill = p.accent_strong.lerp_to_gamma(p.accent_strong_hover, hover);
    if !enabled {
        fill = fill.gamma_multiply(0.6);
    }
    ui.painter().rect_filled(rect, CornerRadius::same(kit::R_CARD), fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, t::bold(15.0), p.on_accent);
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Фон левой части входа: три тихие «волны пульса» цветом акцента.
fn login_waves(p: &egui::Painter, r: Rect) {
    const PERIOD: f32 = 170.0;
    let blip = [(0.0, 0.0), (7.5, 0.0), (12.5, -1.0), (20.0, 1.0), (25.0, -0.55), (30.0, 0.55), (35.0, 0.0)];
    for (i, (fy, alpha, amp)) in [(0.28, 0.07, 16.0), (0.55, 0.09, 20.0), (0.82, 0.05, 14.0)].into_iter().enumerate() {
        let y = r.top() + r.height() * fy;
        let shift = 40.0 + i as f32 * 63.0;
        let mut pts = vec![pos2(r.left(), y)];
        let mut x0 = r.left() + shift - PERIOD;
        while x0 < r.right() {
            for (bx, by) in blip {
                let x = x0 + bx;
                if x > r.left() && x < r.right() {
                    pts.push(pos2(x, y + by * amp));
                }
            }
            x0 += PERIOD;
        }
        pts.push(pos2(r.right(), y));
        p.add(egui::Shape::line(pts, Stroke::new(2.0, t::p().accent.gamma_multiply(alpha))));
    }
}

#[cfg(test)]
mod ping_tests {
    use super::{PingLevel, compact_rects, ping_level};
    use eframe::egui::{Rect, pos2, vec2};

    #[test]
    fn compact_rects_rows_bottom_aligned() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(650.0, 500.0));
        let rects = compact_rects(r, 4, 12.0);
        // 3 в ряд (3*200 + 2*12 = 624 ≤ 650), второй ряд — одна плитка по центру
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[3].bottom(), 500.0);
        assert!((rects[3].center().x - 325.0).abs() < 0.01);
        assert_eq!(rects[0].top(), 500.0 - 68.0 * 2.0 - 12.0);
    }

    #[test]
    fn ping_levels() {
        assert_eq!(ping_level(0.037), PingLevel::Good);
        assert_eq!(ping_level(0.120), PingLevel::Ok);
        assert_eq!(ping_level(0.350), PingLevel::Bad);
    }
}
