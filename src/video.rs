//! Where the player's frames come from: mpv renders each frame on the GPU (the libmpv render
//! API, OpenGL) straight into a Direct3D 11 texture that Rune draws as a `NativeSurface` element
//! on its own D3D12 device. No pixel touches the CPU, and nothing is copied: OpenGL renders into
//! the shared texture's own memory (`GL_EXT_memory_object_win32`), and the two GPUs' queues
//! take turns through shared fences (`GL_EXT_semaphore_win32`).
//!
//! This is the one seam between mpv and the screen: the view only knows the surface, and the
//! player only `start`s a source, gives it a size and drops it.
//!
//! The work runs on a thread of its own, which owns a hidden window's OpenGL context (mpv's
//! render API wants its GL context current on the thread that renders), a D3D11 device that
//! makes the shared textures and fences, and a ring of textures. mpv's update callback only wakes
//! that thread. Each frame: wait (on the CPU) until Rune has let go of the next texture (its
//! `done` fence), let mpv render into it, have GL signal `ready` once that rendering completes,
//! and push it; Rune's queue waits for `ready` before sampling.

use crate::player::sym;
use rune::native_surface::win32::{FrameSync, SharedFence, SharedTexture};
use rune::native_surface::{ColorInfo, Format};
use rune::NativeSurface;
use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIFactory1, IDXGIResource1};
use windows::Win32::Graphics::Gdi::{GetDC, HDC, ReleaseDC};
use windows::Win32::Graphics::OpenGL::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP};
use windows::core::{Interface, PCSTR, s, w};

// mpv_render_param_type values (render.h, render_gl.h).
const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const PARAM_OPENGL_FBO: c_int = 3;
const PARAM_FLIP_Y: c_int = 4;
/// mpv_render_update_flag: a new frame should be rendered.
const UPDATE_FRAME: u64 = 1;
/// Textures in flight between mpv and Rune: one on screen, one being drawn, one spare.
const RING: usize = 3;

// GL_EXT_memory_object(_win32), GL_EXT_semaphore(_win32), GL 3.0 framebuffers.
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_RGBA8: u32 = 0x8058;
const GL_DEDICATED_MEMORY_OBJECT_EXT: u32 = 0x9581;
const GL_HANDLE_TYPE_D3D11_IMAGE_EXT: u32 = 0x958B;
const GL_HANDLE_TYPE_D3D12_FENCE_EXT: u32 = 0x9594;
const GL_D3D12_FENCE_VALUE_EXT: u32 = 0x9595;
const GL_LAYOUT_SHADER_READ_ONLY_EXT: u32 = 0x9591;

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
    /// CPU time on the render thread issuing frames, in total (the GPU works on its own).
    pub render_ms: f64,
    /// Frames mpv had ready that found no free texture (Rune still holding all of them).
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
    /// file is loaded) and start pushing frames into `surface`. Returns at once: setting up GL,
    /// D3D and mpv's renderer takes a moment, and `load` (loading the file) runs on the render
    /// thread once it's done, as mpv needs its renderer before a video starts. If it can't be set
    /// up, `error` says why.
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

/// GL entry points beyond OpenGL 1.1.
struct Gl {
    gen_fbs: unsafe extern "system" fn(i32, *mut u32),
    bind_fb: unsafe extern "system" fn(u32, u32),
    fb_tex: unsafe extern "system" fn(u32, u32, u32, u32, i32),
    fb_status: unsafe extern "system" fn(u32) -> u32,
    del_fbs: unsafe extern "system" fn(i32, *const u32),
    create_memory: unsafe extern "system" fn(i32, *mut u32),
    memory_param: unsafe extern "system" fn(u32, u32, *const i32),
    import_memory: unsafe extern "system" fn(u32, u64, u32, *mut c_void),
    tex_storage_mem: unsafe extern "system" fn(u32, i32, u32, i32, i32, u32, u64),
    del_memory: unsafe extern "system" fn(i32, *const u32),
    gen_semaphores: unsafe extern "system" fn(i32, *mut u32),
    import_semaphore: unsafe extern "system" fn(u32, u32, *mut c_void),
    semaphore_value: unsafe extern "system" fn(u32, u32, *const u64),
    signal_semaphore: unsafe extern "system" fn(u32, u32, *const u32, u32, *const u32, *const u32),
    del_semaphores: unsafe extern "system" fn(i32, *const u32),
}

/// One texture of the ring: the D3D11 texture Rune reads, as GL renders into it, and its last frame.
struct Slot {
    shared: SharedTexture,
    _d3d: ID3D11Texture2D,
    memory: u32,
    texture: u32,
    fbo: u32,
    last: u64,
}

/// What lives on the render thread.
struct Renderer {
    window: HWND,
    dc: HDC,
    glrc: HGLRC,
    gl: Gl,
    render: *mut c_void,
    kick: *mut SyncSender<()>,
    device: ID3D11Device,
    /// Only to ask the allocation size of a texture (`allocation_size`).
    d3d12: windows::Win32::Graphics::Direct3D12::ID3D12Device,
    /// GL signals `ready` once a frame is rendered; Rune raises `done` once it lets one go.
    ready: (u32, SharedFence),
    done: (ID3D11Fence, SharedFence),
    /// `ready` as seen from here, to wait for GL to finish a frame before handing it over.
    ready_fence: ID3D11Fence,
    /// For waiting on either fence (one wait at a time, on this thread).
    done_event: HANDLE,
    frame_id: u64,
    slots: Vec<Slot>,
    size: (u32, u32),
}

fn opengl32() -> HMODULE {
    static LIB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    HMODULE(*LIB.get_or_init(|| unsafe { LoadLibraryA(s!("opengl32.dll")) }.map_or(0, |m| m.0 as usize)) as *mut c_void)
}

/// GL entry points, for mpv and for us: extensions and GL 1.2+ through wglGetProcAddress, the
/// rest from opengl32.dll.
unsafe extern "C" fn get_proc_address(_: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = PCSTR(name.cast());
    let p = unsafe { wglGetProcAddress(name) }.map_or(0, |f| f as usize);
    if p > 3 && p != usize::MAX { return p as *mut c_void }
    unsafe { GetProcAddress(opengl32(), name) }.map_or(std::ptr::null_mut(), |f| f as *mut c_void)
}

fn proc<T>(name: &str) -> Result<T, String> {
    let c = CString::new(name).unwrap();
    let p = unsafe { get_proc_address(std::ptr::null_mut(), c.as_ptr()) };
    if p.is_null() { return Err(format!("OpenGL has no {name} (GL_EXT_memory_object_win32 and GL_EXT_semaphore_win32 needed)")) }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String { move |e| format!("{what}: {e}") }

impl Renderer {
    fn new(mpv: *mut c_void, kick: SyncSender<()>) -> Result<Renderer, String> {
        unsafe {
            // A hidden window for the GL context; nothing is ever drawn into it.
            let window = CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("mpv"), WS_POPUP, 0, 0, 16, 16, None, None, None, None)
                .map_err(err("CreateWindowExW"))?;
            let dc = GetDC(Some(window));
            let pfd = PIXELFORMATDESCRIPTOR {
                nSize: size_of::<PIXELFORMATDESCRIPTOR>() as u16,
                nVersion: 1,
                dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                iPixelType: PFD_TYPE_RGBA,
                cColorBits: 32,
                iLayerType: PFD_MAIN_PLANE.0 as u8,
                ..Default::default()
            };
            SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd).map_err(err("SetPixelFormat"))?;
            let glrc = wglCreateContext(dc).map_err(err("wglCreateContext"))?;
            wglMakeCurrent(dc, glrc).map_err(err("wglMakeCurrent"))?;
            let gl = Gl {
                gen_fbs: proc("glGenFramebuffers")?,
                bind_fb: proc("glBindFramebuffer")?,
                fb_tex: proc("glFramebufferTexture2D")?,
                fb_status: proc("glCheckFramebufferStatus")?,
                del_fbs: proc("glDeleteFramebuffers")?,
                create_memory: proc("glCreateMemoryObjectsEXT")?,
                memory_param: proc("glMemoryObjectParameterivEXT")?,
                import_memory: proc("glImportMemoryWin32HandleEXT")?,
                tex_storage_mem: proc("glTexStorageMem2DEXT")?,
                del_memory: proc("glDeleteMemoryObjectsEXT")?,
                gen_semaphores: proc("glGenSemaphoresEXT")?,
                import_semaphore: proc("glImportSemaphoreWin32HandleEXT")?,
                semaphore_value: proc("glSemaphoreParameterui64vEXT")?,
                signal_semaphore: proc("glSignalSemaphoreEXT")?,
                del_semaphores: proc("glDeleteSemaphoresEXT")?,
            };
            // D3D11 on the GPU that renders: the adapter with the most video memory (adapter 0 can
            // be a virtual display's, which neither GL nor Rune's device is on).
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(err("CreateDXGIFactory1"))?;
            let adapter = (0..).map_while(|i| factory.EnumAdapters1(i).ok())
                .max_by_key(|a| a.GetDesc1().map_or(0, |d| d.DedicatedVideoMemory)).ok_or("no GPU adapter")?;
            let mut device = None;
            D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, Default::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None,
                D3D11_SDK_VERSION, Some(&mut device), None, None).map_err(err("D3D11CreateDevice"))?;
            let device: ID3D11Device = device.ok_or("no D3D11 device")?;
            let mut d3d12: Option<windows::Win32::Graphics::Direct3D12::ID3D12Device> = None;
            windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut d3d12)
                .map_err(err("D3D12CreateDevice"))?;
            let d3d12 = d3d12.ok_or("no D3D12 device")?;
            // The fences, shared with Rune and, for `ready`, with GL.
            let fence = || -> Result<(ID3D11Fence, SharedFence, HANDLE), String> {
                let d5: ID3D11Device5 = device.cast().map_err(err("ID3D11Device5"))?;
                let mut f: Option<ID3D11Fence> = None;
                d5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut f).map_err(err("CreateFence"))?;
                let f = f.ok_or("no fence")?;
                let h = f.CreateSharedHandle(None, 0x1F0003 /* GENERIC_ALL */, None).map_err(err("fence CreateSharedHandle"))?;
                Ok((f, SharedFence::from_nt_handle(h.0)?, h))
            };
            let (ready_fence, ready_shared, ready_handle) = fence()?;
            let mut ready_sem = 0;
            (gl.gen_semaphores)(1, &mut ready_sem);
            (gl.import_semaphore)(ready_sem, GL_HANDLE_TYPE_D3D12_FENCE_EXT, ready_handle.0);
            let _ = CloseHandle(ready_handle);
            let (done_fence, done_shared, done_handle) = fence()?;
            let _ = CloseHandle(done_handle);
            let done_event = CreateEventW(None, false, false, None).map_err(err("CreateEventW"))?;
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
                return Err("mpv's OpenGL renderer didn't start".into());
            }
            let kick = Box::into_raw(Box::new(kick));
            set_update(render, on_update, kick.cast());
            Ok(Renderer { window, dc, glrc, gl, render, kick, device, d3d12, ready: (ready_sem, ready_shared), done: (done_fence, done_shared),
                ready_fence, done_event, frame_id: 0, slots: Vec::new(), size: (0, 0) })
        }
    }

    /// Make a new ring for frames of `w`×`h`. The old textures go at once: a frame Rune still
    /// shows keeps its texture alive through Rune's own reference until a new one replaces it.
    fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        for s in std::mem::take(&mut self.slots) { self.drop_slot(s) }
        for _ in 0..RING {
            unsafe {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w, Height: h, MipLevels: 1, ArraySize: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(err("CreateTexture2D"))?;
                let tex: ID3D11Texture2D = tex.ok_or("no texture")?;
                let res: IDXGIResource1 = tex.cast().map_err(err("IDXGIResource1"))?;
                let handle = res.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, None)
                    .map_err(err("CreateSharedHandle"))?;
                // GL renders into the texture's own memory.
                let mut memory = 0;
                (self.gl.create_memory)(1, &mut memory);
                (self.gl.memory_param)(memory, GL_DEDICATED_MEMORY_OBJECT_EXT, &1);
                (self.gl.import_memory)(memory, self.allocation_size(w, h), GL_HANDLE_TYPE_D3D11_IMAGE_EXT, handle.0);
                let mut texture = 0;
                glGenTextures(1, &mut texture);
                glBindTexture(GL_TEXTURE_2D, texture);
                (self.gl.tex_storage_mem)(GL_TEXTURE_2D, 1, GL_RGBA8, w as i32, h as i32, memory, 0);
                glBindTexture(GL_TEXTURE_2D, 0);
                let shared = SharedTexture::from_nt_handle(handle.0, [w, h], Format::Bgra, ColorInfo::default());
                let _ = CloseHandle(handle);
                let mut fbo = 0;
                (self.gl.gen_fbs)(1, &mut fbo);
                (self.gl.bind_fb)(GL_FRAMEBUFFER, fbo);
                (self.gl.fb_tex)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
                let status = (self.gl.fb_status)(GL_FRAMEBUFFER);
                (self.gl.bind_fb)(GL_FRAMEBUFFER, 0);
                let slot = Slot { shared: shared?, _d3d: tex, memory, texture, fbo, last: 0 };
                if status != GL_FRAMEBUFFER_COMPLETE {
                    self.drop_slot(slot);
                    return Err(format!("the shared texture isn't a complete framebuffer ({status:#x}, GL error {:#x})", glGetError()));
                }
                self.slots.push(slot);
            }
        }
        self.size = (w, h);
        Ok(())
    }

    /// Bytes the GPU allocates for a `w`×`h` RGBA8 texture (tiling pads it past `w*h*4`); GL
    /// needs the true size to place a texture in the shared memory.
    fn allocation_size(&self, w: u32, h: u32) -> u64 {
        use windows::Win32::Graphics::Direct3D12::*;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: w as u64, Height: h, DepthOrArraySize: 1, MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, ..Default::default()
        };
        unsafe { self.d3d12.GetResourceAllocationInfo(0, &[desc]).SizeInBytes }
    }

    /// Wait (up to `limit`) until Rune has let go of frame `id` and everything before it.
    fn wait_done(&self, id: u64, limit: Duration) -> bool { self.wait(&self.done.0, id, limit) }

    /// Wait (up to `limit`) until `fence` reaches `id`.
    fn wait(&self, fence: &ID3D11Fence, id: u64, limit: Duration) -> bool {
        unsafe {
            if fence.GetCompletedValue() >= id { return true }
            if limit.is_zero() || fence.SetEventOnCompletion(id, self.done_event).is_err() { return false }
            WaitForSingleObject(self.done_event, limit.as_millis() as u32);
            fence.GetCompletedValue() >= id
        }
    }

    fn run(&mut self, wake: Receiver<()>, shared: Arc<Shared>, surface: NativeSurface) {
        let Ok(update) = sym::<unsafe extern "C" fn(*mut c_void) -> u64>(b"mpv_render_context_update\0") else { return };
        let Ok(draw) = sym::<unsafe extern "C" fn(*mut c_void, *mut Param) -> c_int>(b"mpv_render_context_render\0") else { return };
        let mut next = 0usize;
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
            // The next texture Rune has let go of; if none comes back in time, skip this frame.
            let free = (0..RING).map(|k| (next + k) % RING).find(|&i| self.wait_done(self.slots[i].last, Duration::ZERO));
            let Some(i) = free.or_else(|| self.wait_done(self.slots[next].last, Duration::from_millis(50)).then_some(next)) else {
                shared.stats.lock().unwrap().starved += 1;
                continue;
            };
            next = (i + 1) % RING;
            self.frame_id += 1;
            let slot = &mut self.slots[i];
            slot.last = self.frame_id;
            unsafe {
                let mut fbo = Fbo { fbo: slot.fbo as c_int, w: w as c_int, h: h as c_int, internal_format: GL_RGBA8 as c_int };
                // The texture's rows are D3D's, top-down, as mpv renders into an FBO: no flip.
                let mut flip: c_int = 0;
                let mut params = [
                    Param { ty: PARAM_OPENGL_FBO, data: (&raw mut fbo).cast() },
                    Param { ty: PARAM_FLIP_Y, data: (&raw mut flip).cast() },
                    Param { ty: PARAM_INVALID, data: std::ptr::null_mut() },
                ];
                draw(self.render, params.as_mut_ptr());
                // `ready` reaches this frame's id once GL has finished rendering it.
                (self.gl.semaphore_value)(self.ready.0, GL_D3D12_FENCE_VALUE_EXT, &self.frame_id);
                (self.gl.signal_semaphore)(self.ready.0, 0, std::ptr::null(), 1, &slot.texture, &GL_LAYOUT_SHADER_READ_ONLY_EXT);
                glFlush();
            }
            let frame = slot.shared.clone().with_sync(FrameSync { ready: self.ready.1.clone(), ready_value: self.frame_id, done: self.done.1.clone() });
            // Hand Rune only a finished frame. Its queue would wait for GL instead, and a frame
            // that waits misses its vsync: one hitch per video frame, fullscreen especially
            // (independent flip). Until this one is ready, Rune keeps showing the last.
            self.wait(&self.ready_fence, self.frame_id, Duration::from_millis(100));
            if self.frame_id == 30 && std::env::var_os("VIDEO_PROBE").is_some() { self.probe(i) }
            if let Err(e) = surface.push(frame, self.frame_id) { eprintln!("video: {e}") }
            let mut s = shared.stats.lock().unwrap();
            s.render_ms += t.elapsed().as_secs_f64() * 1000.0;
            (s.frames, s.size) = (self.frame_id, (w, h));
        }
    }

    /// Debugging only: wait for GL, copy one texture to the CPU and print a few pixels.
    fn probe(&self, i: usize) {
        unsafe {
            glFinish();
            let ctx = self.device.GetImmediateContext().unwrap();
            let (w, h) = self.size;
            let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
            let mut staging = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging)).unwrap();
            let staging: ID3D11Texture2D = staging.unwrap();
            ctx.CopyResource(&staging, &self.slots[i]._d3d);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let px = |x: u32, y: u32| std::slice::from_raw_parts((m.pData as *const u8).add((y * m.RowPitch + x * 4) as usize), 4).to_vec();
            eprintln!("video probe {w}x{h}: top-left {:?} centre {:?} bottom-right {:?} (RGBA)", px(4, 4), px(w / 2, h / 2), px(w - 4, h - 4));
            ctx.Unmap(&staging, 0);
        }
    }

    fn drop_slot(&self, s: Slot) {
        unsafe {
            (self.gl.del_fbs)(1, &s.fbo);
            glDeleteTextures(1, &s.texture);
            (self.gl.del_memory)(1, &s.memory);
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            // mpv's renderer goes first, with its GL context current; then no more callbacks.
            if let Ok(free) = sym::<unsafe extern "C" fn(*mut c_void)>(b"mpv_render_context_free\0") { free(self.render) }
            drop(Box::from_raw(self.kick));
            glFinish();
            let newest = self.slots.iter().map(|s| s.last).max().unwrap_or(0);
            self.wait_done(newest, Duration::from_millis(200));
            for s in std::mem::take(&mut self.slots) { self.drop_slot(s) }
            (self.gl.del_semaphores)(1, &self.ready.0);
            let _ = wglMakeCurrent(HDC::default(), HGLRC::default());
            let _ = wglDeleteContext(self.glrc);
            ReleaseDC(Some(self.window), self.dc);
            let _ = DestroyWindow(self.window);
            let _ = CloseHandle(self.done_event);
        }
    }
}
