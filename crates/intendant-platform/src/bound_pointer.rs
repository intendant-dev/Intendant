/// Low-level, main-thread-only mouse pair. This is not an authority grant: the
/// controller must retain/revalidate the exact process/window and owned monitor.
/// Native code uses an ARC/exception boundary, like the CGVirtualDisplay shim.
#[cfg(target_os = "macos")]
pub mod bound_pointer {
    use std::{ffi::c_void, marker::PhantomData, ptr::NonNull, rc::Rc};
    extern "C" {
        fn intendant_pointer_ready(pid: i32) -> u8;
        fn intendant_pointer_create(
            pid: i32,
            window: u32,
            x: f64,
            y: f64,
            lx: f64,
            ly: f64,
        ) -> *mut c_void;
        fn intendant_pointer_post(pair: *mut c_void, failed: *mut u8) -> u8;
        fn intendant_pointer_release(pair: *mut c_void);
    }
    pub fn ready(pid: i32) -> bool {
        // SAFETY: scalar-only read-only preflight; shim checks thread/TCC/focus.
        unsafe { intendant_pointer_ready(pid) == 1 }
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
        pub fn create(
            pid: i32,
            window: u32,
            global: (f64, f64),
            local: (f64, f64),
        ) -> Result<Self, String> {
            // SAFETY: scalar-only constructor validates all values and main
            // thread, catches Objective-C exceptions, and returns an owned pair.
            let raw = unsafe {
                intendant_pointer_create(pid, window, global.0, global.1, local.0, local.1)
            };
            Ok(Self {
                raw: NonNull::new(raw).ok_or("exact pointer event construction/SPI unavailable")?,
                used: false,
                _thread: PhantomData,
            })
        }
        /// One-shot contiguous posting attempts; never verifies application effects.
        pub fn post_once(&mut self) -> PostResult {
            if self.used {
                return PostResult {
                    calls: 0,
                    failed: true,
                };
            }
            self.used = true;
            let mut failed = 1u8;
            // SAFETY: uniquely owned main-thread pair and valid writable byte.
            // The shim independently preserves native failure and attempted calls.
            let calls = unsafe { intendant_pointer_post(self.raw.as_ptr(), &mut failed) };
            PostResult {
                calls,
                failed: failed != 0,
            }
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            // SAFETY: exact owned pair released once on the same main thread.
            // Cleanup sends no input and releases every native reference.
            unsafe { intendant_pointer_release(self.raw.as_ptr()) }
        }
    }
}
