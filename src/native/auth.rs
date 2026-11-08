//! Existing-session PAM authentication. Secrets have a bounded, locked allocation.
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    ptr,
};

const CAPACITY: usize = 4096;
pub struct Secret {
    data: *mut u8,
    len: usize,
}
// The mapping is exclusively owned and never shared between the UI and PAM worker.
unsafe impl Send for Secret {}
impl Secret {
    pub fn new() -> std::io::Result<Self> {
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                CAPACITY,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::mlock(p, CAPACITY) } != 0 {
            unsafe {
                libc::munmap(p, CAPACITY);
            };
            return Err(std::io::Error::last_os_error());
        }
        unsafe {
            libc::madvise(p, CAPACITY, libc::MADV_DONTDUMP);
        }
        Ok(Self {
            data: p.cast(),
            len: 0,
        })
    }
    pub fn append(&mut self, text: &str) {
        if !text.chars().any(char::is_control) && self.len + text.len() < CAPACITY {
            unsafe {
                ptr::copy_nonoverlapping(text.as_ptr(), self.data.add(self.len), text.len());
            }
            self.len += text.len();
        }
    }
    pub fn len(&self) -> usize {
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.data, self.len)) }
            .chars()
            .count()
    }
    pub fn clear(&mut self) {
        unsafe {
            libc::explicit_bzero(self.data.cast(), CAPACITY);
        }
        self.len = 0;
    }
    pub fn backspace(&mut self) {
        if self.len == 0 {
            return;
        }
        let mut start = self.len - 1;
        unsafe {
            while start > 0 && *self.data.add(start) & 0xc0 == 0x80 {
                start -= 1;
            }
            libc::explicit_bzero(self.data.add(start).cast(), self.len - start);
        }
        self.len = start;
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.clear();
        unsafe {
            libc::munlock(self.data.cast(), CAPACITY);
            libc::munmap(self.data.cast(), CAPACITY);
        }
    }
}

#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}
#[repr(C)]
struct Response {
    text: *mut c_char,
    code: c_int,
}
#[repr(C)]
struct Conversation {
    call:
        unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int,
    data: *mut c_void,
}
#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const Conversation,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_setcred(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}
struct Answers {
    secret: Secret,
    used: bool,
}
unsafe extern "C" fn converse(
    n: c_int,
    messages: *mut *const Message,
    out: *mut *mut Response,
    data: *mut c_void,
) -> c_int {
    const ERROR: c_int = 19; // PAM_CONV_ERR
    if n <= 0 || n > 32 || messages.is_null() || out.is_null() || data.is_null() {
        return ERROR;
    }
    *out = ptr::null_mut();
    let answers = &mut *data.cast::<Answers>();
    let responses = libc::calloc(n as usize, std::mem::size_of::<Response>()).cast::<Response>();
    if responses.is_null() {
        return ERROR;
    }
    for i in 0..n as usize {
        let message = *messages.add(i);
        let ok = if message.is_null() {
            false
        } else {
            match (*message).style {
                1 | 2 if !answers.used => {
                    let text = libc::strdup(answers.secret.data.cast());
                    (*responses.add(i)).text = text;
                    answers.used = true;
                    !text.is_null()
                }
                3 | 4 => true,
                _ => false,
            }
        };
        if !ok {
            for j in 0..=i {
                let p = (*responses.add(j)).text;
                if !p.is_null() {
                    libc::explicit_bzero(p.cast(), libc::strlen(p));
                    libc::free(p.cast());
                }
            }
            libc::free(responses.cast());
            return ERROR;
        }
    }
    *out = responses;
    0
}
pub fn username() -> std::io::Result<String> {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut storage = vec![0u8; 16384];
    let mut result = ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            record.as_mut_ptr(),
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    if result.is_null() {
        return Err(std::io::Error::other("현재 사용자를 찾을 수 없습니다"));
    }
    Ok(unsafe { CStr::from_ptr((*result).pw_name) }
        .to_string_lossy()
        .into_owned())
}
pub fn available() -> bool {
    std::path::Path::new("/etc/pam.d/swaylock").is_file()
}
pub fn authenticate(secret: Secret) -> bool {
    let Ok(user) = username().and_then(|s| CString::new(s).map_err(std::io::Error::other)) else {
        return false;
    };
    let mut answers = Answers {
        secret,
        used: false,
    };
    let conv = Conversation {
        call: converse,
        data: (&mut answers as *mut Answers).cast(),
    };
    let mut handle = ptr::null_mut();
    unsafe {
        let mut status = pam_start(c"swaylock".as_ptr(), user.as_ptr(), &conv, &mut handle);
        if status != 0 {
            return false;
        }
        status = pam_authenticate(handle, 1); // PAM_DISALLOW_NULL_AUTHTOK
        if status == 0 {
            let _ = pam_setcred(handle, 0x10);
        } // PAM_REFRESH_CRED
        pam_end(handle, status);
        status == 0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_is_utf8_safe_bounded_and_cleared() {
        let mut s = Secret::new().unwrap();
        s.append("a한😀");
        assert_eq!(s.len(), 3);
        s.backspace();
        assert_eq!(s.len(), 2);
        s.append("\n");
        assert_eq!(s.len(), 2);
        s.clear();
        assert_eq!(s.len(), 0);
        assert!(unsafe { std::slice::from_raw_parts(s.data, CAPACITY) }
            .iter()
            .all(|v| *v == 0));
        s.append(&"a".repeat(CAPACITY));
        assert_eq!(s.len(), 0);
    }
    #[test]
    fn conversation_returns_secret_only_once() {
        unsafe {
            let mut s = Secret::new().unwrap();
            s.append("test-only");
            let mut a = Answers {
                secret: s,
                used: false,
            };
            let message = Message {
                style: 1,
                text: ptr::null(),
            };
            let mut m = &message as *const Message;
            let mut out = ptr::null_mut();
            assert_eq!(
                converse(1, &mut m, &mut out, (&mut a as *mut Answers).cast()),
                0
            );
            assert_eq!(CStr::from_ptr((*out).text).to_bytes(), b"test-only");
            libc::free((*out).text.cast());
            libc::free(out.cast());
            assert_ne!(
                converse(1, &mut m, &mut out, (&mut a as *mut Answers).cast()),
                0
            );
            assert!(out.is_null());
        }
    }
}
