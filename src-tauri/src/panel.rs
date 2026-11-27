#![cfg(windows)]

//! 面板窗口：explorer 的显示桌面（Win+D、任务栏右角的「显示桌面」）完全不碰的窗口。
//!
//! 2026-11-19 实测（逐项改窗口样式对照）：
//! 1. 显示桌面会收起带任务栏按钮的窗口，也会收起带标题栏或系统菜单样式（WS_CAPTION、WS_SYSMENU）的窗口；
//!    置不置顶、有没有最小化按钮都无关。
//! 2. 只要屏幕上还有任何「会被收起」的窗口可见，第二次 Win+D 就会再执行一次显示桌面，用户原有的窗口回不来。
//!    所以本应用的每个窗口都必须一次变成面板，不能事后恢复最小化的窗口。
//! 3. 唯一不受影响、也不干扰切换的形态：无所有者、没有任务栏按钮、没有 WS_CAPTION 与 WS_SYSMENU 的
//!    置顶工具窗口（WS_EX_TOOLWINDOW + WS_EX_TOPMOST）。
//!
//! tao（0.37.3 的 `window_state.rs`：`to_window_styles` / `apply_diff`）每次改窗口标志都按自己的标志重写
//! `GWL_STYLE` 与 `GWL_EXSTYLE`，无条件加 `WS_CAPTION | WS_SYSMENU`，没有所有者时按 ON_TASKBAR 加
//! `WS_EX_APPWINDOW`，所以一次性改样式留不住。这里给窗口挂一个子类（`SetWindowSubclass`），在
//! `WM_STYLECHANGING` 里改掉系统将要写入的新样式：`GWL_STYLE` 去掉标题栏、系统菜单与最小化、最大化按钮，
//! `GWL_EXSTYLE` 加上 `WS_EX_TOOLWINDOW`、去掉 `WS_EX_APPWINDOW`。挂上子类之后再主动改一次样式，
//! 让窗口立刻变成面板。
//!
//! `WM_NCACTIVATE` 的默认处理会往窗口上画一条小标题栏（工具窗口的高度），透明的地方会一直露着；
//! 子类把 lParam 改成 -1 再往下交，tao 仍收到这条消息，激活与焦点的事件不变。
//!
//! Windows 7/8 的 ULW presenter（`win7_layered.rs`，`WS_POPUP` + `WS_EX_LAYERED | WS_EX_TOOLWINDOW`，
//! 没有标题栏与系统菜单样式、没有所有者）本身就是面板形态，显示桌面不碰它，无需额外处理。
//!
//! 本应用与参考实现不同：主窗口是 `decorations: false` + `shadow: false`，`WM_NCCALCSIZE` 走无边框分支，
//! 没有 tao「无边框投影」留出的那一圈非客户区，所以不需要动 DWM 外框，也不需要把内容区挪回原处。

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
    STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    WM_NCACTIVATE, WM_NCDESTROY, WM_STYLECHANGING, WS_CAPTION, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
};

/// 面板去掉的样式：标题栏、系统菜单与最小化、最大化按钮。
const DROPPED: u32 = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

/// 子类的标识（与 tao 自己的子类区分开）。
const ID: usize = 0x4B43_504E; // "KCPN"

/// 已经挂上面板子类的窗口（`HWND` 的数值）。子类过程按它判断是不是面板窗口。
static ATTACHED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 把窗口设为面板窗口。样式必须在窗口所在的主线程上改，这里排到主线程执行。
pub fn make_panel(window: &tauri::WebviewWindow) {
    let Ok(raw) = window.hwnd() else {
        eprintln!("[kurocat] panel: 取不到窗口句柄，跳过");
        return;
    };
    let value = raw.0 as isize;
    if value == 0 {
        eprintln!("[kurocat] panel: 窗口句柄为空，跳过");
        return;
    }
    let dispatched = window.run_on_main_thread(move || {
        let hwnd = value as HWND;
        let _ = attach(hwnd);
        restyle(hwnd);
    });
    if let Err(err) = dispatched {
        eprintln!("[kurocat] panel: 排到主线程失败: {err}");
    }
}

/// 给窗口挂面板子类（可重复调用）。返回是否挂着子类：已经挂过或这次挂上都算成功。
pub(crate) fn attach(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let value = hwnd as isize;
    let Ok(list) = ATTACHED.lock() else {
        eprintln!("[kurocat] panel: 记录表被毒化，无法挂接子类");
        return false;
    };
    if list.contains(&value) {
        return true;
    }
    // 先放开记录表再挂子类：挂接与随后的改样式可能同步走进子类过程，子类过程也要读记录表。
    drop(list);
    // 先登记再挂接：子类过程一挂上就按记录表判断是不是面板，登记与挂接之间不能有空档。
    if let Ok(mut list) = ATTACHED.lock() {
        if !list.contains(&value) {
            list.push(value);
        }
    }
    // SAFETY：SetWindowSubclass 只登记子类过程，不向窗口发消息；子类过程是本模块里的函数。
    if unsafe { SetWindowSubclass(hwnd, Some(procedure), ID, 0) } == 0 {
        if let Ok(mut list) = ATTACHED.lock() {
            list.retain(|item| *item != value);
        }
        eprintln!("[kurocat] panel: SetWindowSubclass 失败，显示桌面时窗口会被一起收起");
        return false;
    }
    true
}

/// 主动把窗口改成面板样式（在窗口所在线程上调用）。在子类挂好之后调用一次，窗口立刻变成面板。
pub(crate) fn restyle(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    // SAFETY：都是对本进程窗口样式与重绘的读写；句柄失效时各调用失败，不做事。
    unsafe {
        if IsWindow(hwnd) == 0 {
            return;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let style = style & !(DROPPED as isize);
        let ex = (ex | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let _ = SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // 去掉样式之前系统画在窗口上的标题栏与边框，整窗重画一次。
        let _ = RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_FRAME | RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
        );
    }
}

/// 窗口是不是面板窗口。
fn is_panel(hwnd: HWND) -> bool {
    ATTACHED
        .lock()
        .map(|list| list.contains(&(hwnd as isize)))
        .unwrap_or(false)
}

/// 面板子类过程：改掉系统将要写入的样式，抹掉 WM_NCACTIVATE 的非客户区重画。非面板窗口一律原样透传。
unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        // 窗口销毁后句柄值可能被系统复用：记下它的子类也已随窗口销毁，从记录表里去掉。
        if let Ok(mut list) = ATTACHED.lock() {
            list.retain(|item| *item != hwnd as isize);
        }
    }
    if message == WM_NCACTIVATE && is_panel(hwnd) {
        // 面板没有非客户区，默认处理却会往窗口上画一条小标题栏（见模块说明）：lParam 为 -1 时不重画非客户区。
        // SAFETY：照常交给下一层窗口过程，只换掉文档允许的 lParam。
        return unsafe { DefSubclassProc(hwnd, message, wparam, -1) };
    }
    if message == WM_STYLECHANGING && lparam != 0 && is_panel(hwnd) {
        // SAFETY：WM_STYLECHANGING 的 lParam 指向系统给的 STYLESTRUCT，允许修改其中的新样式；
        // 该结构只在处理这条消息期间有效，就地读写。
        let change = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
        if wparam as i32 == GWL_STYLE {
            change.styleNew &= !DROPPED;
        } else if wparam as i32 == GWL_EXSTYLE {
            change.styleNew = (change.styleNew | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
        }
    }
    // SAFETY：照常交给下一层窗口过程。
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow};

    /// 建一个带标题栏、系统菜单、最小化与最大化按钮以及任务栏按钮样式的测试窗口。
    fn create_normal_window() -> HWND {
        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "KuroCatPanelTest\0".encode_utf16().collect();
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        // SAFETY：注册过的系统类 STATIC；参数都是本函数里的有效缓冲区。
        unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                style,
                100,
                100,
                200,
                200,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        }
    }

    fn attached_records(hwnd: HWND) -> usize {
        ATTACHED
            .lock()
            .map(|list| list.iter().filter(|item| **item == hwnd as isize).count())
            .unwrap_or(0)
    }

    #[test]
    fn subclass_strips_caption_and_sysmenu_on_style_write() {
        let hwnd = create_normal_window();
        assert!(!hwnd.is_null(), "CreateWindowExW 应成功");
        assert!(attach(hwnd), "面板子类应挂接成功");
        restyle(hwnd);

        // 模拟 tao：每次改标志都按自己的标志重写样式（总是带标题栏、系统菜单与任务栏按钮）。
        let style = (WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX) as isize;
        // SAFETY：写的是本测试窗口的样式，子类会把新样式改掉。
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, WS_EX_APPWINDOW as isize);

            let got = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            assert_eq!(
                got & (WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX),
                0,
                "子类应去掉标题栏、系统菜单与最小化、最大化按钮样式"
            );
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_TOOLWINDOW, 0, "扩展样式应含 WS_EX_TOOLWINDOW");
            assert_eq!(ex & WS_EX_APPWINDOW, 0, "扩展样式不应含 WS_EX_APPWINDOW");

            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn make_panel_is_idempotent() {
        let hwnd = create_normal_window();
        assert!(!hwnd.is_null(), "CreateWindowExW 应成功");
        assert!(attach(hwnd), "第一次挂接应成功");
        assert!(attach(hwnd), "第二次挂接应报告已挂上");
        assert_eq!(attached_records(hwnd), 1, "记录表里同一个窗口只应有一条");

        restyle(hwnd);
        // SAFETY：读本测试窗口的样式。
        unsafe {
            let got = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            assert_eq!(
                got & (WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX),
                0,
                "重复挂接后样式仍应是面板样式"
            );
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_TOOLWINDOW, 0, "扩展样式应含 WS_EX_TOOLWINDOW");
            let _ = DestroyWindow(hwnd);
        }
    }
}
