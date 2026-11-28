//! Звуковые сессии Windows: какие процессы сейчас что-то играют (для списка «Не включать в звук трансляции»
//! и для захвата по приложениям).

use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::Interface;

use crate::app_audio::{ProcInfo, process_list};

/// (PID, играет ли сейчас) по всем сессиям всех активных устройств вывода.
fn sessions() -> windows::core::Result<Vec<(u32, bool)>> {
    let mut out = Vec::new();
    unsafe {
        // Уже инициализированный поток (в т. ч. STA) — не ошибка.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devs = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devs.GetCount()? {
            let Ok(dev) = devs.Item(i) else { continue };
            let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(list) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..list.GetCount().unwrap_or(0) {
                let Ok(s) = list.GetSession(j) else { continue };
                let Ok(state) = s.GetState() else { continue };
                if state == AudioSessionStateExpired {
                    continue;
                }
                let Ok(s2) = s.cast::<IAudioSessionControl2>() else { continue };
                let Ok(pid) = s2.GetProcessId() else { continue };
                out.push((pid, state == AudioSessionStateActive));
            }
        }
    }
    Ok(out)
}

/// PID всех звуковых сессий (без повторов). Ошибка COM — пустой список.
pub fn session_pids() -> Vec<u32> {
    let mut v: Vec<u32> = match sessions() {
        Ok(s) => s.into_iter().map(|(p, _)| p).collect(),
        Err(e) => {
            log::warn!("звуковые сессии: {e}");
            Vec::new()
        }
    };
    v.sort_unstable();
    v.dedup();
    v
}

/// Программа со звуковой сессией.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub exe: String,
    pub pid: u32,
    /// Сейчас что-то играет.
    pub active: bool,
}

/// Одна запись на exe; без своего процесса (и его потомков) и системных звуков (PID 0).
pub fn audio_apps() -> Vec<AudioApp> {
    let ss = match sessions() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("звуковые сессии: {e}");
            return Vec::new();
        }
    };
    group_apps(&ss, &process_list(), unsafe { GetCurrentProcessId() })
}

fn group_apps(ss: &[(u32, bool)], procs: &[ProcInfo], self_pid: u32) -> Vec<AudioApp> {
    let mut out: Vec<AudioApp> = Vec::new();
    for &(pid, active) in ss {
        if pid == 0 || pid == self_pid || is_child_of(pid, self_pid, procs) {
            continue;
        }
        let Some(p) = procs.iter().find(|p| p.pid == pid) else { continue };
        match out.iter_mut().find(|a| a.exe.eq_ignore_ascii_case(&p.exe)) {
            Some(a) => a.active |= active,
            None => out.push(AudioApp { exe: p.exe.clone(), pid, active }),
        }
    }
    out.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.exe.to_lowercase().cmp(&b.exe.to_lowercase())));
    out
}

fn is_child_of(pid: u32, root: u32, procs: &[ProcInfo]) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        let Some(me) = procs.iter().find(|p| p.pid == cur) else { return false };
        if me.parent == root {
            return true;
        }
        if me.parent == 0 || me.parent == cur {
            return false;
        }
        cur = me.parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_grouped_by_exe_active_first() {
        let procs = vec![
            ProcInfo { pid: 5, parent: 0, exe: "murmur.exe".into() },
            ProcInfo { pid: 6, parent: 5, exe: "child.exe".into() },
            ProcInfo { pid: 10, parent: 0, exe: "Zed.exe".into() },
            ProcInfo { pid: 11, parent: 0, exe: "chrome.exe".into() },
            ProcInfo { pid: 12, parent: 11, exe: "chrome.exe".into() },
        ];
        let got = group_apps(&[(0, true), (5, true), (6, true), (10, false), (11, false), (12, true)], &procs, 5);
        assert_eq!(
            got,
            vec![AudioApp { exe: "chrome.exe".into(), pid: 11, active: true }, AudioApp { exe: "Zed.exe".into(), pid: 10, active: false }]
        );
    }
}
