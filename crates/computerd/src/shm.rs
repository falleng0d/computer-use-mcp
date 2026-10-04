//! Shared memory segment that the X server fills with screen pixels.

use std::{io, ptr::NonNull};

/// A shared memory segment attached to this process.
pub(crate) struct Segment {
    id: i32,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the segment is plain memory owned by this value. The pointer is only
// dereferenced through `&self` or `&mut self` methods, so moving the value to
// another thread cannot create unsynchronized access from this process.
unsafe impl Send for Segment {}

// SAFETY: methods taking `&self` never touch the mapped memory, and the only
// access to it needs `&mut self`, so shared references across threads are harmless.
unsafe impl Sync for Segment {}

impl Segment {
    /// Creates a private segment of `len` bytes and attaches it.
    pub(crate) fn new(len: usize) -> io::Result<Self> {
        // SAFETY: shmget takes plain integers and returns an id or -1.
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `id` names a segment just created; a null address lets the kernel choose.
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if addr as isize == -1 {
            let error = io::Error::last_os_error();
            // SAFETY: `id` is a valid segment id; IPC_RMID ignores the buffer argument.
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(error);
        }
        let ptr = NonNull::new(addr.cast::<u8>()).expect("shmat returned a non-null address");
        Ok(Self { id, ptr, len })
    }

    /// The id the X server needs to attach the segment.
    pub(crate) fn id(&self) -> u32 {
        u32::try_from(self.id).expect("shmget returns a non-negative id")
    }

    /// Deletes the segment once every process has detached. Call after the X server attached.
    pub(crate) fn remove_on_detach(&self) -> io::Result<()> {
        // SAFETY: `self.id` is a valid segment id; IPC_RMID ignores the buffer argument.
        let result = unsafe { libc::shmctl(self.id, libc::IPC_RMID, std::ptr::null_mut()) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The segment contents. The X server only writes during a request that the
    /// caller makes, and the caller holds `&mut self` while it does.
    pub(crate) fn bytes(&mut self) -> &[u8] {
        // SAFETY: `ptr` points to `len` attached bytes that stay mapped until drop.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from shmat and is detached once, here. `id` is the segment's
        // id, and removing it again after an earlier removal fails harmlessly.
        unsafe {
            libc::shmdt(self.ptr.as_ptr().cast_const().cast());
            libc::shmctl(self.id, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}
