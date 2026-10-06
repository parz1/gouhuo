// SPDX-License-Identifier: GPL-3.0-or-later
//! System certificate/proxy handling; explicit HTTPS redirects to GitHub only.

use super::Fetch;
use std::io;

pub struct Https;

fn endpoint(url: &str) -> io::Result<(&str, &str)> {
    if url.len() > 8192
        || !url.is_ascii()
        || url.bytes().any(|b| b <= 32 || b == 127)
        || url.contains('#')
    {
        return Err(io::Error::other("invalid update URL"));
    }
    let https = url
        .strip_prefix("https://")
        .ok_or_else(|| io::Error::other("update URL must use HTTPS"))?;
    let slash = https
        .find('/')
        .ok_or_else(|| io::Error::other("invalid update URL"))?;
    let (host, path) = https.split_at(slash);
    if ![
        "api.github.com",
        "github.com",
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
    ]
    .iter()
    .any(|allowed| host.eq_ignore_ascii_case(allowed))
    {
        return Err(io::Error::other("update host is not allowed"));
    }
    Ok((host, path))
}

#[cfg(windows)]
impl Fetch for Https {
    fn get(
        &mut self,
        url: &str,
        maximum: usize,
        allowed: &dyn Fn() -> bool,
    ) -> io::Result<Vec<u8>> {
        use super::check_allowed;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Networking::WinHttp::*;

        struct Handle(*mut core::ffi::c_void);
        impl Drop for Handle {
            fn drop(&mut self) {
                // SAFETY: this owns one non-null WinHTTP handle.
                unsafe {
                    WinHttpCloseHandle(self.0);
                }
            }
        }
        fn handle(raw: *mut core::ffi::c_void) -> io::Result<Handle> {
            if raw.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Handle(raw))
            }
        }
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let deadline = Instant::now() + Duration::from_secs(60);
        let proceed = || {
            check_allowed(allowed)?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "update request deadline exceeded",
                ));
            }
            Ok(())
        };
        let mut current = url.to_string();
        for _ in 0..=3 {
            proceed()?;
            let (host, path) = endpoint(&current)?;
            let (agent, host, path, verb) = (
                wide("gouhuo-voice-update/1"),
                wide(host),
                wide(path),
                wide("GET"),
            );
            // SAFETY: all UTF-16 strings are terminated and live throughout the
            // calls; request/session handles and output buffers remain owned.
            unsafe {
                let session = handle(WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                ))?;
                if WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let connection = handle(WinHttpConnect(
                    session.0,
                    host.as_ptr(),
                    INTERNET_DEFAULT_HTTPS_PORT,
                    0,
                ))?;
                let request = handle(WinHttpOpenRequest(
                    connection.0,
                    verb.as_ptr(),
                    path.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    WINHTTP_FLAG_SECURE,
                ))?;
                let disabled = WINHTTP_DISABLE_REDIRECTS
                    | WINHTTP_DISABLE_COOKIES
                    | WINHTTP_DISABLE_AUTHENTICATION;
                if WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_DISABLE_FEATURE,
                    (&disabled as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                proceed()?;
                if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0)
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
                proceed()?;
                if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
                    return Err(io::Error::last_os_error());
                }
                proceed()?;
                let mut status = 0u32;
                let mut length = std::mem::size_of::<u32>() as u32;
                if WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    std::ptr::null(),
                    (&mut status as *mut u32).cast(),
                    &mut length,
                    std::ptr::null_mut(),
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if [301, 302, 303, 307, 308].contains(&status) {
                    let mut location = [0u16; 4096];
                    let mut length = std::mem::size_of_val(&location) as u32;
                    if WinHttpQueryHeaders(
                        request.0,
                        WINHTTP_QUERY_LOCATION,
                        std::ptr::null(),
                        location.as_mut_ptr().cast(),
                        &mut length,
                        std::ptr::null_mut(),
                    ) == 0
                        || length as usize > std::mem::size_of_val(&location)
                    {
                        return Err(io::Error::other("invalid update redirect"));
                    }
                    current = String::from_utf16(&location[..length as usize / 2])
                        .map_err(|_| io::Error::other("invalid update redirect"))?
                        .trim_end_matches('\0')
                        .into();
                    endpoint(&current)?;
                    continue;
                }
                if status != 200 {
                    return Err(io::Error::other(format!("update HTTP status {status}")));
                }
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 16384];
                loop {
                    proceed()?;
                    let mut count = 0u32;
                    if WinHttpReadData(
                        request.0,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut count,
                    ) == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if count == 0 {
                        break;
                    }
                    if count as usize > maximum.saturating_sub(bytes.len()) {
                        return Err(io::Error::other("update response too large"));
                    }
                    bytes.extend_from_slice(&buffer[..count as usize]);
                }
                proceed()?;
                return Ok(bytes);
            }
        }
        Err(io::Error::other("too many update redirects"))
    }
}

#[cfg(not(windows))]
impl Fetch for Https {
    fn get(&mut self, url: &str, _: usize, _: &dyn Fn() -> bool) -> io::Result<Vec<u8>> {
        endpoint(url)?;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "system HTTPS downloader requires Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirects_cannot_downgrade_escape_or_send_credentials() {
        for url in [
            "http://github.com/x",
            "https://github.com.evil/x",
            "https://user@github.com/x",
            "https://github.com:443/x",
            "https://127.0.0.1/x",
            "https://github.com/x#secret",
            "https://github.com/x\r\nCookie:foo",
        ] {
            assert!(endpoint(url).is_err(), "accepted {url:?}");
        }
        for url in [
            "https://api.github.com/repos/parz1/gouhuo/releases",
            "https://github.com/x",
            "https://release-assets.githubusercontent.com/x?token=test",
            "https://objects.githubusercontent.com/x",
        ] {
            assert!(endpoint(url).is_ok());
        }
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "requires outbound HTTPS to the public release API"]
    fn system_https_fetches_release_index() {
        let bytes = Https
            .get(
                &format!(
                    "https://api.github.com/repos/{}/releases?per_page=100",
                    super::super::REPO
                ),
                super::super::MAX_INDEX,
                &|| true,
            )
            .unwrap();
        let _: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    }
}
