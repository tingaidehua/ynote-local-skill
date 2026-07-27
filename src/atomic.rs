use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() == bytes.len() as u64)
        && fs::read(path).is_ok_and(|existing| existing == bytes)
    {
        return Ok(());
    }
    let temp = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension().and_then(|x| x.to_str()).unwrap_or("data"),
        std::process::id()
    ));
    fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
    if path.exists() {
        let from = wide(temp.as_os_str());
        let to = wide(path.as_os_str());
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&temp);
            return Err(error).with_context(|| format!("atomically replace {}", path.display()));
        }
    } else {
        fs::rename(&temp, path).with_context(|| format!("atomically create {}", path.display()))?;
    }
    Ok(())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
