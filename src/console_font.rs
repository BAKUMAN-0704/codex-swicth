//! The font of *this* console window.
//!
//! Two different things get called "the terminal font":
//!
//! * Windows Terminal owns the font of its own tabs. Nothing we do at runtime
//!   changes it; only its `settings.json` does, which is what `codex-switch
//!   font --face` edits.
//! * A console window that our own process opens — double-clicking the exe —
//!   is a conhost window, and there the process may set the face itself via
//!   `SetCurrentConsoleFontEx`. That is what this module does, from a face
//!   remembered in `window.toml`.
//!
//! Inside Windows Terminal the call is a no-op: conhost has no font of its own
//! there, so the request is accepted and ignored. That is fine — the same exe
//! needs to work both ways without asking which one it is.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Remembered look of our own window.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowStyle {
    /// Face name as the terminal API expects it (`Sarasa Term SC Nerd`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub face: String,
    /// Cell height in pixels; `None` keeps whatever the window already uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i16>,
}

impl WindowStyle {
    pub fn is_set(&self) -> bool {
        !self.face.trim().is_empty()
    }
}

fn path() -> std::path::PathBuf {
    crate::paths::window_path()
}

pub fn load() -> WindowStyle {
    let p = path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return WindowStyle::default();
    };
    toml_edit::de::from_str(&text).unwrap_or_default()
}

pub fn save(style: &WindowStyle) -> Result<()> {
    let p = path();
    let text = toml_edit::ser::to_string_pretty(style).context("序列化 window.toml")?;
    let header = "# codex-switch 自己那个控制台窗口的外观（双击 exe 时生效）\n\
                  # Windows Terminal 标签页的字体不在这里，用 `codex-switch font --face` 改\n\n";
    crate::paths::atomic_write(&p, format!("{header}{text}").as_bytes())
}

/// Apply the remembered face to the console this process is attached to.
/// A no-op when nothing is configured.
pub fn apply_saved() {
    let style = load();
    if !style.is_set() {
        return;
    }
    // Startup must never fail because of cosmetics.
    let _ = set(&style.face, style.size);
}

#[cfg(windows)]
mod win {
    use anyhow::{bail, Context, Result};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;

    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct FontInfoEx {
        cb_size: u32,
        font: u32,
        size: Coord,
        family: u32,
        weight: u32,
        face: [u16; 32],
    }

    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    /// `FF_DONTCARE`: let Windows pick the family from the face name.
    const DONTCARE: u32 = 0;
    const NORMAL_WEIGHT: u32 = 400;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE: Handle = -1isize as Handle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn GetCurrentConsoleFontEx(h: Handle, maximum: i32, info: *mut FontInfoEx) -> i32;
        fn SetCurrentConsoleFontEx(h: Handle, maximum: i32, info: *const FontInfoEx) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
    }

    /// The console screen buffer, closed again on drop.
    ///
    /// stdout is tried first; when it has been redirected (a pipe, a log file)
    /// the process can still be sitting in a real console window, and
    /// `CONOUT$` reaches it.
    struct Console(Handle, bool);

    impl Drop for Console {
        fn drop(&mut self) {
            if self.1 {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn open() -> Option<Console> {
        let mut mode = 0u32;
        let std = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if !std.is_null() && std != INVALID_HANDLE && unsafe { GetConsoleMode(std, &mut mode) } != 0
        {
            return Some(Console(std, false));
        }

        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE || h.is_null() {
            None
        } else {
            Some(Console(h, true))
        }
    }

    fn read(h: Handle) -> Option<FontInfoEx> {
        let mut info = FontInfoEx {
            cb_size: std::mem::size_of::<FontInfoEx>() as u32,
            ..Default::default()
        };
        match unsafe { GetCurrentConsoleFontEx(h, 0, &mut info) } {
            0 => None,
            _ => Some(info),
        }
    }

    pub fn current_face() -> Option<String> {
        let console = open()?;
        let info = read(console.0)?;
        let len = info.face.iter().position(|c| *c == 0).unwrap_or(32);
        let name = std::ffi::OsString::from_wide(&info.face[..len])
            .to_string_lossy()
            .into_owned();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    pub fn set(face: &str, size: Option<i16>) -> Result<()> {
        let console =
            open().context("当前进程没有控制台窗口（既没有 stdout 控制台，也打不开 CONOUT$）")?;
        let h = console.0;
        let mut info = read(h).context("读取当前控制台字体失败")?;

        let units: Vec<u16> = face.encode_utf16().collect();
        if units.len() >= info.face.len() {
            bail!("字体名太长（最多 31 个字符）");
        }
        info.face = [0; 32];
        info.face[..units.len()].copy_from_slice(&units);
        info.family = DONTCARE;
        // A named TrueType face needs index 0 and an explicit weight; the
        // index is only meaningful for raster fonts.
        info.font = 0;
        info.weight = NORMAL_WEIGHT;
        if let Some(y) = size {
            // Zero width lets conhost derive it from the font metrics.
            info.size = Coord { x: 0, y };
        }

        if unsafe { SetCurrentConsoleFontEx(h, 0, &info) } == 0 {
            bail!(
                "控制台不接受字体 `{}`（可能没装，或 conhost 认为它不是等宽字体）",
                face
            );
        }

        // The call reports success even when the window is drawn by someone
        // else (Windows Terminal, a pseudo console), so trust the read-back
        // rather than the return value.
        match current_face() {
            Some(now) if now.eq_ignore_ascii_case(face) => Ok(()),
            Some(now) => bail!(
                "窗口现在仍是 `{now}`：这个窗口的字体由宿主程序决定（Windows Terminal / ConPTY），\
                 只有双击 exe 打开的真实控制台才会跟着变"
            ),
            None => Ok(()),
        }
    }
}

#[cfg(windows)]
pub use win::{current_face, set};

#[cfg(not(windows))]
pub fn current_face() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn set(_face: &str, _size: Option<i16>) -> Result<()> {
    anyhow::bail!("只有 Windows 的控制台窗口支持改字体")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_survives_a_toml_round_trip() {
        let style = WindowStyle {
            face: "Sarasa Term SC Nerd".to_string(),
            size: Some(18),
        };
        let text = toml_edit::ser::to_string_pretty(&style).unwrap();
        let back: WindowStyle = toml_edit::de::from_str(&text).unwrap();
        assert_eq!(style, back);
    }

    #[test]
    fn empty_file_is_an_empty_style_not_an_error() {
        let style: WindowStyle = toml_edit::de::from_str("").unwrap();
        assert!(!style.is_set());
        assert_eq!(style.size, None);
    }

    #[test]
    fn face_only_file_loads() {
        let style: WindowStyle = toml_edit::de::from_str("face = \"Cascadia Mono\"\n").unwrap();
        assert_eq!(style.face, "Cascadia Mono");
        assert!(style.is_set());
        assert_eq!(style.size, None);
    }

    #[test]
    fn blank_face_does_not_count_as_set() {
        assert!(!WindowStyle::default().is_set());
        assert!(!WindowStyle {
            face: "   ".to_string(),
            size: None
        }
        .is_set());
    }
}
