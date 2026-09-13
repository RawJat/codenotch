//! Jump back to the right terminal: from the session's Claude CLI process PID, walk the parent chain
//! to the hosting terminal window, then SetForegroundWindow + FlashWindowEx. Returns false on failure (the page reports it).

/// Which window to raise, given a session's ancestor chain and the visible titled windows as
/// (handle, pid). The match nearest the session wins: walking to the top of the chain reaches
/// explorer.exe, whose "Program Manager" window is visible and titled, so preferring the highest
/// match raised the desktop for every session started from an editor's terminal.
pub(crate) fn pick_window(
    chain: &[u32],
    ppid: &std::collections::HashMap<u32, u32>,
    wins: &[(isize, u32)],
) -> Option<isize> {
    wins.iter()
        .filter_map(|&(hwnd, pid)| depth_on_chain(pid, chain, ppid).map(|d| (d, hwnd)))
        .min_by_key(|&(d, _)| d)
        .map(|(_, hwnd)| hwnd)
}

/// How far from the session a window's owner sits: its own place on the chain, or its parent's —
/// the classic conhost case, where the console window belongs to a child of the shell.
fn depth_on_chain(
    pid: u32,
    chain: &[u32],
    ppid: &std::collections::HashMap<u32, u32>,
) -> Option<usize> {
    if let Some(i) = chain.iter().position(|&c| c == pid) {
        return Some(i);
    }
    ppid.get(&pid).and_then(|pp| chain.iter().position(|c| c == pp))
}

/// Bring a window to the front, restoring it first if it is minimised.
#[cfg(windows)]
fn raise_window(hwnd_raw: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, IsIconic, SetForegroundWindow, ShowWindow, FLASHWINFO, FLASHW_ALL, SW_RESTORE,
    };
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        let fi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL,
            uCount: 2,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&fi);
    }
    true
}

/// The window hosting a console process, asked of Windows rather than inferred from the process
/// tree. Windows 11 hands a console to whatever is set as the default terminal, so a shell started
/// from Explorer keeps explorer as its parent while Windows Terminal owns the window — a window on
/// no ancestor chain, which the walk below can never reach. The console's own window points at it:
/// its root owner is the terminal.
#[cfg(windows)]
fn console_window_of(pid: u32) -> Option<isize> {
    use windows::Win32::System::Console::{AttachConsole, FreeConsole, GetConsoleWindow};
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, IsWindowVisible, GA_ROOTOWNER};
    unsafe {
        // A process holds one console at a time. A build launched from a terminal already has one,
        // and AttachConsole then fails rather than stealing it — leave that console alone.
        AttachConsole(pid).ok()?;
        let console = GetConsoleWindow();
        let root = if console.0.is_null() {
            None
        } else {
            Some(GetAncestor(console, GA_ROOTOWNER))
        };
        let _ = FreeConsole();
        let root = root?;
        if root.0.is_null() || !IsWindowVisible(root).as_bool() {
            return None;
        }
        Some(root.0 as isize)
    }
}

#[cfg(windows)]
pub fn focus_terminal(claude_pid: u32) -> bool {
    use std::collections::HashMap;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    };

    if claude_pid == 0 {
        return false;
    }

    // 0) The console's host, when Windows can name it: this is the only path that finds a terminal
    //    hosting the session out of tree, as Windows Terminal does for a shell started elsewhere.
    if let Some(h) = console_window_of(claude_pid) {
        return raise_window(h);
    }

    // 1) Full pid -> ppid snapshot
    let mut ppid_map: HashMap<u32, u32> = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                ppid_map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }

    // 2) claude's ancestor chain (itself included), at most 8 levels: node → shell → WindowsTerminal/conhost host…
    let mut chain: Vec<u32> = vec![claude_pid];
    let mut cur = claude_pid;
    for _ in 0..8 {
        match ppid_map.get(&cur) {
            Some(&p) if p != 0 && !chain.contains(&p) => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }

    // 3) Enumerate visible top-level windows
    unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<(isize, u32)>);
        if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            v.push((hwnd.0 as isize, pid));
        }
        BOOL(1)
    }
    let mut raw: Vec<(isize, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }
    // 4) The nearest window-owning ancestor is the one that hosts this session
    let Some(hwnd_raw) = pick_window(&chain, &ppid_map, &raw) else {
        return false;
    };
    raise_window(hwnd_raw)
}

#[cfg(not(windows))]
pub fn focus_terminal(_claude_pid: u32) -> bool {
    false
}

// ---------------- Process and foreground helpers shared by seen-clears-it and the desktop jump-back ----------------

#[cfg(windows)]
pub struct ProcMaps {
    pub ppid: std::collections::HashMap<u32, u32>,
    pub name: std::collections::HashMap<u32, String>, // lower-case exe name
}

#[cfg(windows)]
pub fn proc_maps() -> ProcMaps {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut m = ProcMaps {
        ppid: Default::default(),
        name: Default::default(),
    };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return m;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                m.ppid.insert(e.th32ProcessID, e.th32ParentProcessID);
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                m.name.insert(
                    e.th32ProcessID,
                    String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(),
                );
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }
    m
}

#[cfg(windows)]
pub fn fg_pid() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

#[cfg(windows)]
pub fn chain_of(pid: u32, ppid: &std::collections::HashMap<u32, u32>) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut cur = pid;
    for _ in 0..8 {
        match ppid.get(&cur) {
            Some(&p) if p != 0 && !chain.contains(&p) => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    chain
}

/// Whether the foreground process belongs to a session's terminal window (itself on the chain, or its parent — the conhost case)
#[cfg(windows)]
pub fn pid_hits_chain(pid: u32, chain: &[u32], maps: &ProcMaps) -> bool {
    chain.contains(&pid)
        || maps
            .ppid
            .get(&pid)
            .map(|p| chain.contains(p))
            .unwrap_or(false)
}

/// Focus the Claude desktop app's main window (the jump-back target for desktop sessions: the largest visible window whose process name contains claude)
#[cfg(windows)]
pub fn focus_claude_desktop() -> bool {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FlashWindowEx, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, FLASHWINFO, FLASHW_ALL,
        SW_RESTORE,
    };
    let maps = proc_maps();
    unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<(isize, u32)>);
        if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            v.push((hwnd.0 as isize, pid));
        }
        BOOL(1)
    }
    let mut wins: Vec<(isize, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut wins as *mut _ as isize));
    }
    let mut best: Option<(isize, i64)> = None;
    for (h, pid) in wins {
        let Some(name) = maps.name.get(&pid) else {
            continue;
        };
        if !name.contains("claude") || name.contains("codenotch") {
            continue;
        }
        let mut r = RECT::default();
        let area = unsafe {
            if GetWindowRect(HWND(h as *mut core::ffi::c_void), &mut r).is_ok() {
                ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64)
            } else {
                0
            }
        };
        if best.map(|(_, a)| area > a).unwrap_or(true) {
            best = Some((h, area));
        }
    }
    let Some((h, _)) = best else {
        return false;
    };
    unsafe {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        let fi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL,
            uCount: 2,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&fi);
    }
    true
}

#[cfg(not(windows))]
pub fn focus_claude_desktop() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::pick_window;
    use std::collections::HashMap;

    /// Claude in an editor's terminal: the chain runs claude -> pty host -> editor -> explorer, and
    /// explorer owns "Program Manager". Raising the desktop instead of the editor is the bug this
    /// ordering fixes.
    #[test]
    fn the_editor_window_wins_over_the_desktop() {
        let chain = [25548, 18096, 3940, 10396];
        let ppid = HashMap::new();
        let wins = [(0x2a_isize, 3940_u32), (0x1b, 10396)];
        assert_eq!(pick_window(&chain, &ppid, &wins), Some(0x2a));
    }

    /// The console window belongs to conhost, a child of the shell rather than an ancestor of Claude
    #[test]
    fn a_console_window_counts_through_its_parent() {
        let chain = [700, 800];
        let ppid = HashMap::from([(900_u32, 800_u32)]);
        let wins = [(0x3c_isize, 900_u32)];
        assert_eq!(pick_window(&chain, &ppid, &wins), Some(0x3c));
    }

    #[test]
    fn a_window_belonging_to_nobody_on_the_chain_is_no_jump() {
        let chain = [700, 800];
        let ppid = HashMap::from([(4242_u32, 4243_u32)]);
        let wins = [(0x4d_isize, 4242_u32)];
        assert_eq!(pick_window(&chain, &ppid, &wins), None);
    }
}
