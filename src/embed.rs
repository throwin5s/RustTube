//! In-process playback and rendering through libmpv.
//!
//! mpv is linked as a library instead of spawned as a child process, so video
//! can be drawn straight into the egui viewport: the render API writes frames
//! into an OpenGL FBO, and an `egui_glow` paint callback composites that FBO in
//! the watch view. Same code on macOS and Windows; only the libmpv linking in
//! `build.rs` differs. Audio, HLS handling and subtitle rendering all stay with
//! mpv, which is why this reuses the same stream URLs the IPC player did.

use eframe::egui;
use eframe::glow::{self, HasContext};
use egui_glow::CallbackFn;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// --------------------------------------------------------------------- FFI

type Handle = *mut c_void;
type RenderCtx = *mut c_void;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
    reply_userdata: u64,
    playlist_entry_id: i64,
    playlist_insert_id: i64,
    playlist_insert_num_entries: c_int,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_FILE_LOADED: c_int = 8;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

const RENDER_PARAM_INVALID: c_int = 0;
const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;

extern "C" {
    fn mpv_create() -> Handle;
    fn mpv_initialize(ctx: Handle) -> c_int;
    fn mpv_set_option_string(ctx: Handle, name: *const c_char, value: *const c_char) -> c_int;
    fn mpv_command(ctx: Handle, args: *mut *const c_char) -> c_int;
    fn mpv_observe_property(
        ctx: Handle,
        reply_userdata: u64,
        name: *const c_char,
        format: c_int,
    ) -> c_int;
    fn mpv_wait_event(ctx: Handle, timeout: f64) -> *mut MpvEvent;

    fn mpv_render_context_create(
        res: *mut RenderCtx,
        mpv: Handle,
        params: *mut MpvRenderParam,
    ) -> c_int;
    fn mpv_render_context_update(ctx: RenderCtx) -> c_int;
    fn mpv_render_context_render(ctx: RenderCtx, params: *mut MpvRenderParam) -> c_int;
    fn mpv_render_context_report_swap(ctx: RenderCtx);
    fn mpv_render_context_set_update_callback(
        ctx: RenderCtx,
        callback: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
    );
}

// ---------------------------------------------------------------- GL loader

#[cfg(target_os = "macos")]
mod loader {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    const RTLD_LAZY: i32 = 1;

    extern "C" {
        fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    pub unsafe fn load(name: *const c_char) -> *mut c_void {
        static HANDLE: OnceLock<usize> = OnceLock::new();
        let handle = *HANDLE.get_or_init(|| unsafe {
            dlopen(
                c"/System/Library/Frameworks/OpenGL.framework/OpenGL".as_ptr(),
                RTLD_LAZY,
            ) as usize
        }) as *mut c_void;
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        unsafe { dlsym(handle, name) }
    }
}

#[cfg(target_os = "windows")]
mod loader {
    use std::ffi::{c_char, c_void};

    #[link(name = "opengl32")]
    extern "system" {
        fn wglGetProcAddress(name: *const c_char) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    pub unsafe fn load(name: *const c_char) -> *mut c_void {
        unsafe {
            let pointer = wglGetProcAddress(name);
            // wglGetProcAddress only covers extensions; the sentinel values
            // mean "not found" and core 1.1 entry points live in opengl32.
            if !pointer.is_null()
                && pointer as isize != -1
                && pointer as isize != 1
                && pointer as isize != 2
                && pointer as isize != 3
            {
                return pointer;
            }
            let module = GetModuleHandleA(c"opengl32.dll".as_ptr() as *const u8);
            if module.is_null() {
                return std::ptr::null_mut();
            }
            GetProcAddress(module, name)
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod loader {
    use std::ffi::{c_char, c_void};

    pub unsafe fn load(_name: *const c_char) -> *mut c_void {
        std::ptr::null_mut()
    }
}

unsafe extern "C" fn get_proc_address(
    _ctx: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    unsafe { loader::load(name) }
}

// ------------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub ended: bool,
    pub started: bool,
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub video_id: String,
    pub title: String,
    pub channel_id: String,
    pub channel_name: String,
    pub duration_hint: i64,
}

// ------------------------------------------------------------------- state

#[derive(Default)]
struct GlState {
    fbo: Option<glow::Framebuffer>,
    tex: Option<glow::Texture>,
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    vbo: Option<glow::Buffer>,
    size: (i32, i32),
    size_uniform: Option<glow::UniformLocation>,
    radius_uniform: Option<glow::UniformLocation>,
    clip_pos_uniform: Option<glow::UniformLocation>,
    clip_size_uniform: Option<glow::UniformLocation>,
}

struct RenderState {
    ctx: RenderCtx,
    gl: GlState,
}

unsafe impl Send for RenderState {}
unsafe impl Sync for RenderState {}

struct Inner {
    mpv: Handle,
    render: Mutex<RenderState>,
    frame_ready: AtomicBool,
    snapshot: Mutex<Snapshot>,
    pending_audio: Mutex<Option<String>>,
    resume: Mutex<Option<f64>>,
    fallback: Mutex<Option<String>>,
    error: Mutex<Option<String>>,
    ctx: egui::Context,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn command(&self, args: &[&str]) {
        let cstrings: Vec<CString> = args
            .iter()
            .filter_map(|a| CString::new(*a).ok())
            .collect();
        if cstrings.len() != args.len() {
            return;
        }
        let mut pointers: Vec<*const c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
        pointers.push(ptr::null());
        unsafe {
            mpv_command(self.mpv, pointers.as_mut_ptr());
        }
    }

    fn set_option(&self, name: &str, value: &str) {
        let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) else {
            return;
        };
        unsafe {
            mpv_set_option_string(self.mpv, name.as_ptr(), value.as_ptr());
        }
    }
}

// ------------------------------------------------------------------ player

pub struct Player {
    inner: Option<Arc<Inner>>,
    pub volume: i32,
    pub speed: f64,
    pub muted: bool,
    pub now: Option<NowPlaying>,
}

impl Player {
    pub fn new(volume: i32, ctx: egui::Context) -> Self {
        let volume = volume.clamp(0, 130);
        let inner = Inner::start(volume, ctx, false);
        Self {
            inner,
            volume,
            speed: 1.0,
            muted: false,
            now: None,
        }
    }

    /// A muted, looping renderer for the thumbnail hover preview. It is a
    /// separate mpv instance so it never interferes with the selected video.
    pub fn new_preview(ctx: egui::Context) -> Self {
        let inner = Inner::start(0, ctx, true);
        Self {
            inner,
            volume: 0,
            speed: 1.0,
            muted: true,
            now: None,
        }
    }

    pub fn warm(&mut self) {
        // mpv is created eagerly in `new`; nothing to do.
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner
            .as_ref()
            .and_then(|inner| inner.snapshot.lock().ok().map(|s| *s))
            .unwrap_or_default()
    }

    pub fn has_rendered_frame(&self) -> bool {
        self.inner
            .as_ref()
            .map(|inner| inner.frame_ready.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn is_active(&self) -> bool {
        self.now.is_some()
    }

    pub fn begin(
        &mut self,
        video_id: &str,
        title: &str,
        channel_id: &str,
        channel_name: &str,
        duration_hint: i64,
    ) -> Result<(), String> {
        if self.inner.is_none() {
            return Err("mpv could not be started (is libmpv installed?)".to_string());
        }
        self.now = Some(NowPlaying {
            video_id: video_id.to_string(),
            title: title.to_string(),
            channel_id: channel_id.to_string(),
            channel_name: channel_name.to_string(),
            duration_hint,
        });
        Ok(())
    }

    pub fn load(&mut self, video: &str, audio: Option<&str>, video_id: &str, start_at: f64) {
        let Some(inner) = &self.inner else { return };
        inner.frame_ready.store(false, Ordering::Relaxed);
        if let Ok(mut snapshot) = inner.snapshot.lock() {
            *snapshot = Snapshot::default();
        }
        if let Ok(mut resume) = inner.resume.lock() {
            *resume = if start_at >= 5.0 { Some(start_at) } else { None };
        }
        if let Ok(mut pending) = inner.pending_audio.lock() {
            *pending = audio.map(str::to_string);
        }
        if let Ok(mut fallback) = inner.fallback.lock() {
            *fallback = Some(format!("https://www.youtube.com/watch?v={video_id}"));
        }
        let speed = self.speed.to_string();
        inner.command(&["set", "speed", &speed]);
        inner.command(&["set", "mute", if self.muted { "yes" } else { "no" }]);
        inner.command(&["loadfile", video, "replace"]);
    }

    pub fn toggle_pause(&mut self) {
        self.set_paused(!self.snapshot().paused);
    }

    pub fn set_paused(&mut self, paused: bool) {
        if let Some(inner) = &self.inner {
            // Keep the local view in sync immediately. After a reload the
            // snapshot is reset, and re-pausing an already paused stream emits
            // no mpv property change, so waiting for the event would leave the
            // overlay showing the wrong transport icon.
            if let Ok(mut snapshot) = inner.snapshot.lock() {
                snapshot.paused = paused;
            }
            // mpv's command is `set` (there is no `set_property` command;
            // that is only the C API name). Calling the wrong one failed
            // silently, which made pause and volume look dead.
            inner.command(&["set", "pause", if paused { "yes" } else { "no" }]);
        }
    }

    pub fn seek_to(&mut self, seconds: f64) {
        if let Some(inner) = &self.inner {
            let target = format!("{seconds}");
            inner.command(&["seek", &target, "absolute"]);
        }
    }

    pub fn set_volume(&mut self, volume: i32) {
        self.volume = volume.clamp(0, 130);
        if let Some(inner) = &self.inner {
            let value = self.volume.to_string();
            inner.command(&["set", "volume", &value]);
        }
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(0.25, 4.0);
        if let Some(inner) = &self.inner {
            let value = self.speed.to_string();
            inner.command(&["set", "speed", &value]);
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Some(inner) = &self.inner {
            inner.command(&["set", "mute", if muted { "yes" } else { "no" }]);
        }
    }

    pub fn stop(&mut self) {
        self.now = None;
        if let Some(inner) = &self.inner {
            inner.command(&["stop"]);
            inner.frame_ready.store(false, Ordering::Relaxed);
            if let Ok(mut snapshot) = inner.snapshot.lock() {
                *snapshot = Snapshot::default();
            }
        }
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.error.lock().ok().and_then(|mut slot| slot.take()))
    }

    pub fn quit(&mut self) {
        self.now = None;
        if let Some(inner) = &self.inner {
            inner.command(&["stop"]);
        }
    }

    /// Paint the current video frame into `rect`. Must be called from the UI
    /// thread; the GL work happens in egui's paint callback.
    pub fn paint(&self, ui: &egui::Ui, rect: egui::Rect) {
        let Some(inner) = self.inner.clone() else {
            return;
        };
        let callback = CallbackFn::new(move |info, painter| {
            inner.draw(painter.gl(), info);
        });
        ui.painter().add(egui::PaintCallback {
            rect,
            callback: Arc::new(callback),
        });
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.quit();
    }
}

// ------------------------------------------------------------------- setup

impl Inner {
    fn start(volume: i32, ctx: egui::Context, preview: bool) -> Option<Arc<Inner>> {
        let mpv = unsafe { mpv_create() };
        if mpv.is_null() {
            return None;
        }
        let inner = Arc::new(Inner {
            mpv,
            render: Mutex::new(RenderState {
                ctx: ptr::null_mut(),
                gl: GlState::default(),
            }),
            frame_ready: AtomicBool::new(false),
            snapshot: Mutex::new(Snapshot::default()),
            pending_audio: Mutex::new(None),
            resume: Mutex::new(None),
            fallback: Mutex::new(None),
            error: Mutex::new(None),
            ctx,
        });
        inner.set_option("vo", "libmpv");
        inner.set_option("hwdec", "auto-safe");
        inner.set_option("idle", "yes");
        inner.set_option("keep-open", "no");
        inner.set_option("terminal", "no");
        inner.set_option("audio-display", "no");
        inner.set_option("ytdl-format", "bv*[height<=?1080]+ba/b[height<=?1080]");
        inner.set_option("volume", &volume.to_string());
        if preview {
            inner.set_option("mute", "yes");
            inner.set_option("aid", "no");
            inner.set_option("loop-file", "inf");
        }
        if unsafe { mpv_initialize(mpv) } < 0 {
            return None;
        }
        // The render context must exist before the first frame: creating it
        // later leaves the libmpv video output initialized without a target.
        // eframe's creation context has the GL context current.
        let render = unsafe { create_render_context(mpv) };
        if !render.is_null() {
            if let Ok(mut state) = inner.render.lock() {
                state.ctx = render;
            }
            inner.set_update_callback(render);
        }
        for (id, name) in [
            (1u64, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "eof-reached"),
        ] {
            let Ok(name) = CString::new(name) else { continue };
            let format = if id == 3 || id == 4 {
                MPV_FORMAT_FLAG
            } else {
                MPV_FORMAT_DOUBLE
            };
            unsafe {
                mpv_observe_property(mpv, id, name.as_ptr(), format);
            }
        }
        let events = Arc::clone(&inner);
        std::thread::spawn(move || events.event_loop());
        Some(inner)
    }

    fn event_loop(self: Arc<Self>) {
        loop {
            let event = unsafe { mpv_wait_event(self.mpv, 0.2) };
            if event.is_null() {
                return;
            }
            let event = unsafe { &*event };
            match event.event_id {
                MPV_EVENT_PROPERTY_CHANGE => {
                    if event.data.is_null() {
                        continue;
                    }
                    let property = unsafe { &*(event.data as *const MpvEventProperty) };
                    if property.name.is_null() {
                        continue;
                    }
                    let name = unsafe { CStr::from_ptr(property.name) }
                        .to_string_lossy()
                        .to_string();
                    self.on_property(&name, property);
                }
                MPV_EVENT_FILE_LOADED => {
                    if let Ok(mut snapshot) = self.snapshot.lock() {
                        snapshot.started = false;
                    }
                    let audio = self
                        .pending_audio
                        .lock()
                        .ok()
                        .and_then(|mut slot| slot.take());
                    if let Some(audio) = audio {
                        self.command(&["audio-add", &audio, "select"]);
                    }
                    if let Some(target) = self.resume.lock().ok().and_then(|mut slot| slot.take())
                    {
                        let value = format!("{target}");
                        self.command(&["seek", &value, "absolute"]);
                    }
                    if let Ok(mut slot) = self.fallback.lock() {
                        *slot = None;
                    }
                }
                MPV_EVENT_END_FILE => {
                    let reason = if event.data.is_null() {
                        -1
                    } else {
                        unsafe { (*(event.data as *const MpvEventEndFile)).reason }
                    };
                    // 4 = error. Only then fall back to letting mpv resolve the
                    // watch URL itself; a normal EOF must not restart the video.
                    if reason == 4 {
                        let _ = self.pending_audio.lock().map(|mut slot| slot.take());
                        let fallback = self.fallback.lock().ok().and_then(|mut slot| slot.take());
                        if let Some(url) = fallback {
                            self.command(&["loadfile", &url, "replace"]);
                        } else if let Ok(mut snapshot) = self.snapshot.lock() {
                            snapshot.ended = true;
                        }
                    } else if reason == 0 {
                        if let Ok(mut snapshot) = self.snapshot.lock() {
                            snapshot.ended = true;
                        }
                    }
                }
                MPV_EVENT_SHUTDOWN => return,
                _ => {}
            }
            self.ctx.request_repaint();
        }
    }

    fn on_property(&self, name: &str, property: &MpvEventProperty) {
        if property.data.is_null() {
            return;
        }
        let Ok(mut snapshot) = self.snapshot.lock() else {
            return;
        };
        match name {
            "time-pos" => {
                snapshot.position = unsafe { *(property.data as *const f64) };
                snapshot.started = snapshot.position > 0.0;
                if snapshot.position > 0.0 {
                    snapshot.ended = false;
                }
            }
            "duration" => {
                snapshot.duration = unsafe { *(property.data as *const f64) };
            }
            "pause" => {
                snapshot.paused = unsafe { *(property.data as *const c_int) } != 0;
            }
            "eof-reached" => {
                if unsafe { *(property.data as *const c_int) } != 0 {
                    snapshot.ended = true;
                }
            }
            _ => {}
        }
    }

    fn draw(&self, gl: &Arc<glow::Context>, info: egui::PaintCallbackInfo) {
        // Render the whole video even when scrolling clips part of it. Using
        // the clipped viewport here shrank the render target and made the
        // picture look cut off as it moved behind the scroll edge.
        let ppp = info.pixels_per_point;
        let screen_height = info.screen_size_px[1] as i32;
        let full_left = (info.viewport.min.x * ppp).round() as i32;
        let full_top = (info.viewport.min.y * ppp).round() as i32;
        let full_right = (info.viewport.max.x * ppp).round() as i32;
        let full_bottom = (info.viewport.max.y * ppp).round() as i32;
        let (width, height) = (full_right - full_left, full_bottom - full_top);
        if width <= 0 || height <= 0 {
            return;
        }
        let clip_rect = info.clip_rect.intersect(info.viewport);
        let clip_left = (clip_rect.min.x * ppp).round() as i32;
        let clip_top = (clip_rect.min.y * ppp).round() as i32;
        let clip_right = (clip_rect.max.x * ppp).round() as i32;
        let clip_bottom = (clip_rect.max.y * ppp).round() as i32;
        let (clip_width, clip_height) = (clip_right - clip_left, clip_bottom - clip_top);
        if clip_width <= 0 || clip_height <= 0 {
            return;
        }
        let clip_from_bottom = screen_height - clip_bottom;
        let Ok(mut state) = self.render.lock() else {
            return;
        };

        if state.ctx.is_null() {
            state.ctx = unsafe { create_render_context(self.mpv) };
            eprintln!("embed: render context (late) = {:?}", state.ctx);
            if state.ctx.is_null() {
                if let Ok(mut error) = self.error.lock() {
                    *error = Some("mpv could not create an OpenGL renderer.".into());
                }
                return;
            }
            self.set_update_callback(state.ctx);
        }

        // Do not composite an empty FBO: before the first decoded frame it is
        // black, which is what made hover previews flash black over the poster.
        if !self.frame_ready.load(Ordering::Relaxed)
            && unsafe { mpv_render_context_update(state.ctx) } & 1 == 0
        {
            return;
        }

        if !unsafe { prepare(gl, &mut state.gl, width, height) } {
            return;
        }
        let (Some(fbo), Some(program), Some(vao), Some(tex)) =
            (state.gl.fbo, state.gl.program, state.gl.vao, state.gl.tex)
        else {
            return;
        };

        unsafe {
            let mut target = MpvOpenglFbo {
                fbo: fbo.0.get() as c_int,
                w: width,
                h: height,
                internal_format: 0,
            };
            let mut flip: c_int = 1;
            let mut params = [
                MpvRenderParam {
                    type_: RENDER_PARAM_OPENGL_FBO,
                    data: &mut target as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: RENDER_PARAM_FLIP_Y,
                    data: &mut flip as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: RENDER_PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            let rc = mpv_render_context_render(state.ctx, params.as_mut_ptr());
            if rc < 0 {
                eprintln!("embed: mpv render failed ({rc})");
            } else {
                self.frame_ready.store(true, Ordering::Relaxed);
            }
            mpv_render_context_report_swap(state.ctx);

            // mpv leaves its own viewport behind; put ours back or the frame
            // gets composited in the bottom-left corner of the window.
            gl.viewport(
                full_left,
                screen_height - full_top - height,
                width,
                height,
            );
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(clip_left, clip_from_bottom, clip_width, clip_height);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.use_program(Some(program));
            if let Some(location) = state.gl.size_uniform {
                gl.uniform_2_f32(Some(&location), width as f32, height as f32);
            }
            if let Some(location) = state.gl.radius_uniform {
                gl.uniform_1_f32(Some(&location), 8.0 * info.pixels_per_point);
            }
            if let Some(location) = state.gl.clip_pos_uniform {
                gl.uniform_2_f32(
                    Some(&location),
                    clip_left as f32,
                    clip_from_bottom as f32,
                );
            }
            if let Some(location) = state.gl.clip_size_uniform {
                gl.uniform_2_f32(
                    Some(&location),
                    clip_width as f32,
                    clip_height as f32,
                );
            }
            gl.bind_vertex_array(Some(vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_vertex_array(None);
        }
    }

    fn set_update_callback(&self, ctx: RenderCtx) {
        let userdata = Box::into_raw(Box::new(self.ctx.clone())) as *mut c_void;
        unsafe {
            mpv_render_context_set_update_callback(ctx, Some(on_render_update), userdata);
        }
    }
}

unsafe extern "C" fn on_render_update(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let ctx = &*(data as *const egui::Context);
    ctx.request_repaint();
}

unsafe fn create_render_context(mpv: Handle) -> RenderCtx {
    let api = CString::new("opengl").ok();
    let Some(api) = api else {
        return ptr::null_mut();
    };
    let mut init = MpvOpenglInitParams {
        get_proc_address: Some(get_proc_address),
        get_proc_address_ctx: ptr::null_mut(),
    };
    let mut params = [
        MpvRenderParam {
            type_: RENDER_PARAM_API_TYPE,
            data: api.as_ptr() as *mut c_void,
        },
        MpvRenderParam {
            type_: RENDER_PARAM_OPENGL_INIT_PARAMS,
            data: &mut init as *mut _ as *mut c_void,
        },
        MpvRenderParam {
            type_: RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    let mut ctx: RenderCtx = ptr::null_mut();
    if mpv_render_context_create(&mut ctx, mpv, params.as_mut_ptr()) < 0 {
        return ptr::null_mut();
    }
    ctx
}

// --------------------------------------------------------------------- GL

// 150 core is the highest guaranteed version on macOS' 3.2 core profile;
// 330 fails there and leaves the video area black.
const VERTEX_SHADER: &str = r#"#version 150
in vec2 position;
out vec2 uv;
void main() {
    uv = position * 0.5 + 0.5;
    gl_Position = vec4(position, 0.0, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 150
in vec2 uv;
out vec4 color;
uniform sampler2D frame;
uniform vec2 size;
uniform float radius;
uniform vec2 clip_pos;
uniform vec2 clip_size;
float rounded(vec2 p, vec2 s, float r) {
    vec2 half_size = s * 0.5;
    vec2 q = abs(p - half_size) - (half_size - vec2(r));
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
    // Match the app's 8px rounded corners without a second geometry pass.
    vec2 p = uv * size;
    if (rounded(p, size, radius) > 0.0) {
        discard;
    }
    // When scrolling clips the video, round the visible edge as well so the
    // cut-off top does not become a square corner.
    vec2 visible_p = gl_FragCoord.xy - clip_pos;
    if (rounded(visible_p, clip_size, radius) > 0.0) {
        discard;
    }
    color = texture(frame, uv);
}
"#;

unsafe fn prepare(
    gl: &glow::Context,
    state: &mut GlState,
    width: i32,
    height: i32,
) -> bool {
    if state.program.is_none() {
        let program = compile_program(gl);
        let Some(program) = program else {
            return false;
        };
        state.program = Some(program);
        state.size_uniform = gl.get_uniform_location(program, "size");
        state.radius_uniform = gl.get_uniform_location(program, "radius");
        state.clip_pos_uniform = gl.get_uniform_location(program, "clip_pos");
        state.clip_size_uniform = gl.get_uniform_location(program, "clip_size");

        let vao = gl.create_vertex_array();
        let vbo = gl.create_buffer();
        let (Ok(vao), Ok(vbo)) = (vao, vbo) else {
            return false;
        };
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes: Vec<u8> = quad.iter().flat_map(|value| value.to_ne_bytes()).collect();
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_vertex_array(None);
        state.vao = Some(vao);
        state.vbo = Some(vbo);
    }

    if state.fbo.is_none() || state.size != (width, height) {
        if let Some(fbo) = state.fbo.take() {
            gl.delete_framebuffer(fbo);
        }
        if let Some(tex) = state.tex.take() {
            gl.delete_texture(tex);
        }
        let texture = gl.create_texture();
        let fbo = gl.create_framebuffer();
        let (Ok(texture), Ok(fbo)) = (texture, fbo) else {
            return false;
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width,
            height,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            eprintln!("embed: framebuffer incomplete ({status:#x})");
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        state.fbo = Some(fbo);
        state.tex = Some(texture);
        state.size = (width, height);
    }
    true
}

unsafe fn compile_program(gl: &glow::Context) -> Option<glow::Program> {
    let vertex = gl.create_shader(glow::VERTEX_SHADER).ok()?;
    gl.shader_source(vertex, VERTEX_SHADER);
    gl.compile_shader(vertex);
    if !gl.get_shader_compile_status(vertex) {
        eprintln!("embed: vertex shader failed: {}", gl.get_shader_info_log(vertex));
        return None;
    }
    let fragment = gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
    gl.shader_source(fragment, FRAGMENT_SHADER);
    gl.compile_shader(fragment);
    if !gl.get_shader_compile_status(fragment) {
        eprintln!(
            "embed: fragment shader failed: {}",
            gl.get_shader_info_log(fragment)
        );
        gl.delete_shader(vertex);
        return None;
    }
    let program = gl.create_program().ok()?;
    gl.attach_shader(program, vertex);
    gl.attach_shader(program, fragment);
    gl.link_program(program);
    gl.delete_shader(vertex);
    gl.delete_shader(fragment);
    if gl.get_program_link_status(program) {
        Some(program)
    } else {
        eprintln!("embed: program link failed: {}", gl.get_program_info_log(program));
        gl.delete_program(program);
        None
    }
}
