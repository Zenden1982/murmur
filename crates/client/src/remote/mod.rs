//! Управление трансляцией друга: указка (второй курсор) и управление мышью/клавиатурой по разрешению стримера.
//!
//! Логика доступа — чистые автоматы (`host`, `guest`), Win32 — тонкие модули за трейтами.

// Временно: подключается к voice/app в задачах 8–9 — тогда убрать.
#![allow(dead_code)]

pub mod geom;
pub mod guest;
pub mod host;
#[cfg(windows)]
pub mod inject;
pub mod keys;
pub mod proto;
