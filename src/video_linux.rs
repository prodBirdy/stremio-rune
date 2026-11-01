//! Where the player's frames come from on Linux: mpv renders each frame on the GPU (the libmpv
//! render API, OpenGL on EGL) straight into a GBM buffer, a dmabuf that Rune imports into Vulkan
//! and draws as a `NativeSurface` element. No pixel touches the CPU and nothing is copied: GL
//! renders into the dmabuf's memory, and Vulkan samples that same memory.
//!
//! The same seam as `video.rs` and `video_mac.rs`: the view only knows the surface, and the
//! player only `start`s a source, gives it a size and drops it.
//!
//! The work runs on a thread of its own, which owns an EGL context on the GBM device of the GPU
//! (mpv's render API wants its GL context current on the thread that renders) and a few buffers.
//! mpv's update callback only wakes that thread. Each frame: take a buffer Rune no longer holds
//! (Rune holds each frame it shows until the GPU is done with it), let mpv render into it, wait on
//! a GL fence until that rendering completes, and push it.

use crate::player::sym;
use rune::NativeSurface;
use rune::native_surface::ColorInfo;
use rune::native_surface::linux::{DRM_FORMAT_ABGR8888, DmaBuf};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// mpv_render_param_type values (render.h, render_gl.h).
const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const PARAM_OPENGL_FBO: c_int = 3;
const PARAM_FLIP_Y: c_int = 4;
const PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
/// mpv_render_update_flag: a new frame should be rendered.
const UPDATE_FRAME: u64 = 1;

// EGL.
const EGL_NONE: i32 = 0x3038;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_BIT: i32 = 0x0008;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 1;
const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
/// `GBM_BO_USE_RENDERING`.
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
/// `DRM_FORMAT_MOD_LINEAR`.
const MOD_LINEAR: u64 = 0;
/// Buffers in flight at most: one on screen, one queued, one being drawn, a spare.
const POOL_MAX: usize = 4;

// GL.
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_RGBA: u32 = 0x1908;
const GL_RGBA8: u32 = 0x8058;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const GL_SYNC_FLUSH_COMMANDS_BIT: u32 = 1;
const GL_TIMEOUT_EXPIRED: u32 = 0x911B;
const GL_WAIT_FAILED: u32 = 0x911D;

type Ptr = *mut c_void;

/// EGL and the GL entry points this file calls, resolved once.
struct Gl {
    _egl: libloading::Library,
    get_proc: unsafe extern "C" fn(*const c_char) -> Ptr,
    get_display: unsafe extern "C" fn(Ptr) -> Ptr,
    get_platform_display: Option<unsafe extern "C" fn(u32, Ptr, *const isize) -> Ptr>,
    initialize: unsafe extern "C" fn(Ptr, *mut i32, *mut i32) -> u32,
    bind_api: unsafe extern "C" fn(u32) -> u32,
    choose_config: unsafe extern "C" fn(Ptr, *const i32, *mut Ptr, i32, *mut i32) -> u32,
    create_context: unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr,
    destroy_context: unsafe extern "C" fn(Ptr, Ptr) -> u32,
    make_current: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32,
    get_error: unsafe extern "C" fn() -> i32,
    gen_textures: unsafe extern "C" fn(i32, *mut u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    delete_textures: unsafe extern "C" fn(i32, *const u32),
    tex_image_2d: unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    pixel_store_i: unsafe extern "C" fn(u32, i32),
    fence_sync: unsafe extern "C" fn(u32, u32) -> Ptr,
    client_wait_sync: unsafe extern "C" fn(Ptr, u32, u64) -> u32,
    delete_sync: unsafe extern "C" fn(Ptr),
    create_image: unsafe extern "C" fn(Ptr, Ptr, u32, Ptr, *const i32) -> Ptr,
    destroy_image: unsafe extern "C" fn(Ptr, Ptr) -> u32,
    image_target_texture: unsafe extern "C" fn(u32, Ptr),
    gbm: Gbm,
}

/// libgbm: the GPU's buffers, shareable as dmabufs.
struct Gbm {
    _lib: libloading::Library,
    create_device: unsafe extern "C" fn(c_int) -> Ptr,
    device_destroy: unsafe extern "C" fn(Ptr),
    bo_create: unsafe extern "C" fn(Ptr, u32, u32, u32, u32) -> Ptr,
    bo_create_with_modifiers2: unsafe extern "C" fn(Ptr, u32, u32, u32, *const u64, u32, u32) -> Ptr,
    bo_get_fd: unsafe extern "C" fn(Ptr) -> c_int,
    bo_get_modifier: unsafe extern "C" fn(Ptr) -> u64,
    bo_get_stride_for_plane: unsafe extern "C" fn(Ptr, c_int) -> u32,
    bo_get_offset: unsafe extern "C" fn(Ptr, c_int) -> u32,
    bo_destroy: unsafe extern "C" fn(Ptr),
}
unsafe impl Send for Gl {}
unsafe impl Sync for Gl {}

fn gl() -> Result<&'static Gl, String> {
    static GL: OnceLock<Result<Gl, String>> = OnceLock::new();
    GL.get_or_init(|| unsafe {
        let egl = libloading::Library::new("libEGL.so.1").map_err(|e| format!("Couldn't load libEGL ({e})"))?;
        macro_rules! egl { ($n:literal) => { *egl.get($n).map_err(|e| format!("libEGL: {e}"))? } }
        let get_proc: unsafe extern "C" fn(*const c_char) -> Ptr = egl!(b"eglGetProcAddress\0");
        macro_rules! glf { ($n:literal) => {{
            let p = get_proc($n.as_ptr());
            if p.is_null() { return Err(format!("GL function {} missing", $n.to_string_lossy())) }
            std::mem::transmute::<Ptr, _>(p)
        }} }
        let gpd = get_proc(c"eglGetPlatformDisplayEXT".as_ptr());
        Ok(Gl {
            get_display: egl!(b"eglGetDisplay\0"),
            get_platform_display: (!gpd.is_null()).then(|| std::mem::transmute::<Ptr, _>(gpd)),
            initialize: egl!(b"eglInitialize\0"),
            bind_api: egl!(b"eglBindAPI\0"),
            choose_config: egl!(b"eglChooseConfig\0"),
            create_context: egl!(b"eglCreateContext\0"),
            destroy_context: egl!(b"eglDestroyContext\0"),
            make_current: egl!(b"eglMakeCurrent\0"),
            get_error: egl!(b"eglGetError\0"),
            gen_textures: glf!(c"glGenTextures"),
            bind_texture: glf!(c"glBindTexture"),
            delete_textures: glf!(c"glDeleteTextures"),
            tex_image_2d: glf!(c"glTexImage2D"),
            gen_framebuffers: glf!(c"glGenFramebuffers"),
            bind_framebuffer: glf!(c"glBindFramebuffer"),
            framebuffer_texture_2d: glf!(c"glFramebufferTexture2D"),
            check_framebuffer_status: glf!(c"glCheckFramebufferStatus"),
            delete_framebuffers: glf!(c"glDeleteFramebuffers"),
            read_pixels: glf!(c"glReadPixels"),
            pixel_store_i: glf!(c"glPixelStorei"),
            fence_sync: glf!(c"glFenceSync"),
            client_wait_sync: glf!(c"glClientWaitSync"),
            delete_sync: glf!(c"glDeleteSync"),
            create_image: glf!(c"eglCreateImageKHR"),
            destroy_image: glf!(c"eglDestroyImageKHR"),
            image_target_texture: glf!(c"glEGLImageTargetTexture2DOES"),
            gbm: {
                let lib = libloading::Library::new("libgbm.so.1").map_err(|e| format!("Couldn't load libgbm ({e})"))?;
                macro_rules! gbm { ($n:literal) => { *lib.get($n).map_err(|e| format!("libgbm: {e}"))? } }
                Gbm {
                    create_device: gbm!(b"gbm_create_device\0"),
                    device_destroy: gbm!(b"gbm_device_destroy\0"),
                    bo_create: gbm!(b"gbm_bo_create\0"),
                    bo_create_with_modifiers2: gbm!(b"gbm_bo_create_with_modifiers2\0"),
                    bo_get_fd: gbm!(b"gbm_bo_get_fd\0"),
                    bo_get_modifier: gbm!(b"gbm_bo_get_modifier\0"),
                    bo_get_stride_for_plane: gbm!(b"gbm_bo_get_stride_for_plane\0"),
                    bo_get_offset: gbm!(b"gbm_bo_get_offset\0"),
                    bo_destroy: gbm!(b"gbm_bo_destroy\0"),
                    _lib: lib,
                }
            },
            get_proc,
            _egl: egl,
        })
    }).as_ref().map_err(Clone::clone)
}

/// An EGL OpenGL 3.3 core context on the GPU's GBM device, current on this thread. Rune draws
/// with the same GPU (one render node: `STREMIO_RUNE_DRM` picks another).
struct Context { display: Ptr, ctx: Ptr, gbm: Ptr, _node: std::fs::File }

impl Context {
    fn new() -> Result<Context, String> {
        let g = gl()?;
        unsafe {
            use std::os::fd::AsRawFd;
            let path = std::env::var_os("STREMIO_RUNE_DRM").map(std::path::PathBuf::from).or_else(|| {
                let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri").ok()?.flatten().map(|e| e.path())
                    .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD"))).collect();
                nodes.sort();
                nodes.into_iter().next()
            }).ok_or("no GPU render node in /dev/dri")?;
            let node = std::fs::OpenOptions::new().read(true).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let gbm = (g.gbm.create_device)(node.as_raw_fd());
            if gbm.is_null() { return Err(format!("gbm_create_device failed on {}", path.display())) }
            let fail = |e: String| { (g.gbm.device_destroy)(gbm); Err(e) };
            let display = match g.get_platform_display { Some(f) => f(EGL_PLATFORM_GBM_KHR, gbm, std::ptr::null()), None => (g.get_display)(gbm) };
            let (mut major, mut minor) = (0, 0);
            if display.is_null() || (g.initialize)(display, &mut major, &mut minor) == 0 {
                return fail(format!("eglInitialize on GBM failed ({:#x})", (g.get_error)()));
            }
            if (g.bind_api)(EGL_OPENGL_API) == 0 { return fail("EGL has no desktop OpenGL".into()) }
            let attrs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_SURFACE_TYPE, 0, EGL_NONE];
            let (mut config, mut n) = (std::ptr::null_mut(), 0);
            if (g.choose_config)(display, attrs.as_ptr(), &mut config, 1, &mut n) == 0 || n < 1 {
                return fail(format!("no EGL config for OpenGL ({:#x})", (g.get_error)()));
            }
            let attrs = [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
                EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE];
            let ctx = (g.create_context)(display, config, std::ptr::null_mut(), attrs.as_ptr());
            if ctx.is_null() { return fail(format!("eglCreateContext failed ({:#x})", (g.get_error)())) }
            if (g.make_current)(display, std::ptr::null_mut(), std::ptr::null_mut(), ctx) == 0 {
                (g.destroy_context)(display, ctx);
                return fail(format!("eglMakeCurrent failed ({:#x})", (g.get_error)()));
            }
            Ok(Context { display, ctx, gbm, _node: node })
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Ok(g) = gl() {
            unsafe {
                (g.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                (g.destroy_context)(self.display, self.ctx);
                (g.gbm.device_destroy)(self.gbm);
            }
        }
    }
}

/// An RGBA8 texture with a framebuffer onto it.
struct Target { fbo: u32, tex: u32, size: (u32, u32) }

impl Target {
    fn new(g: &Gl, w: u32, h: u32) -> Result<Target, String> {
        unsafe {
            let mut t = Target { fbo: 0, tex: 0, size: (w, h) };
            (g.gen_textures)(1, &mut t.tex);
            (g.bind_texture)(GL_TEXTURE_2D, t.tex);
            (g.tex_image_2d)(GL_TEXTURE_2D, 0, GL_RGBA8 as i32, w as i32, h as i32, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
            (g.bind_texture)(GL_TEXTURE_2D, 0);
            (g.gen_framebuffers)(1, &mut t.fbo);
            (g.bind_framebuffer)(GL_FRAMEBUFFER, t.fbo);
            (g.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, t.tex, 0);
            if (g.check_framebuffer_status)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                t.free(g);
                return Err("video framebuffer incomplete".into());
            }
            Ok(t)
        }
    }

    /// Read the framebuffer back, top row first (mpv draws into an FBO top-down unflipped).
    fn read(&self, g: &Gl, format: u32) -> Vec<u8> {
        let (w, h) = self.size;
        let mut px = vec![0u8; w as usize * h as usize * 4];
        unsafe {
            (g.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (g.pixel_store_i)(GL_PACK_ALIGNMENT, 1);
            (g.read_pixels)(0, 0, w as i32, h as i32, format, GL_UNSIGNED_BYTE, px.as_mut_ptr().cast());
        }
        px
    }

    fn free(&mut self, g: &Gl) {
        unsafe {
            (g.delete_framebuffers)(1, &self.fbo);
            (g.delete_textures)(1, &self.tex);
        }
        (self.fbo, self.tex) = (0, 0);
    }
}

#[repr(C)]
struct Param { ty: c_int, data: *mut c_void }
#[repr(C)]
struct GlInit { get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void, ctx: *mut c_void, extra_exts: *const c_char }
#[repr(C)]
struct Fbo { fbo: c_int, w: c_int, h: c_int, internal_format: c_int }

unsafe extern "C" fn get_proc_address(_: *mut c_void, name: *const c_char) -> *mut c_void {
    gl().map_or(std::ptr::null_mut(), |g| unsafe { (g.get_proc)(name) })
}

/// mpv's OpenGL renderer for core `mpv`, on the GL context current on this thread.
unsafe fn mpv_gl(mpv: *mut c_void) -> Result<*mut c_void, String> {
    let create: unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *mut Param) -> c_int = sym(b"mpv_render_context_create\0")?;
    let mut init = GlInit { get_proc_address, ctx: std::ptr::null_mut(), extra_exts: std::ptr::null() };
    let mut params = [
        Param { ty: PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
        Param { ty: PARAM_OPENGL_INIT_PARAMS, data: (&raw mut init).cast() },
        Param { ty: PARAM_INVALID, data: std::ptr::null_mut() },
    ];
    let mut render = std::ptr::null_mut();
    if unsafe { create(&mut render, mpv, params.as_mut_ptr()) } < 0 || render.is_null() {
        return Err("mpv's OpenGL renderer didn't start".into());
    }
    Ok(render)
}

/// Draw mpv's current frame into `t`.
fn draw_into(render: *mut c_void, t: &Target, block: bool) -> bool {
    let Ok(draw) = sym::<unsafe extern "C" fn(*mut c_void, *mut Param) -> c_int>(b"mpv_render_context_render\0") else { return false };
    let mut fbo = Fbo { fbo: t.fbo as c_int, w: t.size.0 as c_int, h: t.size.1 as c_int, internal_format: GL_RGBA8 as c_int };
    let (mut flip, mut block): (c_int, c_int) = (0, block as c_int);
    let mut params = [
        Param { ty: PARAM_OPENGL_FBO, data: (&raw mut fbo).cast() },
        Param { ty: PARAM_FLIP_Y, data: (&raw mut flip).cast() },
        Param { ty: PARAM_BLOCK_FOR_TARGET_TIME, data: (&raw mut block).cast() },
        Param { ty: PARAM_INVALID, data: std::ptr::null_mut() },
    ];
    unsafe { draw(render, params.as_mut_ptr()) >= 0 }
}

fn free_render(render: *mut c_void) {
    if !render.is_null() && let Ok(free) = sym::<unsafe extern "C" fn(*mut c_void)>(b"mpv_render_context_free\0") { unsafe { free(render) } }
}

/// Rendering counters, for measuring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    /// CPU time on the render thread per frame, in total, readback included.
    pub render_ms: f64,
    /// Frames that failed to draw or push.
    pub starved: u64,
    pub size: (u32, u32),
}

struct Shared {
    /// Target size in device pixels, `w << 32 | h`; 0 until the app lays the video out.
    size: AtomicU64,
    quit: AtomicBool,
    stats: Mutex<Stats>,
    error: Mutex<Option<String>>,
}

/// Frames from mpv into `surface`. Drop it before the mpv core it was started on.
pub struct FrameSource {
    kick: SyncSender<()>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Core(*mut c_void);
unsafe impl Send for Core {}

unsafe extern "C" fn on_update(data: *mut c_void) {
    // Never render here (mpv's thread, render API rules): wake the render thread instead.
    let _ = unsafe { &*(data as *const SyncSender<()>) }.try_send(());
}

impl FrameSource {
    /// Attach mpv's OpenGL renderer to `mpv` (an initialized core with `vo=libmpv`, before any
    /// file is loaded) and start pushing frames into `surface`. `load` runs on the render thread
    /// once the renderer is up. If it can't be set up, `error` says why.
    pub fn start(mpv: *mut c_void, surface: NativeSurface, load: impl FnOnce() + Send + 'static) -> FrameSource {
        let (kick, wake) = sync_channel(1);
        let shared = Arc::new(Shared { size: AtomicU64::new(0), quit: AtomicBool::new(false), stats: Mutex::new(Stats::default()),
            error: Mutex::new(None) });
        let (core, sh, k) = (Core(mpv), shared.clone(), kick.clone());
        let thread = std::thread::Builder::new().name("mpv-render".into()).spawn(move || {
            let core = core;
            match Renderer::new(core.0, k) {
                Ok(mut r) => {
                    load();
                    r.run(wake, sh, surface);
                }
                Err(e) => *sh.error.lock().unwrap() = Some(e),
            }
        });
        if let Err(e) = &thread { *shared.error.lock().unwrap() = Some(e.to_string()) }
        FrameSource { kick, shared, thread: thread.ok() }
    }

    pub fn error(&self) -> Option<String> { self.shared.error.lock().unwrap().clone() }

    /// Render at this many device pixels from the next frame (and redraw the current one).
    pub fn set_size(&self, w: u32, h: u32) {
        let v = (w as u64) << 32 | h as u64;
        if self.shared.size.swap(v, Ordering::Relaxed) != v { let _ = self.kick.try_send(()); }
    }

    pub fn stats(&self) -> Stats { *self.shared.stats.lock().unwrap() }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        let _ = self.kick.try_send(());
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

/// One GBM buffer, as Rune sees it (`buf`) and as GL renders into it (an EGLImage on a texture,
/// and a framebuffer onto that).
struct Slot { bo: Ptr, image: Ptr, target: Target, buf: DmaBuf }

impl Slot {
    fn new(g: &Gl, ctx: &Context, w: u32, h: u32, linear: bool) -> Result<Slot, String> {
        unsafe {
            let bo = if linear {
                (g.gbm.bo_create_with_modifiers2)(ctx.gbm, w, h, DRM_FORMAT_ABGR8888, &MOD_LINEAR, 1, GBM_BO_USE_RENDERING)
            } else {
                (g.gbm.bo_create)(ctx.gbm, w, h, DRM_FORMAT_ABGR8888, GBM_BO_USE_RENDERING)
            };
            if bo.is_null() { return Err(format!("gbm_bo_create {w}x{h} failed")) }
            let fd = (g.gbm.bo_get_fd)(bo);
            if fd < 0 { (g.gbm.bo_destroy)(bo); return Err("gbm_bo_get_fd failed".into()) }
            let fd = OwnedFd::from_raw_fd(fd);
            let (modifier, stride, offset) = ((g.gbm.bo_get_modifier)(bo), (g.gbm.bo_get_stride_for_plane)(bo, 0), (g.gbm.bo_get_offset)(bo, 0));
            use std::os::fd::AsRawFd;
            let attrs = [
                EGL_WIDTH, w as i32, EGL_HEIGHT, h as i32, EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_ABGR8888 as i32,
                EGL_DMA_BUF_PLANE0_FD_EXT, fd.as_raw_fd(), EGL_DMA_BUF_PLANE0_OFFSET_EXT, offset as i32,
                EGL_DMA_BUF_PLANE0_PITCH_EXT, stride as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, modifier as u32 as i32, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (modifier >> 32) as u32 as i32,
                EGL_NONE,
            ];
            let image = (g.create_image)(ctx.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attrs.as_ptr());
            if image.is_null() { (g.gbm.bo_destroy)(bo); return Err(format!("eglCreateImage (dmabuf, modifier {modifier:#x}) failed ({:#x})", (g.get_error)())) }
            let mut target = Target { fbo: 0, tex: 0, size: (w, h) };
            (g.gen_textures)(1, &mut target.tex);
            (g.bind_texture)(GL_TEXTURE_2D, target.tex);
            (g.image_target_texture)(GL_TEXTURE_2D, image);
            (g.bind_texture)(GL_TEXTURE_2D, 0);
            (g.gen_framebuffers)(1, &mut target.fbo);
            (g.bind_framebuffer)(GL_FRAMEBUFFER, target.fbo);
            (g.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target.tex, 0);
            let complete = (g.check_framebuffer_status)(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
            (g.bind_framebuffer)(GL_FRAMEBUFFER, 0);
            let buf = DmaBuf::from_fd(fd.as_fd(), [w, h], DRM_FORMAT_ABGR8888, modifier, stride, offset, ColorInfo::default());
            let buf = match buf {
                Ok(b) => b,
                Err(e) => { target.free(g); (g.destroy_image)(ctx.display, image); (g.gbm.bo_destroy)(bo); return Err(e) }
            };
            let mut slot = Slot { bo, image, target, buf };
            if !complete { slot.free(g, ctx); return Err(format!("dmabuf framebuffer incomplete (modifier {modifier:#x})")) }
            Ok(slot)
        }
    }

    fn free(&mut self, g: &Gl, ctx: &Context) {
        self.target.free(g);
        unsafe {
            (g.destroy_image)(ctx.display, self.image);
            (g.gbm.bo_destroy)(self.bo);
        }
    }
}

struct Renderer {
    // Field order is drop order: mpv's renderer and the slots (in Drop) go before the context.
    render: *mut c_void,
    kick: *mut SyncSender<()>,
    /// Buffers of the current size; one Rune no longer holds is drawn into next.
    slots: Vec<Slot>,
    /// Buffers of an old size Rune may still hold, freed once it lets go.
    retired: Vec<Slot>,
    size: (u32, u32),
    /// Linear buffers, after the GPU's own layout failed (or `STREMIO_RUNE_DMABUF_LINEAR`).
    linear: bool,
    frame_id: u64,
    ctx: Context,
}

impl Renderer {
    fn new(mpv: *mut c_void, kick: SyncSender<()>) -> Result<Renderer, String> {
        let ctx = Context::new()?;
        let set_update: unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void), *mut c_void) =
            sym(b"mpv_render_context_set_update_callback\0")?;
        let render = unsafe { mpv_gl(mpv)? };
        let kick = Box::into_raw(Box::new(kick));
        unsafe { set_update(render, on_update, kick.cast()) };
        let linear = std::env::var_os("STREMIO_RUNE_DMABUF_LINEAR").is_some();
        Ok(Renderer { render, kick, slots: Vec::new(), retired: Vec::new(), size: (0, 0), linear, frame_id: 0, ctx })
    }

    fn resize(&mut self, g: &Gl, w: u32, h: u32) {
        self.retired.append(&mut self.slots);
        self.size = (w, h);
        self.reap(g);
    }

    /// Free retired buffers Rune has let go of.
    fn reap(&mut self, g: &Gl) {
        let (ctx, mut keep) = (&self.ctx, Vec::new());
        for mut s in self.retired.drain(..) { if s.buf.in_use() { keep.push(s) } else { s.free(g, ctx) } }
        self.retired = keep;
    }

    /// A buffer Rune doesn't hold, waiting up to `limit` for one to come back.
    fn take(&mut self, g: &Gl, limit: Duration) -> Result<Option<usize>, String> {
        let until = Instant::now() + limit;
        loop {
            if let Some(i) = self.slots.iter().position(|s| !s.buf.in_use()) { return Ok(Some(i)) }
            if self.slots.len() < POOL_MAX {
                let (w, h) = self.size;
                let slot = match Slot::new(g, &self.ctx, w, h, self.linear) {
                    Err(e) if !self.linear => {
                        eprintln!("video: {e}; trying linear buffers");
                        self.linear = true;
                        Slot::new(g, &self.ctx, w, h, true)?
                    }
                    r => r?,
                };
                self.slots.push(slot);
                return Ok(Some(self.slots.len() - 1));
            }
            if Instant::now() >= until { return Ok(None) }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn run(&mut self, wake: Receiver<()>, shared: Arc<Shared>, surface: NativeSurface) {
        let Ok(update) = sym::<unsafe extern "C" fn(*mut c_void) -> u64>(b"mpv_render_context_update\0") else { return };
        let Ok(g) = gl() else { return };
        while wake.recv().is_ok() && !shared.quit.load(Ordering::Relaxed) {
            let flags = unsafe { update(self.render) };
            let v = shared.size.load(Ordering::Relaxed);
            let (w, h) = ((v >> 32) as u32, v as u32);
            if w == 0 || h == 0 { continue }
            let resized = (w, h) != self.size;
            if flags & UPDATE_FRAME == 0 && !resized { continue }
            if resized { self.resize(g, w, h) } else if !self.retired.is_empty() { self.reap(g) }
            let t0 = Instant::now();
            let i = match self.take(g, Duration::from_millis(50)) {
                Ok(Some(i)) => i,
                Ok(None) => { shared.stats.lock().unwrap().starved += 1; continue }
                Err(e) => { *shared.error.lock().unwrap() = Some(e.clone()); eprintln!("video: {e}"); continue }
            };
            if !draw_into(self.render, &self.slots[i].target, true) {
                shared.stats.lock().unwrap().starved += 1;
                continue;
            }
            // Vulkan doesn't wait for GL: the frame goes out once GL has finished it.
            unsafe {
                let fence = (g.fence_sync)(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                let waited = (g.client_wait_sync)(fence, GL_SYNC_FLUSH_COMMANDS_BIT, 100_000_000);
                (g.delete_sync)(fence);
                if waited == GL_TIMEOUT_EXPIRED || waited == GL_WAIT_FAILED {
                    shared.stats.lock().unwrap().starved += 1;
                    continue;
                }
            }
            self.frame_id += 1;
            if let Err(e) = surface.push(self.slots[i].buf.clone(), self.frame_id) {
                eprintln!("video: {e}");
                continue;
            }
            // Rune couldn't import the GPU's own layout: start over with linear buffers.
            if self.frame_id == 3 && !self.linear && let Some(e) = surface.last_error() {
                eprintln!("video: {e}; trying linear buffers");
                self.linear = true;
                self.retired.append(&mut self.slots);
            }
            let mut s = shared.stats.lock().unwrap();
            s.render_ms += t0.elapsed().as_secs_f64() * 1000.0;
            (s.frames, s.size) = (self.frame_id, (w, h));
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // mpv's renderer first, with the GL context current; then no more callbacks.
        free_render(self.render);
        unsafe { drop(Box::from_raw(self.kick)) };
        // Rune keeps its imports alive through its own dmabuf fds; GL's side can go now.
        if let Ok(g) = gl() {
            for mut s in self.slots.drain(..).chain(self.retired.drain(..)) { s.free(g, &self.ctx) }
        }
    }
}

/// mpv's frames drawn small, offscreen, and read back: seek-bar previews (preview.rs). The same
/// renderer as the player's draws them, so they look the same.
pub struct Snapshot {
    render: *mut c_void,
    target: Target,
    _ctx: Context,
}

impl Snapshot {
    /// mpv's renderer for core `mpv` (before it loads a file), drawing at `w`x`h`.
    pub fn new(mpv: *mut c_void, w: u32, h: u32) -> Result<Snapshot, String> {
        let ctx = Context::new()?;
        let target = Target::new(gl()?, w, h)?;
        let render = unsafe { mpv_gl(mpv)? };
        Ok(Snapshot { render, target, _ctx: ctx })
    }

    /// Draw mpv's frame if it has a new one; true if it did.
    pub fn pump(&mut self) -> bool {
        let Ok(update) = sym::<unsafe extern "C" fn(*mut c_void) -> u64>(b"mpv_render_context_update\0") else { return false };
        if unsafe { update(self.render) } & UPDATE_FRAME == 0 { return false }
        draw_into(self.render, &self.target, false)
    }

    /// The frame drawn last, as RGBA.
    pub fn grab(&self) -> Option<image::RgbaImage> {
        let g = gl().ok()?;
        let (w, h) = self.target.size;
        image::RgbaImage::from_raw(w, h, self.target.read(g, GL_RGBA))
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        free_render(self.render);
        if let Ok(g) = gl() { self.target.free(g) }
    }
}
