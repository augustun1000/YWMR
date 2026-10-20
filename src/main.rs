use std::ffi::{c_void, CString};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CONNECTOR_ID: u32 = 102;
const DRM_CAP_DUMB_BUFFER: u64 = 1;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
const PIXMAN_X8R8G8B8: c_int = (32 << 24) | (2 << 16) | (8 << 8) | (8 << 4) | 8;
const PIXMAN_OP_SRC: c_int = 1;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const FLIPS: usize = 120;
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct LibSeat { _opaque: [u8; 0] }
#[repr(C)]
struct LibSeatListener {
    enable_seat: Option<unsafe extern "C" fn(*mut LibSeat, *mut c_void)>,
    disable_seat: Option<unsafe extern "C" fn(*mut LibSeat, *mut c_void)>,
}
struct SeatState { active: AtomicBool }
unsafe extern "C" fn enable_seat(_seat: *mut LibSeat, data: *mut c_void) {
    if !data.is_null() {
        unsafe { &*(data as *const SeatState) }.active.store(true, Ordering::SeqCst);
    }
}
unsafe extern "C" fn disable_seat(seat: *mut LibSeat, data: *mut c_void) {
    if !data.is_null() {
        unsafe { &*(data as *const SeatState) }.active.store(false, Ordering::SeqCst);
    }
    // Obligatorio: confirmar enseguida al gestor de sesiones.
    unsafe { libseat_disable_seat(seat); }
}
static LISTENER: LibSeatListener = LibSeatListener {
    enable_seat: Some(enable_seat),
    disable_seat: Some(disable_seat),
};

#[link(name = "seat")]
unsafe extern "C" {
    fn libseat_open_seat(listener: *const LibSeatListener, userdata: *mut c_void) -> *mut LibSeat;
    fn libseat_close_seat(seat: *mut LibSeat) -> c_int;
    fn libseat_dispatch(seat: *mut LibSeat, timeout: c_int) -> c_int;
    fn libseat_disable_seat(seat: *mut LibSeat) -> c_int;
    fn libseat_open_device(seat: *mut LibSeat, path: *const c_char, fd: *mut c_int) -> c_int;
    fn libseat_close_device(seat: *mut LibSeat, device_id: c_int) -> c_int;
}

struct Seat { ptr: *mut LibSeat, state: Box<SeatState> }
impl Seat {
    fn open() -> io::Result<Self> {
        let mut state = Box::new(SeatState { active: AtomicBool::new(false) });
        let ptr = unsafe {
            libseat_open_seat(&LISTENER, (&mut *state as *mut SeatState).cast::<c_void>())
        };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let seat = Self { ptr, state };
        let limit = Instant::now() + Duration::from_secs(3);
        while !seat.active() && Instant::now() < limit {
            seat.dispatch()?;
            thread::sleep(Duration::from_millis(20));
        }
        if !seat.active() { return Err(io::Error::other("libseat no activó la sesión")); }
        Ok(seat)
    }
    fn active(&self) -> bool { self.state.active.load(Ordering::SeqCst) }
    fn dispatch(&self) -> io::Result<()> {
        if unsafe { libseat_dispatch(self.ptr, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Seat {
    fn drop(&mut self) { unsafe { libseat_close_seat(self.ptr); } }
}

unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
}
#[repr(C)]
struct PollFd { fd: c_int, events: i16, revents: i16 }
extern "C" fn request_stop(_: c_int) { STOP_REQUESTED.store(true, Ordering::SeqCst); }

struct DrmDevice<'a> { seat: &'a Seat, fd: RawFd, device_id: c_int }
impl<'a> DrmDevice<'a> {
    fn open(seat: &'a Seat, path: &str) -> io::Result<Self> {
        let path = CString::new(path).map_err(|_| io::Error::other("Ruta con NUL"))?;
        let mut fd = -1;
        let device_id = unsafe { libseat_open_device(seat.ptr, path.as_ptr(), &mut fd) };
        if device_id < 0 { return Err(io::Error::last_os_error()); }
        if fd < 0 {
            unsafe { libseat_close_device(seat.ptr, device_id); }
            return Err(io::Error::other("libseat devolvió un fd inválido"));
        }
        Ok(Self { seat, fd, device_id })
    }
}
impl Drop for DrmDevice<'_> {
    fn drop(&mut self) {
        unsafe {
            libseat_close_device(self.seat.ptr, self.device_id);
            close(self.fd);
        }
    }
}

#[repr(C)]
struct DrmResources {
    count_fbs: c_int, fbs: *mut u32,
    count_crtcs: c_int, crtcs: *mut u32,
    count_connectors: c_int, connectors: *mut u32,
    count_encoders: c_int, encoders: *mut u32,
    min_width: u32, max_width: u32, min_height: u32, max_height: u32,
}
#[repr(C)]
#[derive(Copy, Clone)]
struct DrmMode {
    clock: u32,
    hdisplay: u16, hsync_start: u16, hsync_end: u16, htotal: u16, hskew: u16,
    vdisplay: u16, vsync_start: u16, vsync_end: u16, vtotal: u16, vscan: u16,
    vrefresh: u32, flags: u32, mode_type: u32, name: [c_char; 32],
}
#[repr(C)]
struct DrmConnector {
    connector_id: u32, encoder_id: u32, connector_type: u32, connector_type_id: u32,
    connection: c_int, mm_width: u32, mm_height: u32, subpixel: c_int,
    count_modes: c_int, modes: *mut DrmMode,
    count_props: c_int, props: *mut u32, prop_values: *mut u64,
    count_encoders: c_int, encoders: *mut u32,
}
#[repr(C)]
struct DrmEncoder {
    encoder_id: u32, encoder_type: u32, crtc_id: u32,
    possible_crtcs: u32, possible_clones: u32,
}
#[repr(C)]
struct DrmCrtc {
    crtc_id: u32, buffer_id: u32, x: u32, y: u32,
    width: u32, height: u32, mode_valid: c_int, mode: DrmMode, gamma_size: c_int,
}

#[link(name = "drm")]
unsafe extern "C" {
    fn drmGetCap(fd: c_int, capability: u64, value: *mut u64) -> c_int;
    fn drmModeGetResources(fd: c_int) -> *mut DrmResources;
    fn drmModeFreeResources(ptr: *mut DrmResources);
    fn drmModeGetConnector(fd: c_int, connector_id: u32) -> *mut DrmConnector;
    fn drmModeFreeConnector(ptr: *mut DrmConnector);
    fn drmModeGetEncoder(fd: c_int, encoder_id: u32) -> *mut DrmEncoder;
    fn drmModeFreeEncoder(ptr: *mut DrmEncoder);
    fn drmModeGetCrtc(fd: c_int, crtc_id: u32) -> *mut DrmCrtc;
    fn drmModeFreeCrtc(ptr: *mut DrmCrtc);
    fn drmModeSetCrtc(fd: c_int, crtc_id: u32, fb_id: u32,
        x: u32, y: u32, connectors: *const u32, count: c_int, mode: *const DrmMode) -> c_int;
    fn drmModeCreateDumbBuffer(fd: c_int, width: u32, height: u32, bpp: u32,
        flags: u32, handle: *mut u32, pitch: *mut u32, size: *mut u64) -> c_int;
    fn drmModeMapDumbBuffer(fd: c_int, handle: u32, offset: *mut u64) -> c_int;
    fn drmModeDestroyDumbBuffer(fd: c_int, handle: u32) -> c_int;
    fn drmModeAddFB2(fd: c_int, width: u32, height: u32, format: u32,
        handles: *const u32, pitches: *const u32, offsets: *const u32,
        buf_id: *mut u32, flags: u32) -> c_int;
    fn drmModeRmFB(fd: c_int, id: u32) -> c_int;
    fn drmModePageFlip(fd: c_int, crtc_id: u32, fb_id: u32,
        flags: u32, user_data: *mut c_void) -> c_int;
    fn drmHandleEvent(fd: c_int, context: *mut DrmEventContext) -> c_int;
}

// libdrm event context ABI (versión 2). Evitamos handlers nuevos
// porque la estructura debe corresponder exactamente a su versión.
type DrmEventHandler = unsafe extern "C" fn(c_int, u32, u32, u32, *mut c_void);
#[repr(C)]
struct DrmEventContext {
    version: c_int,
    vblank_handler: Option<DrmEventHandler>,
    page_flip_handler: Option<DrmEventHandler>,
}
struct FlipState {
    completed: AtomicBool,
    sequence: AtomicU32,
    timestamp_us: AtomicU64,
}
unsafe extern "C" fn on_page_flip(
    _fd: c_int, sequence: u32, sec: u32, usec: u32, data: *mut c_void,
) {
    if data.is_null() { return; }
    let state = unsafe { &*(data as *const FlipState) };
    state.sequence.store(sequence, Ordering::SeqCst);
    state.timestamp_us.store(u64::from(sec) * 1_000_000 + u64::from(usec), Ordering::SeqCst);
    state.completed.store(true, Ordering::SeqCst);
}

// Este bucle atiende tanto la sesión como el evento KMS. Una petición Ctrl+C
// durante un flip NO interrumpe la espera: el buffer debe mantenerse vivo
// hasta el callback del kernel.
fn wait_flip(fd: RawFd, seat: &Seat, state: &FlipState) -> io::Result<()> {
    let mut context = DrmEventContext {
        version: 2,
        vblank_handler: None,
        page_flip_handler: Some(on_page_flip),
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        seat.dispatch()?;
        if !seat.active() { return Err(io::Error::other("La sesión perdió DRM durante un page flip")); }
        if state.completed.load(Ordering::SeqCst) { return Ok(()); }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                "El kernel no notificó el page flip en 3 segundos"));
        }
        let mut pfd = PollFd { fd, events: POLLIN, revents: 0 };
        let polled = unsafe { poll(&mut pfd, 1, 25) };
        if polled < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue; }
            return Err(e);
        }
        if polled == 0 { continue; }
        if (pfd.revents & (POLLERR | POLLHUP)) != 0 {
            return Err(io::Error::other("Error o cierre del dispositivo DRM"));
        }
        if (pfd.revents & POLLIN) != 0 {
            let r = unsafe { drmHandleEvent(fd, &mut context) };
            if r != 0 { return Err(drm_error("drmHandleEvent", r)); }
        }
    }
}

fn drm_error(name: &str, code: c_int) -> io::Error {
    let source = if code < -1 { io::Error::from_raw_os_error(-code) }
                 else { io::Error::last_os_error() };
    io::Error::new(source.kind(), format!("{name}: {source} (ret={code})"))
}

struct SelectedOutput { mode: DrmMode, crtc_id: u32 }

fn select_output(fd: RawFd) -> io::Result<SelectedOutput> {
    let res_ptr = unsafe { drmModeGetResources(fd) };
    if res_ptr.is_null() { return Err(io::Error::last_os_error()); }
    let res = unsafe { &*res_ptr };
    let output = (|| -> io::Result<SelectedOutput> {
        if res.count_crtcs <= 0 || res.crtcs.is_null() {
            return Err(io::Error::other("No hay CRTCs disponibles"));
        }
        let crtc_ids = unsafe { std::slice::from_raw_parts(res.crtcs, res.count_crtcs as usize) };
        let ptr = unsafe { drmModeGetConnector(fd, CONNECTOR_ID) };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let connector = unsafe { &*ptr };
        let answer = (|| -> io::Result<SelectedOutput> {
            if connector.connection != 1 || connector.count_modes <= 0 || connector.modes.is_null() {
                return Err(io::Error::other("eDP-1 no está conectado o no tiene modos"));
            }
            let modes = unsafe { std::slice::from_raw_parts(connector.modes, connector.count_modes as usize) };
            let mode = *modes.iter().find(|m| m.mode_type & DRM_MODE_TYPE_PREFERRED != 0)
                .unwrap_or(&modes[0]);
            if (mode.hdisplay, mode.vdisplay) != (1366, 768) {
                return Err(io::Error::other(format!("Modo inesperado: {}x{} (se esperaba 1366x768)", mode.hdisplay, mode.vdisplay)));
            }
            let mut encoder_ids = Vec::new();
            if connector.encoder_id != 0 { encoder_ids.push(connector.encoder_id); }
            if connector.count_encoders > 0 && !connector.encoders.is_null() {
                let encoders = unsafe { std::slice::from_raw_parts(connector.encoders, connector.count_encoders as usize) };
                for &id in encoders { if !encoder_ids.contains(&id) { encoder_ids.push(id); } }
            }
            // Preferimos el CRTC que el conector ya tenía asignado.
            for id in encoder_ids {
                let eptr = unsafe { drmModeGetEncoder(fd, id) };
                if eptr.is_null() { continue; }
                let e = unsafe { &*eptr };
                let chosen = if e.crtc_id != 0 && crtc_ids.contains(&e.crtc_id) {
                    Some(e.crtc_id)
                } else {
                    crtc_ids.iter().enumerate()
                        .find(|(i, _)| *i < 32 && (e.possible_crtcs & (1u32 << *i)) != 0)
                        .map(|(_, id)| *id)
                };
                unsafe { drmModeFreeEncoder(eptr) };
                if let Some(crtc_id) = chosen { return Ok(SelectedOutput { mode, crtc_id }); }
            }
            Err(io::Error::other("No se encontró CRTC compatible con eDP-1"))
        })();
        unsafe { drmModeFreeConnector(ptr) };
        answer
    })();
    unsafe { drmModeFreeResources(res_ptr) };
    output
}

struct SavedCrtc { crtc_id: u32, fb_id: u32, x: u32, y: u32, mode: Option<DrmMode>, connectors: Vec<u32> }
impl SavedCrtc {
    fn capture(fd: RawFd, crtc_id: u32) -> io::Result<Self> {
        let ptr = unsafe { drmModeGetCrtc(fd, crtc_id) };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let c = unsafe { &*ptr };
        let mut snap = Self {
            crtc_id: c.crtc_id, fb_id: c.buffer_id, x: c.x, y: c.y,
            mode: if c.mode_valid != 0 { Some(c.mode) } else { None },
            connectors: Vec::new(),
        };
        unsafe { drmModeFreeCrtc(ptr) };
        let res_ptr = unsafe { drmModeGetResources(fd) };
        if res_ptr.is_null() { return Err(io::Error::last_os_error()); }
        let res = unsafe { &*res_ptr };
        if res.count_connectors > 0 && !res.connectors.is_null() {
            let ids = unsafe { std::slice::from_raw_parts(res.connectors, res.count_connectors as usize) };
            for &id in ids {
                let con = unsafe { drmModeGetConnector(fd, id) };
                if con.is_null() { continue; }
                let en_id = unsafe { (*con).encoder_id };
                if en_id != 0 {
                    let en = unsafe { drmModeGetEncoder(fd, en_id) };
                    if !en.is_null() {
                        if unsafe { (*en).crtc_id } == crtc_id { snap.connectors.push(id); }
                        unsafe { drmModeFreeEncoder(en) };
                    }
                }
                unsafe { drmModeFreeConnector(con) };
            }
        }
        unsafe { drmModeFreeResources(res_ptr) };
        if snap.fb_id != 0 && snap.mode.is_some() && snap.connectors.is_empty() {
            return Err(io::Error::other("CRTC activo sin conectores recuperables: se cancela por seguridad"));
        }
        Ok(snap)
    }
    fn restore(&self, fd: RawFd) -> io::Result<()> {
        let (fb, x, y, connectors, mode) = match self.mode.as_ref() {
            Some(mode) if self.fb_id != 0 =>
                (self.fb_id, self.x, self.y, self.connectors.as_slice(), mode as *const DrmMode),
            _ => (0, 0, 0, &[][..], ptr::null()),
        };
        let conn_ptr = if connectors.is_empty() { ptr::null() } else { connectors.as_ptr() };
        let ret = unsafe { drmModeSetCrtc(fd, self.crtc_id, fb, x, y,
            conn_ptr, connectors.len() as c_int, mode) };
        if ret != 0 { return Err(drm_error("restaurar drmModeSetCrtc", ret)); }
        Ok(())
    }
}

struct DumbBuffer { fd: RawFd, handle: u32, pitch: u32, len: usize, map: *mut u8, width: u32, height: u32 }
impl DumbBuffer {
    fn create(fd: RawFd, width: u32, height: u32) -> io::Result<Self> {
        let (mut handle, mut pitch, mut size) = (0, 0, 0u64);
        let ret = unsafe { drmModeCreateDumbBuffer(fd, width, height, 32, 0,
            &mut handle, &mut pitch, &mut size) };
        if ret != 0 { return Err(drm_error("drmModeCreateDumbBuffer", ret)); }
        let mut buf = Self { fd, handle, pitch, len: 0, map: ptr::null_mut(), width, height };
        let len = usize::try_from(size).map_err(|_| io::Error::other("Tamaño fuera de rango"))?;
        if pitch < width.saturating_mul(4) || pitch > c_int::MAX as u32 || pitch % 4 != 0
            || size < u64::from(pitch) * u64::from(height) {
            return Err(io::Error::other("Pitch o tamaño inválido"));
        }
        let mut offset: u64 = 0;
        let ret = unsafe { drmModeMapDumbBuffer(fd, handle, &mut offset) };
        if ret != 0 { return Err(drm_error("drmModeMapDumbBuffer", ret)); }
        let offset = i64::try_from(offset).map_err(|_| io::Error::other("Offset demasiado grande"))?;
        let p = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset) };
        if p == usize::MAX as *mut c_void { return Err(io::Error::last_os_error()); }
        buf.map = p.cast::<u8>();
        buf.len = len;
        Ok(buf)
    }
}
impl Drop for DumbBuffer {
    fn drop(&mut self) {
        if !self.map.is_null() { unsafe { munmap(self.map.cast::<c_void>(), self.len); } }
        if self.handle != 0 { unsafe { drmModeDestroyDumbBuffer(self.fd, self.handle); } }
    }
}

struct Framebuffer<'a> { fd: RawFd, id: u32, _buffer: &'a DumbBuffer }
impl<'a> Framebuffer<'a> {
    fn new(fd: RawFd, buffer: &'a DumbBuffer) -> io::Result<Self> {
        let handles = [buffer.handle, 0, 0, 0];
        let pitches = [buffer.pitch, 0, 0, 0];
        let offsets = [0u32; 4];
        let mut id = 0;
        let ret = unsafe { drmModeAddFB2(fd, buffer.width, buffer.height, DRM_FORMAT_XRGB8888,
            handles.as_ptr(), pitches.as_ptr(), offsets.as_ptr(), &mut id, 0) };
        if ret != 0 { return Err(drm_error("drmModeAddFB2", ret)); }
        Ok(Self { fd, id, _buffer: buffer })
    }
}
impl Drop for Framebuffer<'_> {
    fn drop(&mut self) {
        if unsafe { drmModeRmFB(self.fd, self.id) } != 0 {
            eprintln!("Aviso: no se pudo retirar framebuffer {}", self.id);
        }
    }
}

#[repr(C)]
struct PixmanImage { _opaque: [u8; 0] }
#[repr(C)]
struct PixmanColor { red: u16, green: u16, blue: u16, alpha: u16 }
#[repr(C)]
struct PixmanBox32 { x1: i32, y1: i32, x2: i32, y2: i32 }
#[link(name = "pixman-1")]
unsafe extern "C" {
    fn pixman_image_create_bits(fmt: c_int, width: c_int, height: c_int,
        bits: *mut u32, stride: c_int) -> *mut PixmanImage;
    fn pixman_image_fill_boxes(op: c_int, dest: *mut PixmanImage,
        color: *const PixmanColor, n_boxes: c_int, boxes: *const PixmanBox32) -> c_int;
    fn pixman_image_unref(image: *mut PixmanImage) -> c_int;
}
fn color(r: u8, g: u8, b: u8) -> PixmanColor {
    PixmanColor { red: u16::from(r) * 257, green: u16::from(g) * 257,
        blue: u16::from(b) * 257, alpha: 65535 }
}
fn box_fill(image: *mut PixmanImage, (x1,y1,x2,y2): (i32,i32,i32,i32), c: PixmanColor) -> io::Result<()> {
    let rect = PixmanBox32 { x1,y1,x2,y2 };
    if unsafe { pixman_image_fill_boxes(PIXMAN_OP_SRC, image, &c, 1, &rect) } == 0 {
        return Err(io::Error::other("pixman_image_fill_boxes falló"));
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect { x1: i32, y1: i32, x2: i32, y2: i32 }
impl Rect {
    fn full(w: u32, h: u32) -> Self {
        Self { x1: 0, y1: 0, x2: w as i32, y2: h as i32 }
    }
    fn area(self) -> u64 {
        ((self.x2-self.x1).max(0) as u64) * ((self.y2-self.y1).max(0) as u64)
    }
    fn intersection(self, b: Self) -> Option<Self> {
        let r = Self {
            x1: self.x1.max(b.x1), y1: self.y1.max(b.y1),
            x2: self.x2.min(b.x2), y2: self.y2.min(b.y2),
        };
        (r.x1 < r.x2 && r.y1 < r.y2).then_some(r)
    }
    fn union(self, b: Self) -> Self {
        Self {
            x1: self.x1.min(b.x1), y1: self.y1.min(b.y1),
            x2: self.x2.max(b.x2), y2: self.y2.max(b.y2),
        }
    }
    fn coords(self) -> (i32,i32,i32,i32) {
        (self.x1, self.y1, self.x2, self.y2)
    }
}

fn bar_rect(width: u32, frame: usize) -> Rect {
    // El programa exige 1366x768, por lo que el recorrido es positivo.
    let travel = (width as usize).saturating_sub(200).max(1);
    let x = (frame.wrapping_mul(8) % travel) as i32;
    Rect { x1: x, y1: 100, x2: x + 200, y2: 490 }
}

fn damage_rects(width: u32, height: u32, frame: usize, previous: Option<usize>) -> Vec<Rect> {
    let screen = Rect::full(width, height);
    let current = bar_rect(width, frame);
    match previous {
        None => vec![screen],
        Some(old_frame) => {
            let old = bar_rect(width, old_frame);
            if old == current { return vec![]; }
            // Si los rectángulos se solapan, una envolvente pequeña reduce
            // las llamadas a Pixman. En el salto al borde evitamos una gran
            // envolvente y procesamos cada extremo por separado.
            if old.intersection(current).is_some() {
                vec![old.union(current).intersection(screen).unwrap()]
            } else {
                [old, current].into_iter()
                    .filter_map(|r| r.intersection(screen)).collect()
            }
        }
    }
}

#[derive(Default)]
struct DrawStats {
    pixels: u64,
    rects: usize,
    elapsed: Duration,
}

fn render_scene(
    image: *mut PixmanImage, width: u32, height: u32,
    frame: usize, previous: Option<usize>,
) -> io::Result<DrawStats> {
    let began = Instant::now();
    let damage = damage_rects(width, height, frame, previous);
    let current = bar_rect(width, frame);
    let fixed = Rect { x1: 400, y1: 250, x2: 920, y2: 550 };
    let mut pixels = 0u64;
    for r in &damage {
        pixels += r.area();
        // Redibujamos la escena respetando el orden de capas original:
        // fondo -> franja turquesa -> rectángulo naranja.
        box_fill(image, r.coords(), color(22,38,56))?;
        if let Some(i) = r.intersection(current) {
            box_fill(image, i.coords(), color(14,116,144))?;
        }
        if let Some(i) = r.intersection(fixed) {
            box_fill(image, i.coords(), color(245,158,11))?;
        }
    }
    Ok(DrawStats { pixels, rects: damage.len(), elapsed: began.elapsed() })
}

fn draw(buf: &DumbBuffer, frame: usize, previous: Option<usize>) -> io::Result<DrawStats> {
    let p = unsafe { pixman_image_create_bits(PIXMAN_X8R8G8B8,
        buf.width as c_int, buf.height as c_int, buf.map.cast::<u32>(), buf.pitch as c_int) };
    if p.is_null() { return Err(io::Error::other("pixman_image_create_bits falló")); }
    let result = render_scene(p, buf.width, buf.height, frame, previous);
    unsafe { pixman_image_unref(p); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sin DRM: dibujamos sobre memoria RAM convencional, con Pixman, y
    // comparamos byte a byte el resultado incremental con el frame completo.
    fn draw_ram(data: &mut [u32], w: u32, h: u32, frame: usize, prev: Option<usize>) {
        let img = unsafe { pixman_image_create_bits(
            PIXMAN_X8R8G8B8, w as c_int, h as c_int, data.as_mut_ptr(), (w*4) as c_int,
        ) };
        assert!(!img.is_null());
        render_scene(img, w, h, frame, prev).unwrap();
        unsafe { pixman_image_unref(img); }
    }

    #[test]
    fn damage_produce_la_misma_imagen_que_un_redibujado_total() {
        let (w,h)=(1366u32,768u32);
        let count=(w*h) as usize;
        let mut incremental=vec![0u32;count];
        let mut full=vec![0u32;count];
        draw_ram(&mut incremental,w,h,0,None);
        let mut last=0usize;
        // Incluye saltos, intersecciones con el rectángulo fijo y wraparound.
        for frame in [2,4,8,20,50,75,100,145,146,147,148,200,290,450,582,583,584] {
            draw_ram(&mut incremental,w,h,frame,Some(last));
            draw_ram(&mut full,w,h,frame,None);
            assert_eq!(incremental,full,"Diferencia en frame {frame}");
            last=frame;
        }
    }

    #[test]
    fn ambos_buffers_alternados_son_correctos() {
        let (w,h) = (1366u32,768u32);
        let count=(w*h) as usize;
        let mut buffers=[vec![0u32;count],vec![0u32;count]];
        let mut last=[0usize,0usize];
        for b in &mut buffers { draw_ram(b,w,h,0,None); }
        let mut expected=vec![0u32;count];
        for frame in 1..=40usize {
            let target=frame % 2; // A comienza visible (0); B recibe frame 1.
            draw_ram(&mut buffers[target],w,h,frame,Some(last[target]));
            last[target]=frame;
            draw_ram(&mut expected,w,h,frame,None);
            assert_eq!(buffers[target],expected,"Buffer {target}, frame {frame}");
        }
    }

    #[test]
    fn ahorra_area_y_no_redibuja_sin_cambios() {
        let full = 1366u64*768;
        let damage = damage_rects(1366,768,2,Some(0));
        let pixels: u64=damage.iter().map(|r|r.area()).sum();
        assert!(pixels > 0 && pixels < full/5);
        assert!(damage_rects(1366,768,2,Some(2)).is_empty());
    }
}

struct RestoreGuard<'a> { seat: &'a Seat, fd: RawFd, saved: SavedCrtc, showing: bool }
impl<'a> RestoreGuard<'a> {
    fn new(seat: &'a Seat, fd: RawFd, saved: SavedCrtc) -> Self {
        Self { seat, fd, saved, showing: false }
    }
    fn present(&mut self, fb: u32, mode: &DrmMode) -> io::Result<()> {
        if !self.seat.active() { return Err(io::Error::other("Sesión inactiva")); }
        let connector = [CONNECTOR_ID];
        let ret = unsafe { drmModeSetCrtc(self.fd, self.saved.crtc_id, fb, 0, 0,
            connector.as_ptr(), 1, mode) };
        if ret != 0 { return Err(drm_error("presentar drmModeSetCrtc", ret)); }
        self.showing = true;
        Ok(())
    }
    fn restore(&mut self) -> io::Result<()> {
        if !self.showing { return Ok(()); }
        if !self.seat.active() {
            return Err(io::Error::other("La sesión cambió; no es seguro restaurar un CRTC desde sesión inactiva"));
        }
        self.saved.restore(self.fd)?;
        self.showing = false;
        Ok(())
    }
}
impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        if self.showing {
            if let Err(e) = self.restore() { eprintln!("ADVERTENCIA: no se restauró KMS: {e}"); }
        }
    }
}

fn require_real_tty() -> io::Result<()> {
    let target = std::fs::read_link("/proc/self/fd/0")?;
    let path = target.to_string_lossy();
    let vt = path.strip_prefix("/dev/tty")
        .map(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if !vt {
        return Err(io::Error::other(format!(
            "--show exige una TTY Linux real (/dev/ttyN), no una terminal gráfica: stdin={path}"
        )));
    }
    Ok(())
}

fn show(path: &str) -> io::Result<()> {
    require_real_tty()?;
    unsafe { signal(2, request_stop); signal(15, request_stop); }
    let seat = Seat::open()?;
    let dev = DrmDevice::open(&seat, path)?;
    let fd = dev.fd;
    let mut cap = 0u64;
    let ret = unsafe { drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &mut cap) };
    if ret != 0 { return Err(drm_error("drmGetCap", ret)); }
    if cap == 0 { return Err(io::Error::other("GPU sin soporte dumb buffer")); }
    let output = select_output(fd)?;
    let saved = SavedCrtc::capture(fd, output.crtc_id)?;
    eprintln!("eDP-1 (conector {CONNECTOR_ID}) CRTC {}: {}x{} @ {} Hz",
        output.crtc_id, output.mode.hdisplay, output.mode.vdisplay, output.mode.vrefresh);

    let w = u32::from(output.mode.hdisplay);
    let h = u32::from(output.mode.vdisplay);
    let a = DumbBuffer::create(fd, w, h)?;
    let b = DumbBuffer::create(fd, w, h)?;
    draw(&a, 0, None)?;
    draw(&b, 0, None)?;
    let fb_a = Framebuffer::new(fd, &a)?;
    let fb_b = Framebuffer::new(fd, &b)?;
    eprintln!("Buffers independientes: {} y {} ({} bytes cada uno)",
        fb_a.id, fb_b.id, a.len);

    let mut guard = RestoreGuard::new(&seat, fd, saved);
    guard.present(fb_a.id, &output.mode)?;
    // El framebuffer A está visible. B está disponible para dibujo.
    let buffers = [&a, &b];
    let ids = [fb_a.id, fb_b.id];
    let mut showing = 0usize;
    let mut buffer_frames = [Some(0usize), Some(0usize)];
    let mut total_damage_pixels = 0u64;
    let mut total_render_ns = 0u128;
    let mut total_damage_rects = 0usize;
    let mut flip_state = Box::new(FlipState {
        completed: AtomicBool::new(false),
        sequence: AtomicU32::new(0),
        timestamp_us: AtomicU64::new(0),
    });
    let user_data = (&mut *flip_state as *mut FlipState).cast::<c_void>();
    let mut prev_timestamp_us: Option<u64> = None;
    let mut sum_delta_us = 0u64;
    let mut measured_intervals = 0usize;
    let started = Instant::now();
    let mut successful_flips = 0usize;
    eprintln!("Realizando hasta {FLIPS} page flips; Ctrl+C solicita detener tras el flip pendiente.");

    let flips_result = (|| -> io::Result<()> {
        for frame in 1..=FLIPS {
            seat.dispatch()?;
            if !seat.active() || STOP_REQUESTED.load(Ordering::SeqCst) { break; }
            let next = showing ^ 1;
            // No se modifica el buffer que todavía está en pantalla.
            let stats = draw(buffers[next], frame, buffer_frames[next])?;
            buffer_frames[next] = Some(frame);
            total_damage_pixels += stats.pixels;
            total_damage_rects += stats.rects;
            total_render_ns += stats.elapsed.as_nanos();
            flip_state.completed.store(false, Ordering::SeqCst);
            let ret = unsafe { drmModePageFlip(fd, output.crtc_id, ids[next],
                DRM_MODE_PAGE_FLIP_EVENT, user_data) };
            if ret != 0 { return Err(drm_error("drmModePageFlip", ret)); }
            // IMPORTANTE: esperamos el evento antes de intercambiar los índices.
            wait_flip(fd, &seat, &flip_state)?;
            showing = next;
            successful_flips += 1;
            let ts = flip_state.timestamp_us.load(Ordering::SeqCst);
            if let Some(prev) = prev_timestamp_us {
                if ts > prev { sum_delta_us += ts - prev; measured_intervals += 1; }
            }
            prev_timestamp_us = Some(ts);
            if frame % 30 == 0 {
                eprintln!("Flip {:3}: framebuffer={} secuencia={} | daño: {} rect(s), {} píxeles | render: {:.3} ms",
                    frame, ids[showing], flip_state.sequence.load(Ordering::SeqCst),
                    stats.rects, stats.pixels, stats.elapsed.as_secs_f64()*1000.0);
            }
        }
        Ok(())
    })();

    eprintln!("Flips completados: {} | Duración de la prueba: {:.3} s",
        successful_flips, started.elapsed().as_secs_f64());
    if measured_intervals > 0 {
        let avg_ms = (sum_delta_us as f64 / measured_intervals as f64) / 1000.0;
        eprintln!("Separación media entre eventos KMS: {:.3} ms ({:.2} Hz)",
            avg_ms, 1000.0 / avg_ms);
    }
    if successful_flips > 0 {
        let baseline = u64::from(w) * u64::from(h) * successful_flips as u64;
        eprintln!("Daño total: {} píxeles / {} (pantallas completas); {:.1}% menos superficie por redibujar",
            total_damage_pixels, baseline, 100.0 * (1.0 - total_damage_pixels as f64 / baseline as f64));
        eprintln!("Rectángulos de daño: {} | tiempo medio dibujando con Pixman: {:.3} ms/frame",
            total_damage_rects, total_render_ns as f64 / successful_flips as f64 / 1_000_000.0);
    }
    // Si falló la espera del flip, es posible que aún haya uno pendiente.
    // Intentamos restaurar solamente si conservamos el control del seat.
    // Los buffers y el puntero al callback permanecen vivos hasta cerrar DRM.
    if seat.active() {
        if let Err(e) = guard.restore() {
            eprintln!("ADVERTENCIA: fallo al restaurar el CRTC: {e}");
            if flips_result.is_ok() { return Err(e); }
        } else {
            eprintln!("Modo anterior restaurado.");
        }
    } else {
        eprintln!("Sesión inactiva: no se intenta un modeset fuera de su sesión.");
    }
    // Nota: drop ordenado y explícito; el fd se cierra tras destruir los fb.
    drop(guard);
    drop(fb_b);
    drop(fb_a);
    drop(b);
    drop(a);
    drop(flip_state);
    drop(dev);
    drop(seat);
    flips_result
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args[1] != "--show" {
        eprintln!("DRM-07: damage tracking por buffer + Pixman + page flips DRM/KMS + libseat");
        eprintln!("Sin --show no se modifica la pantalla.");
        eprintln!("Uso desde TTY Linux: {} --show /dev/dri/card1", args[0]);
        return Ok(());
    }
    let path = args.get(2).map(String::as_str).unwrap_or("/dev/dri/card1");
    io::stderr().flush()?;
    show(path)
}
