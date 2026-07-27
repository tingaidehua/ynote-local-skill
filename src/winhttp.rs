use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::ptr;

type HInternet = *mut c_void;
type Dword = u32;
type Bool = i32;
type Word = u16;

type WinHttpOpen =
    unsafe extern "system" fn(*const u16, Dword, *const u16, *const u16, Dword) -> HInternet;
type WinHttpConnect = unsafe extern "system" fn(HInternet, *const u16, Word, Dword) -> HInternet;
type WinHttpOpenRequest = unsafe extern "system" fn(
    HInternet,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *const *const u16,
    Dword,
) -> HInternet;
type WinHttpSendRequest = unsafe extern "system" fn(
    HInternet,
    *const u16,
    Dword,
    *mut c_void,
    Dword,
    Dword,
    usize,
) -> Bool;
type WinHttpReceiveResponse = unsafe extern "system" fn(HInternet, *mut c_void) -> Bool;
type WinHttpQueryHeaders = unsafe extern "system" fn(
    HInternet,
    Dword,
    *const u16,
    *mut c_void,
    *mut Dword,
    *mut Dword,
) -> Bool;
type WinHttpReadData = unsafe extern "system" fn(HInternet, *mut c_void, Dword, *mut Dword) -> Bool;
type WinHttpCloseHandle = unsafe extern "system" fn(HInternet) -> Bool;
type WinHttpSetTimeouts = unsafe extern "system" fn(HInternet, i32, i32, i32, i32) -> Bool;

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: Dword = 4;
const WINHTTP_FLAG_SECURE: Dword = 0x0080_0000;
const WINHTTP_QUERY_STATUS_CODE: Dword = 19;
const WINHTTP_QUERY_FLAG_NUMBER: Dword = 0x2000_0000;
const MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

struct Api {
    _library: Library,
    open: WinHttpOpen,
    connect: WinHttpConnect,
    open_request: WinHttpOpenRequest,
    send_request: WinHttpSendRequest,
    receive_response: WinHttpReceiveResponse,
    query_headers: WinHttpQueryHeaders,
    read_data: WinHttpReadData,
    close_handle: WinHttpCloseHandle,
    set_timeouts: WinHttpSetTimeouts,
}

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

struct InternetHandle<'a> {
    api: &'a Api,
    value: HInternet,
}

impl Drop for InternetHandle<'_> {
    fn drop(&mut self) {
        if !self.value.is_null() {
            unsafe { (self.api.close_handle)(self.value) };
        }
    }
}

impl Api {
    fn load() -> Result<Self> {
        unsafe {
            let library = Library::new("winhttp.dll").context("load winhttp.dll")?;
            let open = *library.get::<WinHttpOpen>(b"WinHttpOpen\0")?;
            let connect = *library.get::<WinHttpConnect>(b"WinHttpConnect\0")?;
            let open_request = *library.get::<WinHttpOpenRequest>(b"WinHttpOpenRequest\0")?;
            let send_request = *library.get::<WinHttpSendRequest>(b"WinHttpSendRequest\0")?;
            let receive_response =
                *library.get::<WinHttpReceiveResponse>(b"WinHttpReceiveResponse\0")?;
            let query_headers = *library.get::<WinHttpQueryHeaders>(b"WinHttpQueryHeaders\0")?;
            let read_data = *library.get::<WinHttpReadData>(b"WinHttpReadData\0")?;
            let close_handle = *library.get::<WinHttpCloseHandle>(b"WinHttpCloseHandle\0")?;
            let set_timeouts = *library.get::<WinHttpSetTimeouts>(b"WinHttpSetTimeouts\0")?;
            Ok(Self {
                _library: library,
                open,
                connect,
                open_request,
                send_request,
                receive_response,
                query_headers,
                read_data,
                close_handle,
                set_timeouts,
            })
        }
    }
}

pub fn get(url: &str) -> Result<Response> {
    request("GET", url, &[], &[])
}

pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
    let (host, path) = parse_youdao_https_url(url)?;
    if !matches!(method, "GET" | "POST") {
        bail!("unsupported HTTP method");
    }
    let api = Api::load()?;
    let user_agent = wide("ynote-cli/0.2 (local mirror)");
    let session = handle(
        &api,
        unsafe {
            (api.open)(
                user_agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                ptr::null(),
                ptr::null(),
                0,
            )
        },
        "WinHttpOpen",
    )?;
    unsafe { (api.set_timeouts)(session.value, 10_000, 10_000, 60_000, 60_000) };
    let host_wide = wide(host);
    let connection = handle(
        &api,
        unsafe { (api.connect)(session.value, host_wide.as_ptr(), 443, 0) },
        "WinHttpConnect",
    )?;
    let method_wide = wide(method);
    let path_wide = wide(path);
    let request = handle(
        &api,
        unsafe {
            (api.open_request)(
                connection.value,
                method_wide.as_ptr(),
                path_wide.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;

    let header_text = headers
        .iter()
        .map(|(name, value)| {
            if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
                bail!("invalid HTTP header");
            }
            Ok(format!("{name}: {value}\r\n"))
        })
        .collect::<Result<String>>()?;
    let header_wide = wide(&header_text);
    let header_ptr = if header_text.is_empty() {
        ptr::null()
    } else {
        header_wide.as_ptr()
    };
    let body_ptr = if body.is_empty() {
        ptr::null_mut()
    } else {
        body.as_ptr().cast_mut().cast()
    };
    check(
        unsafe {
            (api.send_request)(
                request.value,
                header_ptr,
                header_text.encode_utf16().count() as u32,
                body_ptr,
                body.len() as u32,
                body.len() as u32,
                0,
            )
        },
        "WinHttpSendRequest",
    )?;
    check(
        unsafe { (api.receive_response)(request.value, ptr::null_mut()) },
        "WinHttpReceiveResponse",
    )?;
    let mut status = 0u32;
    let mut status_size = size_of::<u32>() as u32;
    check(
        unsafe {
            (api.query_headers)(
                request.value,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                ptr::null(),
                (&mut status as *mut u32).cast(),
                &mut status_size,
                ptr::null_mut(),
            )
        },
        "WinHttpQueryHeaders(status)",
    )?;
    let mut response_body = Vec::new();
    loop {
        let mut buffer = [0u8; 64 * 1024];
        let mut read = 0u32;
        check(
            unsafe {
                (api.read_data)(
                    request.value,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                )
            },
            "WinHttpReadData",
        )?;
        if read == 0 {
            break;
        }
        response_body.extend_from_slice(&buffer[..read as usize]);
        if response_body.len() > MAX_RESPONSE_BYTES {
            bail!("Youdao response exceeded the 128 MiB safety limit");
        }
    }
    Ok(Response {
        status,
        body: response_body,
    })
}

pub fn form_encode(values: &[(&str, String)]) -> Vec<u8> {
    values
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

pub fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn parse_youdao_https_url(url: &str) -> Result<(&str, &str)> {
    let value = url
        .strip_prefix("https://")
        .context("only HTTPS URLs are supported")?;
    let (host, path) = value.split_once('/').unwrap_or((value, ""));
    if host != "note.youdao.com" {
        bail!("refusing non-Youdao host");
    }
    Ok((
        host,
        if path.is_empty() {
            "/"
        } else {
            &url[url.len() - path.len() - 1..]
        },
    ))
}

fn handle<'a>(api: &'a Api, value: HInternet, operation: &str) -> Result<InternetHandle<'a>> {
    if value.is_null() {
        return Err(anyhow!(
            "{} failed: {}",
            operation,
            std::io::Error::last_os_error()
        ));
    }
    Ok(InternetHandle { api, value })
}

fn check(value: Bool, operation: &str) -> Result<()> {
    if value == 0 {
        return Err(anyhow!(
            "{} failed: {}",
            operation,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_youdao_https_url, percent_encode};

    #[test]
    fn only_allows_exact_youdao_host() {
        assert!(parse_youdao_https_url("https://note.youdao.com/a").is_ok());
        assert!(parse_youdao_https_url("https://note.youdao.com.evil.test/a").is_err());
        assert!(parse_youdao_https_url("http://note.youdao.com/a").is_err());
    }

    #[test]
    fn percent_encoding_is_utf8_safe() {
        assert_eq!(percent_encode("a b/中"), "a%20b%2F%E4%B8%AD");
    }
}
