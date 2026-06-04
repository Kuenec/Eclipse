//! Graphics: forward, don't render (component-map F · 🟢 winit / 🟡 ash·egl).
//!
//! Eclipse is **not** a renderer. Roblox's native engine issues its own Vulkan/GLES; this
//! module provides the `libvulkan.so`/`libEGL.so`/`libGLESv2.so` the engine links and
//! **forwards** those calls to the host driver, **translating WSI** (Android
//! `vkCreateAndroidSurfaceKHR` / `ANativeWindow` → host Wayland/X11 surface). Vulkan is
//! preferred; GL is the fallback. Capability is detected at runtime (never assume a vendor).
//!
//! Planned deps: `ash` (+ `ash-window`, `raw-window-handle`), `khronos-egl`, `winit` (the
//! game window/surface + kbd/mouse). Optional `gbm`/`drm` for DMA-BUF buffer interop.
//! TODO(M4): Vulkan loader shim + WSI translation; GL fallback path.
