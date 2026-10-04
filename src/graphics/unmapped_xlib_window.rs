use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

type XOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type XDefaultRootWindow = unsafe extern "C" fn(*mut c_void) -> c_ulong;
type XCreateSimpleWindow = unsafe extern "C" fn(
    *mut c_void,
    c_ulong,
    c_int,
    c_int,
    c_uint,
    c_uint,
    c_uint,
    c_ulong,
    c_ulong,
) -> c_ulong;
type XSync = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type XDefaultScreen = unsafe extern "C" fn(*mut c_void) -> c_int;
type XDefaultVisual = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type XVisualIDFromVisual = unsafe extern "C" fn(*mut c_void) -> c_ulong;
type XDestroyWindow = unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int;
type XCloseDisplay = unsafe extern "C" fn(*mut c_void) -> c_int;

pub(crate) struct UnmappedXlibWindow {
    pub(crate) display: *mut c_void,
    pub(crate) screen: c_int,
    pub(crate) window: c_ulong,
    pub(crate) visual_id: c_ulong,
    destroy_window: XDestroyWindow,
    close_display: XCloseDisplay,
    _lib: libloading::Library,
}

impl UnmappedXlibWindow {
    pub(crate) fn open(width: c_uint, height: c_uint) -> Result<Self, String> {
        let lib = unsafe { libloading::Library::new("libX11.so.6") }.map_err(|e| e.to_string())?;
        let symbol = |name: &[u8]| -> Result<*const (), String> {
            unsafe { lib.get::<*const ()>(name) }
                .map(|s| *s)
                .map_err(|e| e.to_string())
        };
        let (open_display, default_root, create_window, sync, destroy_window, close_display) = unsafe {
            (
                std::mem::transmute::<*const (), XOpenDisplay>(symbol(b"XOpenDisplay\0")?),
                std::mem::transmute::<*const (), XDefaultRootWindow>(symbol(
                    b"XDefaultRootWindow\0",
                )?),
                std::mem::transmute::<*const (), XCreateSimpleWindow>(symbol(
                    b"XCreateSimpleWindow\0",
                )?),
                std::mem::transmute::<*const (), XSync>(symbol(b"XSync\0")?),
                std::mem::transmute::<*const (), XDestroyWindow>(symbol(b"XDestroyWindow\0")?),
                std::mem::transmute::<*const (), XCloseDisplay>(symbol(b"XCloseDisplay\0")?),
            )
        };
        let (default_screen, default_visual, visual_id_of) = unsafe {
            (
                std::mem::transmute::<*const (), XDefaultScreen>(symbol(b"XDefaultScreen\0")?),
                std::mem::transmute::<*const (), XDefaultVisual>(symbol(b"XDefaultVisual\0")?),
                std::mem::transmute::<*const (), XVisualIDFromVisual>(symbol(
                    b"XVisualIDFromVisual\0",
                )?),
            )
        };
        let display = unsafe { open_display(std::ptr::null()) };
        if display.is_null() {
            return Err("XOpenDisplay(NULL) failed".into());
        }
        let (screen, window, visual_id) = unsafe {
            let screen = default_screen(display);
            let window =
                create_window(display, default_root(display), 0, 0, width, height, 0, 0, 0);
            sync(display, 0);
            (
                screen,
                window,
                visual_id_of(default_visual(display, screen)),
            )
        };
        Ok(Self {
            display,
            screen,
            window,
            visual_id,
            destroy_window,
            close_display,
            _lib: lib,
        })
    }
}

impl Drop for UnmappedXlibWindow {
    fn drop(&mut self) {
        unsafe {
            (self.destroy_window)(self.display, self.window);
            (self.close_display)(self.display);
        }
    }
}
