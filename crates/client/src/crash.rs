//! Аварийное падение (нарушение доступа в нативном коде: d3d11, libwebrtc…) → код, адрес и стек в лог.
//! Без этого падение видно только в журнале Windows («Ошибка приложения»), без стека.

/// Паника в любом потоке (в т.ч. в задаче tokio, которую рантайм иначе гасит молча) → сообщение и стек в лог.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "\n[{}] ПАНИКА в потоке {:?}: {info}\n{bt}\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            std::thread::current().name().unwrap_or("?"),
        );
        // Мимо логгера (его замок может держать паникующий поток), как и в обработчике падений ниже.
        if let Some(p) = crate::config::log_path()
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
        {
            use std::io::Write;
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        prev(info);
    }));
}

#[cfg(windows)]
pub fn install() {
    install_panic_hook();
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};

    unsafe extern "system" fn on_crash(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows передаёт действительные указатели на время вызова фильтра.
        let (code, addr) = unsafe {
            info.as_ref()
                .and_then(|i| i.ExceptionRecord.as_ref())
                .map(|r| (r.ExceptionCode as u32, r.ExceptionAddress as usize))
                .unwrap_or((0, 0))
        };
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "\n[{}] ПАДЕНИЕ: исключение {code:#010x} по адресу {addr:#x}, поток {:?}\n{bt}\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            std::thread::current().name().unwrap_or("?"),
        );
        // Мимо логгера: его блокировка может быть занята упавшим потоком.
        if let Some(p) = crate::config::log_path()
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p)
        {
            use std::io::Write;
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        0 // EXCEPTION_CONTINUE_SEARCH: дальше — обычный отчёт Windows
    }

    unsafe { SetUnhandledExceptionFilter(Some(on_crash)) };
}

#[cfg(not(windows))]
pub fn install() {
    install_panic_hook();
}
