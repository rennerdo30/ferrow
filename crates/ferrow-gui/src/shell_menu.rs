//! The Explorer context menu for a result (Windows).
//!
//! Right-clicking a result shows the same menu as in Explorer (Open with,
//! Send to, Properties, cloud and archive tools, ...) with Ferrow's own
//! items on top. The menu runs on its own thread with its own hidden owner
//! window: shell extensions can be slow, and resolving a path on a
//! disconnected network drive can hang, so none of that may touch the UI
//! thread (never freeze). One menu at a time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    IContextMenu, IContextMenu2, IContextMenu3, ILFree, IShellFolder, SHBindToParent,
    SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, HMENU, MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW, SW_SHOWNORMAL,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WINDOW_EX_STYLE,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NULL, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCSTR, PCWSTR, w};

/// `CMIC_MASK_UNICODE` (the crate only names it `SEE_MASK_UNICODE`).
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
const OPEN_LOCATION: u32 = 1;
const COPY_PATH: u32 = 2;
const COPY_NAME: u32 = 3;
/// Shell commands get ids from here up.
const FIRST_SHELL: u32 = 0x100;
const LAST_SHELL: u32 = 0x7FFF;

static OPEN: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The menu being shown on this thread, for owner-drawn and lazily
    /// filled submenus (Open with, Send to).
    static ACTIVE: std::cell::RefCell<Option<IContextMenu>> = const { std::cell::RefCell::new(None) };
}

/// Show the menu for `path` at the mouse position. Returns at once; the
/// menu lives on its own thread. Ignored while another menu is open.
pub fn show(path: PathBuf, extended: bool) {
    if OPEN.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ferrow-shell-menu".into())
        .spawn(move || {
            run(&path, extended);
            OPEN.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        OPEN.store(false, Ordering::Release);
    }
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

/// Owns a shell item id list.
struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: allocated by SHParseDisplayName, freed once.
        unsafe { ILFree(Some(self.0)) };
    }
}

/// The shell's context menu object for `path`, or None (path gone,
/// unreachable, no handler).
fn shell_menu_for(path: &Path, owner: HWND) -> Option<IContextMenu> {
    let name = wide(path.as_os_str());
    let mut raw = std::ptr::null_mut();
    // SAFETY: a terminated path; `raw` receives an id list we own.
    unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut raw, 0, None) }.ok()?;
    let pidl = Pidl(raw);
    let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `pidl` is valid; `child` points into it and is used while it lives.
    let folder: IShellFolder = unsafe { SHBindToParent(pidl.0, Some(&mut child)) }.ok()?;
    // SAFETY: as above.
    unsafe { folder.GetUIObjectOf(owner, &[child as *const ITEMIDLIST], None) }.ok()
}

/// Build the whole popup: Ferrow's items, then the shell's (when there is
/// a shell menu). The caller destroys the menu.
fn build(path: &Path, owner: HWND, extended: bool) -> Option<(HMENU, Option<IContextMenu>)> {
    // SAFETY: plain menu calls on menus this function owns.
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (id, text) in [
            (OPEN_LOCATION, w!("Open file &location")),
            (COPY_PATH, w!("Copy full &path")),
            (COPY_NAME, w!("Copy &name")),
        ] {
            let _ = AppendMenuW(menu, MF_STRING, id as usize, text);
        }
        let shell = shell_menu_for(path, owner);
        if let Some(shell) = &shell {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let flags = if extended {
                CMF_NORMAL | CMF_EXTENDEDVERBS
            } else {
                CMF_NORMAL
            };
            let position = 4; // after Ferrow's three items and the separator
            let _ = shell.QueryContextMenu(menu, position, FIRST_SHELL, LAST_SHELL, flags);
        }
        Some((menu, shell))
    }
}

unsafe extern "system" fn owner_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = ACTIVE.with(|active| {
            let active = active.borrow();
            let menu = active.as_ref()?;
            let mut result = LRESULT(0);
            // SAFETY: forwarding the message the menu loop gave us.
            unsafe {
                if let Ok(menu3) = windows::core::Interface::cast::<IContextMenu3>(menu) {
                    menu3
                        .HandleMenuMsg2(msg, wparam, lparam, Some(&mut result))
                        .ok()?;
                } else {
                    let menu2: IContextMenu2 = windows::core::Interface::cast(menu).ok()?;
                    menu2.HandleMenuMsg(msg, wparam, lparam).ok()?;
                }
            }
            Some(result)
        });
        if let Some(result) = handled {
            return result;
        }
    }
    // SAFETY: default handling for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A hidden window on this thread to own the menu.
fn owner_window() -> Option<HWND> {
    // SAFETY: registering a class whose procedure lives for the process;
    // a second registration fails harmlessly with "class exists".
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(owner_proc),
            hInstance: instance.into(),
            lpszClassName: w!("FerrowShellMenuOwner"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WINDOW_EX_STYLE(0),
            w!("FerrowShellMenuOwner"),
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()
    }
}

fn run(path: &Path, extended: bool) {
    // SAFETY: this thread is ours; STA as shell extensions require.
    if unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_err() {
        return;
    }
    if let Some(owner) = owner_window() {
        show_on(owner, path, extended);
        // SAFETY: our own window, on this thread.
        let _ = unsafe { DestroyWindow(owner) };
    }
    // SAFETY: pairs the successful CoInitializeEx above.
    unsafe { CoUninitialize() };
}

fn show_on(owner: HWND, path: &Path, extended: bool) {
    let Some((menu, shell)) = build(path, owner, extended) else {
        return;
    };
    let mut at = POINT::default();
    // SAFETY: menu calls on our own menu and window; the shell object is
    // kept alive (in ACTIVE and `shell`) until after InvokeCommand.
    let invoked = unsafe {
        let _ = GetCursorPos(&mut at);
        ACTIVE.with(|active| *active.borrow_mut() = shell.clone());
        // Without this the menu would not close when clicking elsewhere.
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            at.x,
            at.y,
            owner,
            None,
        )
        .0 as u32;
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        // The menu is closed: another right-click may open a new one.
        OPEN.store(false, Ordering::Release);
        let invoked = chosen >= FIRST_SHELL;
        match chosen {
            OPEN_LOCATION => crate::platform::reveal(path),
            COPY_PATH => crate::platform::copy_text(&path.display().to_string()),
            COPY_NAME => crate::platform::copy_text(&path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )),
            id if id >= FIRST_SHELL => {
                if let Some(shell) = &shell {
                    let offset = (id - FIRST_SHELL) as usize;
                    let info = CMINVOKECOMMANDINFOEX {
                        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
                        hwnd: owner,
                        // MAKEINTRESOURCE: the command offset as a pointer.
                        lpVerb: PCSTR(offset as *const u8),
                        lpVerbW: PCWSTR(offset as *const u16),
                        nShow: SW_SHOWNORMAL.0,
                        ptInvoke: at,
                        ..Default::default()
                    };
                    let _ = shell.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
                }
            }
            _ => {}
        }
        ACTIVE.with(|active| active.borrow_mut().take());
        let _ = DestroyMenu(menu);
        invoked
    };
    if invoked {
        // The shell object stays alive while its dialogs run.
        serve_dialogs(owner);
    }
    drop(shell);
}

/// Some commands open a dialog owned by this thread (Open with → Choose
/// another app, Delete confirmations, cloud prompts). Keep its messages
/// flowing while one is open, so the dialog works and is not torn down
/// with the owner window. Bounded: a short grace period for a dialog to
/// appear, then as long as one is visible, at most ten minutes.
fn serve_dialogs(owner: HWND) {
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HANDLE, TRUE};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EnumThreadWindows, IsWindowVisible, MSG, MsgWaitForMultipleObjects,
        PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage,
    };
    unsafe extern "system" fn visible(hwnd: HWND, owner: LPARAM) -> windows::core::BOOL {
        // SAFETY: a window handle the system passed us.
        if hwnd.0 as isize != owner.0 && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return windows::core::BOOL(0); // found one: stop
        }
        TRUE
    }
    let start = Instant::now();
    let grace = Duration::from_secs(2);
    let limit = Duration::from_secs(600);
    loop {
        // SAFETY: enumerating this thread's own windows.
        let open = unsafe {
            !EnumThreadWindows(
                GetCurrentThreadId(),
                Some(visible),
                LPARAM(owner.0 as isize),
            )
            .as_bool()
        };
        let elapsed = start.elapsed();
        if (!open && elapsed >= grace) || elapsed >= limit {
            return;
        }
        // SAFETY: waiting for and dispatching this thread's messages.
        unsafe {
            MsgWaitForMultipleObjects(None::<&[HANDLE]>, false, 250, QS_ALLINPUT);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount;

    /// The shell adds its items (Open, Properties, ...) after Ferrow's,
    /// without showing anything on screen.
    #[test]
    fn builds_ferrow_and_shell_items_for_a_file() {
        let dir = std::env::temp_dir().join(format!("ferrow-shell-menu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("menu test.txt");
        std::fs::write(&file, b"x").unwrap();
        // SAFETY: test thread, paired below.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.unwrap();
        let owner = owner_window().expect("owner window");
        let (menu, shell) = build(&file, owner, false).expect("menu");
        // SAFETY: our own menu.
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        assert!(shell.is_some(), "the shell has a context menu for a file");
        assert!(
            count > 5,
            "Ferrow's 3 items, a separator and shell items: {count}"
        );
        drop(shell);
        // SAFETY: our own menu, window and COM init.
        unsafe {
            let _ = DestroyMenu(menu);
            let _ = DestroyWindow(owner);
            CoUninitialize();
        }
        // A missing path still gets Ferrow's own items.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.unwrap();
        let owner = owner_window().expect("owner window");
        let (menu, shell) = build(&dir.join("gone.txt"), owner, false).expect("menu");
        assert!(shell.is_none());
        // SAFETY: as above.
        unsafe {
            assert_eq!(GetMenuItemCount(Some(menu)), 3);
            let _ = DestroyMenu(menu);
            let _ = DestroyWindow(owner);
            CoUninitialize();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
