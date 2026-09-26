//! Executable memory for generated code.
//!
//! macOS on Apple silicon: one `MAP_JIT` mapping, toggled between writable
//! and executable per thread with `pthread_jit_write_protect_np`, and the
//! instruction cache invalidated for every range written. Linux (AArch64,
//! e.g. the Raspberry Pi): a read-write-execute anonymous mapping and
//! `__clear_cache`. The Linux path compiles but has not been run yet.

use std::ffi::c_void;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
#[cfg(target_os = "macos")]
const MAP_FLAGS: i32 = 0x0002 /* PRIVATE */ | 0x1000 /* ANON */ | 0x0800 /* JIT */;
#[cfg(not(target_os = "macos"))]
const MAP_FLAGS: i32 = 0x02 /* PRIVATE */ | 0x20 /* ANONYMOUS */;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
        -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

#[cfg(target_os = "macos")]
extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

#[cfg(not(target_os = "macos"))]
extern "C" {
    fn __clear_cache(start: *mut c_void, end: *mut c_void);
}

/// A bump-allocated region of executable memory.
pub struct CodeBuffer {
    base: *mut u8,
    size: usize,
    used: usize,
}

// The buffer is owned by one compiler and only written through `&mut self`;
// shared references read nothing but its size.
unsafe impl Send for CodeBuffer {}
unsafe impl Sync for CodeBuffer {}

impl CodeBuffer {
    /// Map `size` bytes of executable memory.
    pub fn new(size: usize) -> Option<Self> {
        // SAFETY: anonymous private mapping, no file descriptor.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_FLAGS,
                -1,
                0,
            )
        };
        if base as isize == -1 || base.is_null() {
            return None;
        }
        Some(Self {
            base: base.cast(),
            size,
            used: 0,
        })
    }

    /// Copy `words` in and return their entry address, or `None` when the
    /// buffer is full.
    pub fn install(&mut self, words: &[u32]) -> Option<*const u8> {
        let bytes = words.len() * 4;
        // Keep entries 16-byte aligned.
        let start = (self.used + 15) & !15;
        if start + bytes > self.size {
            return None;
        }
        // SAFETY: `start..start + bytes` lies inside the mapping; the write
        // window is opened for this thread only while copying.
        unsafe {
            let dst = self.base.add(start);
            #[cfg(target_os = "macos")]
            pthread_jit_write_protect_np(0);
            std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), dst, bytes);
            #[cfg(target_os = "macos")]
            {
                pthread_jit_write_protect_np(1);
                sys_icache_invalidate(dst.cast(), bytes);
            }
            #[cfg(not(target_os = "macos"))]
            __clear_cache(dst.cast(), dst.add(bytes).cast());
            self.used = start + bytes;
            Some(dst)
        }
    }
}

impl Drop for CodeBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        unsafe {
            munmap(self.base.cast(), self.size);
        }
    }
}
