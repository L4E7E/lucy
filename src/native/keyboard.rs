use std::{
    ffi::{c_char, c_void},
    fs::File,
    os::fd::OwnedFd,
};
#[link(name = "xkbcommon")]
extern "C" {
    fn xkb_context_new(flags: u32) -> *mut c_void;
    fn xkb_context_unref(p: *mut c_void);
    fn xkb_keymap_new_from_string(
        ctx: *mut c_void,
        text: *const c_char,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn xkb_keymap_unref(p: *mut c_void);
    fn xkb_state_new(map: *mut c_void) -> *mut c_void;
    fn xkb_state_unref(p: *mut c_void);
    fn xkb_state_update_mask(
        state: *mut c_void,
        depressed: u32,
        latched: u32,
        locked: u32,
        dl: u32,
        ll: u32,
        group: u32,
    ) -> u32;
    fn xkb_state_key_get_one_sym(state: *mut c_void, key: u32) -> u32;
    fn xkb_state_key_get_utf8(state: *mut c_void, key: u32, out: *mut c_char, size: usize) -> i32;
    fn xkb_state_mod_name_is_active(state: *mut c_void, name: *const c_char, component: u32)
        -> i32;
}
pub struct Keyboard {
    context: *mut c_void,
    map: *mut c_void,
    state: *mut c_void,
}
impl Keyboard {
    pub fn new() -> Self {
        Self {
            context: unsafe { xkb_context_new(0) },
            map: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
        }
    }
    pub fn keymap(&mut self, fd: OwnedFd, size: u32) {
        if size == 0 || size > 4 * 1024 * 1024 {
            return;
        }
        let file = File::from(fd);
        // Received keymap descriptors share their file offset with other clients.
        // mmap at zero: read()/seek() would corrupt subsequent keymap deliveries.
        let Ok(bytes) = (unsafe { memmap2::MmapOptions::new().len(size as usize).map(&file) })
        else {
            return;
        };
        if bytes.last() != Some(&0) {
            return;
        }
        unsafe {
            if !self.state.is_null() {
                xkb_state_unref(self.state);
            }
            if !self.map.is_null() {
                xkb_keymap_unref(self.map);
            }
            self.map = xkb_keymap_new_from_string(self.context, bytes.as_ptr().cast(), 1, 0);
            self.state = if self.map.is_null() {
                std::ptr::null_mut()
            } else {
                xkb_state_new(self.map)
            };
        }
    }
    pub fn modifiers(&mut self, d: u32, l: u32, k: u32, g: u32) {
        if !self.state.is_null() {
            unsafe {
                xkb_state_update_mask(self.state, d, l, k, 0, 0, g);
            }
        }
    }
    pub fn key(&self, key: u32) -> (u32, String, bool) {
        if self.state.is_null() {
            return (0, String::new(), false);
        }
        unsafe {
            let code = key + 8;
            let sym = xkb_state_key_get_one_sym(self.state, code);
            let mut buffer = [0u8; 128];
            let n =
                xkb_state_key_get_utf8(self.state, code, buffer.as_mut_ptr().cast(), buffer.len());
            let ctrl = xkb_state_mod_name_is_active(self.state, c"Control".as_ptr(), 1 << 3) > 0;
            (
                sym,
                String::from_utf8_lossy(&buffer[..(n.max(0) as usize).min(127)]).into_owned(),
                ctrl,
            )
        }
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe {
            if !self.state.is_null() {
                xkb_state_unref(self.state);
            }
            if !self.map.is_null() {
                xkb_keymap_unref(self.map);
            }
            if !self.context.is_null() {
                xkb_context_unref(self.context);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::CStr,
        io::{Seek, SeekFrom, Write},
        os::fd::{AsRawFd, FromRawFd},
    };
    #[link(name = "xkbcommon")]
    extern "C" {
        fn xkb_keymap_new_from_names(
            ctx: *mut c_void,
            names: *const c_void,
            flags: u32,
        ) -> *mut c_void;
        fn xkb_keymap_get_as_string(map: *mut c_void, format: u32) -> *mut c_char;
    }
    #[test]
    fn keymap_uses_mmap_without_changing_shared_fd_offset() {
        let mut keyboard = Keyboard::new();
        let bytes = unsafe {
            let map = xkb_keymap_new_from_names(keyboard.context, std::ptr::null(), 0);
            assert!(!map.is_null());
            let text = xkb_keymap_get_as_string(map, 1);
            let bytes = CStr::from_ptr(text).to_bytes_with_nul().to_vec();
            libc::free(text.cast());
            xkb_keymap_unref(map);
            bytes
        };
        let fd = unsafe { libc::memfd_create(c"lucy-keymap-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes).unwrap();
        let position = file.stream_position().unwrap();
        keyboard.keymap(file.try_clone().unwrap().into(), bytes.len() as u32);
        assert_eq!(file.stream_position().unwrap(), position);
        assert_eq!(keyboard.key(30).1, "a");
        keyboard.modifiers(1, 0, 0, 0);
        assert_eq!(keyboard.key(30).1, "A");
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) },
            position as i64
        );
        file.seek(SeekFrom::Start(0)).unwrap();
    }
}
