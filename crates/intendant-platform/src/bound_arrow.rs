//! Main-thread-only fixed ArrowRight ownership. The controller owns all authority
//! and retained-receiver checks. Construction never posts; Drop only releases.
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull, rc::Rc};
extern "C" {
    fn intendant_arrow_ready(pid: i32) -> u8;
    fn intendant_arrow_create(pid: i32, window: u32) -> *mut c_void;
    fn intendant_arrow_post(pair: *mut c_void, failed: *mut u8) -> u8;
    fn intendant_arrow_release(pair: *mut c_void);
}
pub fn ready(pid: i32) -> bool {
    // SAFETY: scalar read-only probe; native code checks thread, permissions and input.
    unsafe { intendant_arrow_ready(pid) == 1 }
}
pub struct PostResult {
    pub calls: u8,
    pub failed: bool,
}
pub struct Pair {
    raw: NonNull<c_void>,
    used: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Pair {
    pub fn create(pid: i32, window: u32) -> Result<Self, String> {
        // SAFETY: scalar constructor, main-thread check and exception boundary;
        // the non-null Create-rule result is owned exactly once by this wrapper.
        let raw = unsafe { intendant_arrow_create(pid, window) };
        Ok(Self {
            raw: NonNull::new(raw).ok_or("exact ArrowRight construction unavailable")?,
            used: false,
            _thread: PhantomData,
        })
    }
    pub fn post_once(&mut self) -> PostResult {
        if self.used {
            return PostResult {
                calls: 0,
                failed: true,
            };
        }
        self.used = true;
        let mut failed = 1u8;
        // SAFETY: uniquely owned, same-main-thread object and writable flag.
        let calls = unsafe { intendant_arrow_post(self.raw.as_ptr(), &mut failed) };
        PostResult {
            calls,
            failed: failed != 0,
        }
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        // SAFETY: exact native object released once on the creating thread.
        unsafe { intendant_arrow_release(self.raw.as_ptr()) }
    }
}
