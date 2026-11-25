use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Default)]
pub struct Activity {
    pub claude_processes: Option<HashMap<u32, i64>>,
    pub codex_logs: Option<HashSet<PathBuf>>,
    pub claude_terminals: HashMap<u32, Terminal>,
    pub codex_terminals: HashMap<PathBuf, Terminal>,
    pub claude_shells: HashMap<String, HashSet<String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalHost {
    Apple,
    Iterm,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Terminal {
    pub pid: u32,
    pub started: i64,
    device: u32,
    pub host: TerminalHost,
}

#[cfg(target_os = "macos")]
pub use macos::{read, still_open, tty};

#[cfg(not(target_os = "macos"))]
pub fn read() -> Activity {
    Activity::default()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{
        ffi::OsString,
        fs,
        mem::{size_of, size_of_val, MaybeUninit},
        os::unix::{ffi::OsStringExt, fs::MetadataExt},
        ptr,
    };

    // Flavors and the file-info prefix come from the macOS SDK's sys/proc_info.h.
    const PROC_UID_ONLY: u32 = 4;
    const PROC_PIDFDVNODEPATHINFO: i32 = 2;

    #[repr(C)]
    struct ProcFileInfo {
        open_flags: u32,
        status: u32,
        offset: libc::off_t,
        file_type: i32,
        guard_flags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        file: ProcFileInfo,
        node: libc::vnode_info_path,
    }

    fn c_bytes(bytes: &[libc::c_char]) -> Vec<u8> {
        bytes
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| byte as u8)
            .collect()
    }

    fn process_ids() -> Option<Vec<libc::pid_t>> {
        let uid = unsafe { libc::geteuid() };
        // libproc returns the required allocation size when the buffer pointer is null.
        let min_bytes = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, ptr::null_mut(), 0) };
        if min_bytes <= 0 {
            return None;
        }
        let mut pids = vec![0; min_bytes as usize / size_of::<libc::pid_t>() + 128];
        let capacity = i32::try_from(size_of_val(pids.as_slice())).ok()?;
        let bytes =
            unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), capacity) };
        if bytes <= 0 || bytes >= capacity {
            return None;
        }
        pids.truncate(bytes as usize / size_of::<libc::pid_t>());
        Some(pids)
    }

    fn process_info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        (bytes == size).then(|| unsafe { info.assume_init() })
    }

    fn started(info: &libc::proc_bsdinfo) -> i64 {
        (info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000) as i64
    }

    fn terminal(info: &libc::proc_bsdinfo) -> Option<Terminal> {
        let device = info.e_tdev;
        if device == u32::MAX {
            return None;
        }
        let mut host = TerminalHost::Unsupported;
        let mut parent = info.pbi_ppid;
        for _ in 0..16 {
            if parent <= 1 {
                break;
            }
            let Some(process) = process_info(parent as libc::pid_t) else {
                break;
            };
            host = match c_bytes(&process.pbi_comm).as_slice() {
                b"Terminal" => TerminalHost::Apple,
                b"iTerm2" | b"iTermServer" | b"iTerm2Server" => TerminalHost::Iterm,
                _ => TerminalHost::Unsupported,
            };
            if host != TerminalHost::Unsupported || parent == process.pbi_ppid {
                break;
            }
            parent = process.pbi_ppid;
        }
        Some(Terminal {
            pid: info.pbi_pid,
            started: started(info),
            device,
            host,
        })
    }

    pub fn tty(target: Terminal) -> Option<String> {
        if terminal(&process_info(target.pid as libc::pid_t)?)? != target {
            return None;
        }
        fs::read_dir("/dev").ok()?.flatten().find_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with("ttys") {
                return None;
            }
            let path = entry.path();
            (fs::metadata(&path).ok()?.rdev() == u64::from(target.device))
                .then(|| path.to_string_lossy().into_owned())
        })
    }

    fn vnode_paths(pid: libc::pid_t, files: u32, writable: bool) -> Option<Vec<PathBuf>> {
        let mut fds = Vec::new();
        fds.resize_with(files as usize + 32, || libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        });
        let capacity = i32::try_from(size_of_val(fds.as_slice())).ok()?;
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                capacity,
            )
        };
        if bytes <= 0 || bytes >= capacity {
            return None;
        }
        fds.truncate(bytes as usize / size_of::<libc::proc_fdinfo>());
        let mut paths = Vec::new();
        for fd in fds
            .into_iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            let mut vnode = MaybeUninit::<VnodeFdInfoWithPath>::uninit();
            let size = size_of::<VnodeFdInfoWithPath>() as i32;
            let bytes = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    vnode.as_mut_ptr().cast(),
                    size,
                )
            };
            if bytes != size {
                continue;
            }
            let vnode = unsafe { vnode.assume_init() };
            // XNU's proc_fileinfo.fi_openflags uses FREAD (1) and FWRITE (2).
            if writable && vnode.file.open_flags & 2 == 0 {
                continue;
            }
            let path = PathBuf::from(OsString::from_vec(c_bytes(
                vnode.node.vip_path.as_flattened(),
            )));
            if path.is_absolute() {
                paths.push(path);
            }
        }
        Some(paths)
    }

    fn rollout_paths(pid: libc::pid_t, files: u32) -> Option<HashSet<PathBuf>> {
        Some(
            vnode_paths(pid, files, false)?
                .into_iter()
                .filter(|path| {
                    path.is_absolute()
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "jsonl")
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("rollout-"))
                })
                .collect(),
        )
    }

    fn claude_task(path: &std::path::Path, directory: &str) -> Option<(String, String)> {
        if path.extension()?.to_str()? != "output" {
            return None;
        }
        let tasks = path.parent()?;
        if tasks.file_name()?.to_str()? != "tasks" {
            return None;
        }
        let session = tasks.parent()?.file_name()?.to_str()?;
        if !crate::model::is_uuid(session)
            || !path
                .ancestors()
                .any(|part| part.file_name().is_some_and(|name| name == directory))
        {
            return None;
        }
        let id = path.file_stem()?.to_str()?;
        (!id.is_empty()
            && id
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'-'))
        .then(|| (session.to_owned(), id.to_owned()))
    }

    pub fn still_open(target: Terminal, path: &std::path::Path) -> bool {
        process_info(target.pid as libc::pid_t).is_some_and(|info| {
            started(&info) == target.started
                && rollout_paths(target.pid as libc::pid_t, info.pbi_nfiles)
                    .is_some_and(|paths| paths.contains(path))
        })
    }

    pub fn read() -> Activity {
        let Some(pids) = process_ids() else {
            return Activity::default();
        };
        let mut claude_processes = HashMap::new();
        let mut codex_logs = HashSet::new();
        let mut codex_error = false;
        let mut claude_terminals = HashMap::new();
        let mut codex_terminals = HashMap::new();
        let mut candidates = Vec::new();
        let mut parents = HashMap::new();
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            let Some(info) = process_info(pid) else {
                continue;
            };
            let command = c_bytes(&info.pbi_comm);
            let is_shell = matches!(command.as_slice(), b"zsh" | b"bash" | b"sh" | b"fish");
            parents.insert(info.pbi_pid, info.pbi_ppid);
            if info.pbi_nfiles > 0 {
                candidates.push((info.pbi_pid, info.pbi_nfiles, is_shell));
            }
            match command.as_slice() {
                b"claude" => {
                    claude_processes.insert(pid as u32, started(&info));
                    if let Some(target) = terminal(&info) {
                        claude_terminals.insert(pid as u32, target);
                    }
                }
                b"codex" => match rollout_paths(pid, info.pbi_nfiles) {
                    Some(paths) => {
                        if !paths.is_empty() {
                            if let Some(target) = terminal(&info) {
                                for path in &paths {
                                    codex_terminals
                                        .entry(path.clone())
                                        .and_modify(|existing: &mut Terminal| {
                                            if existing.host != target.host
                                                || existing.device != target.device
                                            {
                                                existing.host = TerminalHost::Unsupported;
                                            }
                                        })
                                        .or_insert(target);
                                }
                            }
                            codex_logs.extend(paths);
                        }
                    }
                    None => codex_error |= process_info(pid).is_some(),
                },
                _ => {}
            }
        }
        let directory = format!("claude-{}", unsafe { libc::geteuid() });
        let mut claude_shells = HashMap::<String, HashSet<String>>::new();
        if !claude_processes.is_empty() {
            for (pid, files, is_shell) in candidates {
                let is_claude_child = std::iter::successors(Some(pid), |pid| {
                    parents.get(pid).copied().filter(|parent| *parent > 1)
                })
                .take(16)
                .any(|ancestor| claude_processes.contains_key(&ancestor));
                if !is_shell && !is_claude_child {
                    continue;
                }
                for path in vnode_paths(pid as libc::pid_t, files, true)
                    .into_iter()
                    .flatten()
                {
                    if let Some((session, task)) = claude_task(&path, &directory) {
                        claude_shells.entry(session).or_default().insert(task);
                    }
                }
            }
        }
        Activity {
            claude_processes: Some(claude_processes),
            codex_logs: (!codex_error).then_some(codex_logs),
            claude_terminals,
            codex_terminals,
            claude_shells,
        }
    }
}
