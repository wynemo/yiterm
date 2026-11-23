//! Owned by the listener, not a viewer connection. Reconnecting preserves the
//! virtual screen and its windows until the user stops sharing/exits YiTerm.
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn yiterm_virtual_display_create(w: u32, h: u32, scale: u32) -> *mut c_void;
    fn yiterm_virtual_display_id(handle: *mut c_void) -> u32;
    fn yiterm_virtual_display_resize(handle: *mut c_void, w: u32, h: u32, scale: u32) -> bool;
    fn yiterm_virtual_display_destroy(handle: *mut c_void);
}

pub(super) struct VirtualDesktop(NonNull<c_void>);
impl VirtualDesktop {
    pub(super) fn create(size: [u16; 3]) -> Result<Self> {
        let [w, h, scale] = size.map(u32::from);
        let handle = unsafe { yiterm_virtual_display_create(w, h, scale) };
        Ok(Self(NonNull::new(handle).context(
            "Cannot create the iPad virtual display on this Mac; physical display sharing was not started",
        )?))
    }
    pub(super) fn id(&self) -> Result<u32> {
        let id = unsafe { yiterm_virtual_display_id(self.0.as_ptr()) };
        ensure!(id != 0, "The iPad virtual display is unavailable");
        Ok(id)
    }
    pub(super) fn resize(&mut self, size: [u16; 3]) -> Result<()> {
        let [w, h, scale] = size.map(u32::from);
        ensure!(unsafe { yiterm_virtual_display_resize(self.0.as_ptr(), w, h, scale) },
                "Cannot resize the iPad virtual display; physical displays were not changed");
        Ok(())
    }
}
impl Drop for VirtualDesktop {
    fn drop(&mut self) { unsafe { yiterm_virtual_display_destroy(self.0.as_ptr()) }; }
}
