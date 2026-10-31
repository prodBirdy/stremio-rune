//! Where the player's frames come from on macOS: mpv renders each frame on the GPU (the libmpv
//! render API, OpenGL through CGL) straight into an IOSurface-backed `CVPixelBuffer` that Rune
//! draws as a `NativeSurface` element on its Metal device. No pixel touches the CPU, and nothing
//! is copied: GL renders into the IOSurface's own memory, and Metal samples that same memory.
//!
//! The same seam as `video.rs` on Windows: the view only knows the surface, and the player only
//! `start`s a source, gives it a size and drops it.
//!
//! The work runs on a thread of its own, which owns an offscreen CGL context (mpv's render API
//! wants its GL context current on the thread that renders) and a CoreVideo buffer pool. mpv's
//! update callback only wakes that thread. Each frame: take a buffer from the pool (the pool hands
//! out only buffers nobody holds any more, and Rune holds each frame it shows until the GPU is
//! done with it), let mpv render into it, wait on a GL fence until that rendering completes, and
//! push it.

use crate::player::sym;
use rune::NativeSurface;
use rune::native_surface::macos::PixelBuffer;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// mpv_render_param_type values (render.h, render_gl.h).
const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const PARAM_OPENGL_FBO: c_int = 3;
const PARAM_FLIP_Y: c_int = 4;
/// mpv_render_update_flag: a new frame should be rendered.
const UPDATE_FRAME: u64 = 1;
/// Buffers the pool keeps, and the most it makes: one on screen, one being drawn, spares.
const POOL_MIN: i32 = 3;
const POOL_MAX: i32 = 4;

// CGL pixel format attributes (CGLTypes.h).
const CGL_PFA_ACCELERATED: i32 = 73;
const CGL_PFA_ALLOW_OFFLINE_RENDERERS: i32 = 96;
const CGL_PFA_OPENGL_PROFILE: i32 = 99;
const CGL_OGLP_VERSION_3_2_CORE: i32 = 0x3200;

// GL enums.
const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_RGBA: u32 = 0x1908;
const GL_RGBA8: u32 = 0x8058;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const GL_SYNC_FLUSH_COMMANDS_BIT: u32 = 1;
const GL_TIMEOUT_EXPIRED: u32 = 0x911B;
const GL_WAIT_FAILED: u32 = 0x911D;

/// `kCVPixelFormatType_32BGRA`, the format Rune draws without conversion.
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");
/// `kCFNumberSInt32Type`.
const CF_SINT32: isize = 3;
/// `kCVReturnWouldExceedAllocationThreshold`: every buffer the pool may make is still in use.
const CV_WOULD_EXCEED: i32 = -6689;

type CFType = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFType);
    fn CFNumberCreate(alloc: CFType, ty: isize, value: *const c_void) -> CFType;
    fn CFDictionaryCreate(alloc: CFType, keys: *const CFType, values: *const CFType, n: isize, key_cb: *const c_void, value_cb: *const c_void) -> CFType;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    static kCFBooleanTrue: CFType;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferPoolCreate(alloc: CFType, pool_attrs: CFType, buffer_attrs: CFType, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(alloc: CFType, pool: *mut c_void, aux: CFType, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferPoolFlush(pool: *mut c_void, flags: u64);
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    static kCVPixelBufferPoolMinimumBufferCountKey: CFType;
    static kCVPixelBufferPoolAllocationThresholdKey: CFType;
    static kCVPixelBufferPixelFormatTypeKey: CFType;
    static kCVPixelBufferWidthKey: CFType;
    static kCVPixelBufferHeightKey: CFType;
    static kCVPixelBufferIOSurfacePropertiesKey: CFType;
    static kCVPixelBufferMetalCompatibilityKey: CFType;
    static kCVPixelBufferOpenGLCompatibilityKey: CFType;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(surface: *mut c_void) -> u32;
}

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLChoosePixelFormat(attrs: *const i32, pix: *mut *mut c_void, npix: *mut i32) -> i32;
    fn CGLDestroyPixelFormat(pix: *mut c_void) -> i32;
    fn CGLCreateContext(pix: *mut c_void, share: *mut c_void, ctx: *mut *mut c_void) -> i32;
    fn CGLDestroyContext(ctx: *mut c_void) -> i32;
    fn CGLSetCurrentContext(ctx: *mut c_void) -> i32;
    fn CGLTexImageIOSurface2D(ctx: *mut c_void, target: u32, internal_format: u32, w: i32, h: i32, format: u32, ty: u32, surface: *mut c_void, plane: u32) -> i32;
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glGenFramebuffers(n: i32, fbos: *mut u32);
    fn glBindFramebuffer(target: u32, fbo: u32);
    fn glFramebufferTexture2D(target: u32, attachment: u32, textarget: u32, texture: u32, level: i32);
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glDeleteFramebuffers(n: i32, fbos: *const u32);
    fn glFenceSync(condition: u32, flags: u32) -> *mut c_void;
    fn glClientWaitSync(sync: *mut c_void, flags: u32, timeout_ns: u64) -> u32;
    fn glDeleteSync(sync: *mut c_void);
    fn glFinish();
    fn glGetError() -> u32;
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
/// `RTLD_DEFAULT` on macOS: every image loaded, OpenGL.framework (linked above) among them.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

#[repr(C)]
struct Param { ty: c_int, data: *mut c_void }
#[repr(C)]
struct GlInit { get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void, ctx: *mut c_void, extra_exts: *const c_char }
#[repr(C)]
struct Fbo { fbo: c_int, w: c_int, h: c_int, internal_format: c_int }

/// Rendering counters, for measuring.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    /// CPU time on the render thread per frame, in total, waiting for the GPU included.
    pub render_ms: f64,
    /// Frames mpv had ready that found no free buffer (Rune still holding all of them).
    pub starved: u64,
    pub size: (u32, u32),
}

struct Shared {
    /// Target size in device pixels, `w << 32 | h`; 0 until the app lays the video out.
    size: AtomicU64,
    quit: AtomicBool,
    stats: Mutex<Stats>,
    /// Why the renderer couldn't start, if it couldn't.
    error: Mutex<Option<String>>,
}

/// Frames from mpv into `surface`. Drop it before the mpv core it was started on.
pub struct FrameSource {
    kick: SyncSender<()>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// mpv's core handle, used only to create the render context on the render thread.
struct Core(*mut c_void);
unsafe impl Send for Core {}

unsafe extern "C" fn on_update(data: *mut c_void) {
    // Never render here (mpv's thread, render API rules): wake the render thread instead.
    let _ = unsafe { &*(data as *const SyncSender<()>) }.try_send(());
}

impl FrameSource {
    /// Attach mpv's OpenGL renderer to `mpv` (an initialized core with `vo=libmpv`, before any
    /// file is loaded) and start pushing frames into `surface`. Returns at once; `load` (loading
    /// the file) runs on the render thread once the renderer is up. If it can't be set up,
    /// `error` says why.
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

/// A CoreFoundation object this code owns one retain of.
struct Owned(CFType);
impl Drop for Owned {
    fn drop(&mut self) { if !self.0.is_null() { unsafe { CFRelease(self.0) } } }
}

fn number(v: i32) -> Owned { Owned(unsafe { CFNumberCreate(std::ptr::null(), CF_SINT32, (&raw const v).cast()) }) }

fn dict(pairs: &[(CFType, CFType)]) -> Owned {
    let (keys, values): (Vec<CFType>, Vec<CFType>) = pairs.iter().copied().unzip();
    Owned(unsafe {
        CFDictionaryCreate(std::ptr::null(), keys.as_ptr(), values.as_ptr(), pairs.len() as isize,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(), (&raw const kCFTypeDictionaryValueCallBacks).cast())
    })
}

/// The GL side of one IOSurface the pool hands out: a rectangle texture on its memory, and a
/// framebuffer for mpv to render into.
struct Target { texture: u32, fbo: u32 }

/// What lives on the render thread.
struct Renderer {
    cgl: *mut c_void,
    render: *mut c_void,
    kick: *mut SyncSender<()>,
    /// Buffers of the current size, recycled once nobody holds them.
    pool: Option<Owned>,
    /// Per IOSurface (by id), the texture and framebuffer onto it.
    targets: HashMap<u32, Target>,
    frame_id: u64,
    size: (u32, u32),
}

/// GL entry points, for mpv: every one OpenGL.framework exports.
unsafe extern "C" fn get_proc_address(_: *mut c_void, name: *const c_char) -> *mut c_void {
    unsafe { dlsym(RTLD_DEFAULT, name) }
}

impl Renderer {
    fn new(mpv: *mut c_void, kick: SyncSender<()>) -> Result<Renderer, String> {
        unsafe {
            // An offscreen context: mpv only ever draws into the framebuffers made here.
            let attrs = [CGL_PFA_OPENGL_PROFILE, CGL_OGLP_VERSION_3_2_CORE, CGL_PFA_ACCELERATED, CGL_PFA_ALLOW_OFFLINE_RENDERERS, 0];
            let (mut pix, mut n) = (std::ptr::null_mut(), 0);
            if CGLChoosePixelFormat(attrs.as_ptr(), &mut pix, &mut n) != 0 || pix.is_null() { return Err("no OpenGL pixel format".into()) }
            let mut cgl = std::ptr::null_mut();
            let made = CGLCreateContext(pix, std::ptr::null_mut(), &mut cgl);
            CGLDestroyPixelFormat(pix);
            if made != 0 || cgl.is_null() { return Err(format!("CGLCreateContext failed ({made})")) }
            CGLSetCurrentContext(cgl);
            // mpv's renderer, on this GL context.
            let create: unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *mut Param) -> c_int = sym(b"mpv_render_context_create\0")?;
            let set_update: unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void), *mut c_void) =
                sym(b"mpv_render_context_set_update_callback\0")?;
            let mut init = GlInit { get_proc_address, ctx: std::ptr::null_mut(), extra_exts: std::ptr::null() };
            let mut params = [
                Param { ty: PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
                Param { ty: PARAM_OPENGL_INIT_PARAMS, data: (&raw mut init).cast() },
                Param { ty: PARAM_INVALID, data: std::ptr::null_mut() },
            ];
            let mut render = std::ptr::null_mut();
            if create(&mut render, mpv, params.as_mut_ptr()) < 0 || render.is_null() {
                CGLSetCurrentContext(std::ptr::null_mut());
                CGLDestroyContext(cgl);
                return Err("mpv's OpenGL renderer didn't start".into());
            }
            let kick = Box::into_raw(Box::new(kick));
            set_update(render, on_update, kick.cast());
            Ok(Renderer { cgl, render, kick, pool: None, targets: HashMap::new(), frame_id: 0, size: (0, 0) })
        }
    }

    /// A new pool for frames of `w`×`h`. Buffers of the old size still on screen stay alive
    /// through Rune's own retain until a new frame replaces them.
    fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        self.drop_targets();
        self.pool = None;
        let (min, format, width, height) = (number(POOL_MIN), number(BGRA), number(w as i32), number(h as i32));
        let empty = dict(&[]);
        let pool_attrs = dict(&[(unsafe { kCVPixelBufferPoolMinimumBufferCountKey }, min.0)]);
        let buffer_attrs = unsafe {
            dict(&[
                (kCVPixelBufferPixelFormatTypeKey, format.0), (kCVPixelBufferWidthKey, width.0), (kCVPixelBufferHeightKey, height.0),
                (kCVPixelBufferIOSurfacePropertiesKey, empty.0), (kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue),
                (kCVPixelBufferOpenGLCompatibilityKey, kCFBooleanTrue),
            ])
        };
        let mut pool = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferPoolCreate(std::ptr::null(), pool_attrs.0, buffer_attrs.0, &mut pool) };
        if status != 0 || pool.is_null() { return Err(format!("CVPixelBufferPoolCreate failed ({status})")) }
        self.pool = Some(Owned(pool));
        self.size = (w, h);
        Ok(())
    }

    /// A buffer nobody holds any more, waiting up to `limit` for Rune to let one go.
    fn take(&self, limit: Duration) -> Option<Owned> {
        let pool = self.pool.as_ref()?.0 as *mut c_void;
        let max = number(POOL_MAX);
        let aux = dict(&[(unsafe { kCVPixelBufferPoolAllocationThresholdKey }, max.0)]);
        let until = Instant::now() + limit;
        loop {
            let mut buffer = std::ptr::null_mut();
            match unsafe { CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(std::ptr::null(), pool, aux.0, &mut buffer) } {
                0 if !buffer.is_null() => return Some(Owned(buffer)),
                CV_WOULD_EXCEED if Instant::now() < until => std::thread::sleep(Duration::from_millis(1)),
                _ => return None,
            }
        }
    }

    /// The framebuffer onto `buffer`'s IOSurface, made the first time the pool hands it out.
    fn target(&mut self, buffer: &Owned) -> Result<u32, String> {
        let surface = unsafe { CVPixelBufferGetIOSurface(buffer.0 as *mut c_void) };
        if surface.is_null() { return Err("the pixel buffer has no IOSurface".into()) }
        let id = unsafe { IOSurfaceGetID(surface) };
        if let Some(t) = self.targets.get(&id) { return Ok(t.fbo) }
        // The pool retires idle buffers and makes new ones; forget the ones it let go.
        if self.targets.len() >= 2 * POOL_MAX as usize { self.drop_targets() }
        let (w, h) = self.size;
        unsafe {
            let mut texture = 0;
            glGenTextures(1, &mut texture);
            glBindTexture(GL_TEXTURE_RECTANGLE, texture);
            let bound = CGLTexImageIOSurface2D(self.cgl, GL_TEXTURE_RECTANGLE, GL_RGBA, w as i32, h as i32, GL_BGRA, GL_UNSIGNED_INT_8_8_8_8_REV, surface, 0);
            glBindTexture(GL_TEXTURE_RECTANGLE, 0);
            let mut fbo = 0;
            glGenFramebuffers(1, &mut fbo);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, texture, 0);
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            let t = Target { texture, fbo };
            if bound != 0 || status != GL_FRAMEBUFFER_COMPLETE {
                self.drop_target(t);
                return Err(format!("the IOSurface isn't a complete framebuffer (CGL {bound}, status {status:#x}, GL error {:#x})", glGetError()));
            }
            self.targets.insert(id, t);
        }
        Ok(self.targets[&id].fbo)
    }

    fn run(&mut self, wake: Receiver<()>, shared: Arc<Shared>, surface: NativeSurface) {
        let Ok(update) = sym::<unsafe extern "C" fn(*mut c_void) -> u64>(b"mpv_render_context_update\0") else { return };
        let Ok(draw) = sym::<unsafe extern "C" fn(*mut c_void, *mut Param) -> c_int>(b"mpv_render_context_render\0") else { return };
        while wake.recv().is_ok() && !shared.quit.load(Ordering::Relaxed) {
            let flags = unsafe { update(self.render) };
            let v = shared.size.load(Ordering::Relaxed);
            let (w, h) = ((v >> 32) as u32, v as u32);
            if w == 0 || h == 0 { continue }
            let resized = (w, h) != self.size;
            if flags & UPDATE_FRAME == 0 && !resized { continue }
            if resized && let Err(e) = self.resize(w, h) {
                eprintln!("video: {e}");
                continue;
            }
            let t = Instant::now();
            // A buffer Rune has let go of; if none comes back in time, skip this frame.
            let Some(buffer) = self.take(Duration::from_millis(50)) else {
                shared.stats.lock().unwrap().starved += 1;
                continue;
            };
            let fbo = match self.target(&buffer) {
                Ok(fbo) => fbo,
                Err(e) => { eprintln!("video: {e}"); continue }
            };
            unsafe {
                let mut fbo = Fbo { fbo: fbo as c_int, w: w as c_int, h: h as c_int, internal_format: GL_RGBA8 as c_int };
                // The IOSurface's rows are top-down, as mpv renders into an FBO: no flip.
                let mut flip: c_int = 0;
                let mut params = [
                    Param { ty: PARAM_OPENGL_FBO, data: (&raw mut fbo).cast() },
                    Param { ty: PARAM_FLIP_Y, data: (&raw mut flip).cast() },
                    Param { ty: PARAM_INVALID, data: std::ptr::null_mut() },
                ];
                draw(self.render, params.as_mut_ptr());
                // Metal doesn't wait for GL: the frame goes out once GL has finished it.
                let fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                let waited = glClientWaitSync(fence, GL_SYNC_FLUSH_COMMANDS_BIT, 100_000_000);
                glDeleteSync(fence);
                if waited == GL_TIMEOUT_EXPIRED || waited == GL_WAIT_FAILED {
                    shared.stats.lock().unwrap().starved += 1;
                    continue;
                }
            }
            self.frame_id += 1;
            if self.frame_id == 30 && std::env::var_os("VIDEO_PROBE").is_some() { self.probe(&buffer) }
            // Rune keeps its own retain for as long as it shows the frame; ours goes now, so
            // the pool can hand the buffer out again once Rune lets go.
            let frame = match unsafe { PixelBuffer::from_raw(buffer.0 as *mut c_void) } {
                Ok(f) => f,
                Err(e) => { eprintln!("video: {e}"); continue }
            };
            drop(buffer);
            if let Err(e) = surface.push(frame, self.frame_id) { eprintln!("video: {e}") }
            let mut s = shared.stats.lock().unwrap();
            s.render_ms += t.elapsed().as_secs_f64() * 1000.0;
            (s.frames, s.size) = (self.frame_id, (w, h));
        }
    }

    /// Debugging only: read a few pixels of a rendered frame on the CPU.
    fn probe(&self, buffer: &Owned) {
        let b = buffer.0 as *mut c_void;
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        unsafe {
            if CVPixelBufferLockBaseAddress(b, 1 /* read only */) != 0 { return }
            let (base, row) = (CVPixelBufferGetBaseAddress(b) as *const u8, CVPixelBufferGetBytesPerRow(b));
            let px = |x: usize, y: usize| { let p = std::slice::from_raw_parts(base.add(y * row + x * 4), 4); [p[2], p[1], p[0], p[3]] };
            let column: Vec<_> = (1..8).map(|k| px(w / 2, h * k / 8)).collect();
            eprintln!("video probe {w}x{h}: top-left {:?} bottom-right {:?}, down the middle {column:?} (RGBA)", px(4, 4), px(w - 4, h - 4));
            CVPixelBufferUnlockBaseAddress(b, 1);
        }
    }

    fn drop_target(&self, t: Target) {
        unsafe {
            glDeleteFramebuffers(1, &t.fbo);
            glDeleteTextures(1, &t.texture);
        }
    }

    fn drop_targets(&mut self) {
        for (_, t) in std::mem::take(&mut self.targets) { self.drop_target(t) }
        if let Some(pool) = &self.pool { unsafe { CVPixelBufferPoolFlush(pool.0 as *mut c_void, 0) } }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            // mpv's renderer goes first, with its GL context current; then no more callbacks.
            if let Ok(free) = sym::<unsafe extern "C" fn(*mut c_void)>(b"mpv_render_context_free\0") { free(self.render) }
            drop(Box::from_raw(self.kick));
            glFinish();
            self.drop_targets();
            self.pool = None;
            CGLSetCurrentContext(std::ptr::null_mut());
            CGLDestroyContext(self.cgl);
        }
    }
}
