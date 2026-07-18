//! Windows executable metadata: friendly display name from the VERSIONINFO
//! resource (`FileDescription`, then `ProductName`). Used to prefill the
//! unknown-game "track it?" prompt with a human name instead of the raw
//! filename ("Night Stones" rather than "nightstones.exe").

/// Friendly display name for an executable, from its version resource.
/// Returns `None` when the exe has no usable version strings.
#[cfg(target_os = "windows")]
pub fn exe_display_name(path: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Query one `\StringFileInfo\{lang}{codepage}\{key}` string.
    unsafe fn query_string(block: &[u8], lang: u16, codepage: u16, key: &str) -> Option<String> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::VerQueryValueW;
        let sub = format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\{key}");
        let sub_w: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            PCWSTR(sub_w.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || ptr.is_null()
            || len == 0
        {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        let s = String::from_utf16_lossy(&chars[..end]).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    let wide = to_wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(
            PCWSTR(wide.as_ptr()),
            None,
            size,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
        )
        .is_err()
        {
            return None;
        }

        // Preferred language/codepage from the translation table; fall back
        // to en-US with the standard Unicode codepage.
        let mut langs: Vec<(u16, u16)> = Vec::new();
        let sub_w = to_wide("\\VarFileInfo\\Translation");
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(
            buf.as_ptr() as *const core::ffi::c_void,
            PCWSTR(sub_w.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            && !ptr.is_null()
            && len >= 4
        {
            let pairs = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
            for pair in pairs.chunks_exact(2) {
                langs.push((pair[0], pair[1]));
            }
        }
        langs.push((0x0409, 0x04B0)); // en-US, Unicode

        for (lang, cp) in langs {
            for key in ["FileDescription", "ProductName"] {
                if let Some(name) = query_string(&buf, lang, cp, key) {
                    return Some(name);
                }
            }
        }
        None
    }
}

#[cfg(not(target_os = "windows"))]
pub fn exe_display_name(_path: &str) -> Option<String> {
    None
}

/// Fallback display name from the exe filename stem: "night_stones.exe" →
/// "night_stones". The prompt shows this when no version resource exists.
pub fn filename_stem(exe: &str) -> String {
    let stem = exe
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(exe)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    if stem.is_empty() {
        exe.to_string()
    } else {
        stem.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_stem_strips_extension_and_dirs() {
        assert_eq!(filename_stem("Night Stones.exe"), "Night Stones");
        assert_eq!(filename_stem("C:\\Games\\thing.EXE"), "thing");
        assert_eq!(filename_stem("noext"), "noext");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn display_name_from_a_real_system_exe() {
        // notepad ships a FileDescription on every Windows install.
        let name = exe_display_name("C:\\Windows\\System32\\notepad.exe");
        assert!(name.is_some(), "expected a version-resource name");
    }
}
