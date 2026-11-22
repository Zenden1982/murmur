//! Управление трансляцией друга: указка (второй курсор) и управление мышью/клавиатурой по разрешению стримера.
//!
//! Логика доступа — чистые автоматы (`host`, `guest`), Win32 — тонкие модули за трейтами.


#[cfg(windows)]
pub mod engine;
pub mod geom;
pub mod guest;
pub mod host;
#[cfg(windows)]
pub mod hooks;
#[cfg(windows)]
pub mod inject;
#[cfg(windows)]
pub mod overlay;
pub mod keys;
pub mod proto;
