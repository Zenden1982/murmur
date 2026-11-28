//! Какие приложения захватывать для звука трансляции экрана, если владелец исключил программы.
//!
//! Windows умеет исключать только одно дерево процессов (оно занято Murmur), поэтому при непустом списке
//! исключений захватывается каждое разрешённое приложение отдельно (INCLUDE дерева) и потоки смешиваются.

use crate::app_audio::{AudioTarget, ProcInfo, app_root};

fn find(procs: &[ProcInfo], pid: u32) -> Option<&ProcInfo> {
    procs.iter().find(|p| p.pid == pid)
}

/// Предки процесса (без него самого), с защитой от петель.
fn ancestors(pid: u32, procs: &[ProcInfo]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..64 {
        let Some(me) = find(procs, cur) else { break };
        let parent = me.parent;
        if parent == 0 || parent == cur || out.contains(&parent) || parent == pid {
            break;
        }
        if find(procs, parent).is_none() {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

/// `pid` — потомок `root` (или он сам).
fn in_tree(pid: u32, root: u32, procs: &[ProcInfo]) -> bool {
    pid == root || ancestors(pid, procs).contains(&root)
}

fn exe_listed(pid: u32, procs: &[ProcInfo], excluded: &[String]) -> bool {
    find(procs, pid).is_some_and(|p| excluded.iter().any(|e| e.eq_ignore_ascii_case(&p.exe)))
}

/// Исключён ли процесс: его exe или exe любого предка в списке.
pub fn is_excluded(pid: u32, procs: &[ProcInfo], excluded: &[String]) -> bool {
    exe_listed(pid, procs, excluded) || ancestors(pid, procs).into_iter().any(|a| exe_listed(a, procs, excluded))
}

/// Цели захвата для экрана. Пустой список исключений — как раньше: всё, кроме своего дерева.
pub fn plan_targets(session_pids: &[u32], procs: &[ProcInfo], self_pid: u32, excluded: &[String]) -> Vec<AudioTarget> {
    if excluded.is_empty() {
        return vec![AudioTarget::AllExcept(self_pid)];
    }
    let excluded_pids: Vec<u32> = procs.iter().map(|p| p.pid).filter(|&p| is_excluded(p, procs, excluded)).collect();
    let mut cands: Vec<u32> = Vec::new();
    for &pid in session_pids {
        // нет в снимке процессов — программа закрылась (сессия Windows ещё висит) или PID уже чужой
        if pid == 0 || find(procs, pid).is_none() || in_tree(pid, self_pid, procs) || is_excluded(pid, procs, excluded) {
            continue;
        }
        let root = app_root(pid, procs);
        let root_ok = !in_tree(root, self_pid, procs) && !excluded_pids.iter().any(|&x| in_tree(x, root, procs));
        let c = if root_ok { root } else { pid };
        // у самого процесса сессии могут быть исключённые потомки — тогда захват дерева нельзя
        if c == pid && excluded_pids.iter().any(|&x| x != pid && in_tree(x, pid, procs)) {
            continue;
        }
        if !cands.contains(&c) {
            cands.push(c);
        }
    }
    let all = cands.clone();
    cands.retain(|&c| !all.iter().any(|&o| o != c && in_tree(c, o, procs)));
    cands.sort_unstable();
    cands.into_iter().map(AudioTarget::App).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, exe: &str) -> ProcInfo {
        ProcInfo { pid, parent, exe: exe.into() }
    }

    fn procs() -> Vec<ProcInfo> {
        vec![
            p(1, 0, "explorer.exe"),
            p(7, 1, "murmur.exe"),
            p(8, 7, "murmur-helper.exe"),
            p(10, 1, "chrome.exe"),
            p(11, 10, "chrome.exe"),
            p(12, 11, "chrome.exe"),
            p(20, 1, "Sonar.EXE"),
            p(21, 20, "sonar-audio.exe"),
            p(30, 1, "launcher.exe"),
            p(31, 30, "game.exe"),
            p(32, 30, "voicefx.exe"),
        ]
    }

    fn ex(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_exclusions_keep_old_mode() {
        assert_eq!(plan_targets(&[12, 20], &procs(), 7, &[]), vec![AudioTarget::AllExcept(7)]);
    }

    #[test]
    fn excluded_exe_case_insensitive() {
        assert_eq!(plan_targets(&[20, 12], &procs(), 7, &ex(&["sonar.exe"])), vec![AudioTarget::App(10)]);
    }

    #[test]
    fn excluded_parent_hides_children() {
        // звук играет дочерний процесс исключённой программы — тоже не захватывается
        assert_eq!(plan_targets(&[21, 12], &procs(), 7, &ex(&["sonar.exe"])), vec![AudioTarget::App(10)]);
    }

    #[test]
    fn excluded_child_under_allowed_root_captures_session_pid_only() {
        // лаунчер запустил игру и обработку голоса; исключена обработка — дерево лаунчера брать нельзя
        assert_eq!(plan_targets(&[31, 32], &procs(), 7, &ex(&["voicefx.exe"])), vec![AudioTarget::App(31)]);
        // лаунчер сам играет звук, а в его дереве исключённый — его не захватить без утечки
        assert_eq!(plan_targets(&[30], &procs(), 7, &ex(&["voicefx.exe"])), vec![]);
    }

    #[test]
    fn descendant_of_target_dropped() {
        assert_eq!(plan_targets(&[10, 12, 11], &procs(), 7, &ex(&["x.exe"])), vec![AudioTarget::App(10)]);
        // игра и лаунчер оба со звуком, ничего не исключено в их дереве — один захват дерева лаунчера
        assert_eq!(plan_targets(&[31, 30], &procs(), 7, &ex(&["x.exe"])), vec![AudioTarget::App(30)]);
    }

    #[test]
    fn self_tree_never_captured() {
        assert_eq!(plan_targets(&[7, 8, 12], &procs(), 7, &ex(&["x.exe"])), vec![AudioTarget::App(10)]);
    }

    #[test]
    fn pid_zero_skipped() {
        assert_eq!(plan_targets(&[0, 12], &procs(), 7, &ex(&["x.exe"])), vec![AudioTarget::App(10)]);
    }

    #[test]
    fn closed_session_pid_dropped() {
        // сессия закрытой программы ещё висит в списке Windows, процесса уже нет
        assert_eq!(plan_targets(&[99, 12], &procs(), 7, &ex(&["x.exe"])), vec![AudioTarget::App(10)]);
    }
}
