//! Render display math through the terminal's graphics protocol.
//!
//! The renderer shells out to a local LaTeX installation (`latex` + `dvipng`,
//! with an `xelatex` fallback for UTF-8/CJK formulas), then hands the PNG to
//! `ratatui-image`, which speaks Kitty, Sixel or iTerm2. When no protocol is
//! available, or the external tools are missing, callers fall back to the
//! character-grid renderer in [`super::math`].
//!
//! Images are cached by formula, available width and colour so scrolling and
//! redraws do not re-run LaTeX.
//!
//! Compilation is asynchronous: [`render`] only consumes PNGs that are already
//! in memory or on disk and returns `None` on a miss, so the first frame shows
//! the ASCII fallback. A small worker pool compiles visible formulas first,
//! then the rest of the session at low priority. [`displayable`] tells the UI
//! when a re-layout would replace a fallback with its image.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::SystemTime;

use eix_core::config::MathConfig;
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use ratatui::layout::Size;
use ratatui::style::Color;
#[cfg(unix)]
use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, FontSize, Resize};
use sha2::{Digest, Sha256};

/// An image encoded for the active terminal protocol. Despite the name it is
/// also used for user attachments and `view_image` thumbnails, not only math.
#[derive(Clone)]
pub struct MathImage {
    pub protocol: Arc<Protocol>,
    pub cols: u16,
    pub rows: u16,
}

/// One image pinned to a rendered transcript line. `line` is relative to the
/// block that produced it; `column` is the first cell of the image.
#[derive(Clone)]
pub struct Placement {
    pub line: usize,
    pub column: u16,
    pub image: MathImage,
}

/// A formula waiting for its PNG. The transcript keeps these so the viewport
/// can request renders and notice when an image becomes available.
#[derive(Clone, Debug)]
pub struct Pending {
    pub line: usize,
    pub source: String,
    pub width: usize,
    pub color: Option<Color>,
    /// Inline images are scaled to a single row; display math keeps its own
    /// vertical size.
    pub inline: bool,
}

/// Background work ordering. Formulas inside or near the viewport go first;
/// the rest is warmed after the first frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Visible,
    Prewarm,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    source: String,
    width: usize,
    color: Option<Color>,
    /// Inline math is scaled to one row, so it cannot share an encoded image
    /// with the same formula rendered as display math.
    inline: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FileKey {
    path: String,
    max_cols: u16,
    max_rows: u16,
    /// Modification time so an edited file is re-read instead of serving a
    /// stale thumbnail from the cache.
    modified: Option<u128>,
}

/// `(source, colour, inline)`. Inline math is compiled in text style with a
/// baseline strut, so it needs its own PNG.
type LatexKey = (String, Option<Color>, bool);
type LatexCache = HashMap<LatexKey, Result<Vec<u8>, String>>;

/// Bump when the generated PNG changes for the same source (preamble layout,
/// package list, engine flags, ...). The digest is part of every cache file
/// name, so old entries are simply ignored and pruned later.
const CACHE_VERSION: u32 = 3;

/// Upper bound for `~/.cache/eix/math`. A typical formula PNG is a few KiB,
/// so this holds thousands of renders.
const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

static CACHE_PRUNED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Settings {
    /// `[math].images`; general image thumbnails stay available regardless.
    math_images: bool,
    dpi: u32,
    max_source_bytes: usize,
    min_scale: f64,
    max_rows: u32,
    preamble: String,
    /// Persist rendered PNGs across runs. `EIX_MATH_CACHE=0` disables both
    /// reading and writing the on-disk cache.
    cache: bool,
}

impl Settings {
    fn from_config(config: &MathConfig) -> Self {
        // `EIX_MATH_DPI` predates the config file and stays as an override for
        // scripts and quick experiments.
        let dpi = std::env::var("EIX_MATH_DPI")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|dpi| (50..=1200).contains(dpi))
            .unwrap_or(config.dpi);
        let cache = !std::env::var("EIX_MATH_CACHE").is_ok_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        });
        Self {
            math_images: config.images,
            dpi,
            max_source_bytes: config.max_source_bytes,
            min_scale: f64::from(config.min_scale),
            max_rows: config.max_rows,
            preamble: config.preamble.clone(),
            cache,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_config(&MathConfig::default())
    }
}

/// Why image rendering is on or off. Exposed to the status bar and `/math`.
#[derive(Clone)]
pub struct Status {
    pub enabled: bool,
    /// `kitty`, `sixel`, `iterm2`, or `off`.
    pub protocol: String,
    /// Set when images are disabled or detection failed.
    pub reason: Option<String>,
    pub dpi: u32,
    pub last_error: Option<String>,
}

struct Job {
    source: String,
    color: Option<Color>,
    inline: bool,
}

/// Background compile queue. Visible formulas are always taken before warmed
/// ones, and `active` deduplicates requests across panes and redraws.
#[derive(Default)]
struct Queue {
    visible: VecDeque<Job>,
    prewarm: VecDeque<Job>,
    active: HashSet<LatexKey>,
    shutdown: bool,
}

/// State shared with the compiler threads: the PNG cache and the queue. The
/// terminal picker stays on the UI thread; workers only produce PNG bytes.
struct Shared {
    settings: Settings,
    latex: Mutex<LatexCache>,
    known: Mutex<HashSet<LatexKey>>,
    /// Formulas discovered by the renderer and not yet offered to the warm
    /// queue. Draining this per frame keeps `prewarm` proportional to new
    /// formulas instead of the whole session.
    discovered: Mutex<VecDeque<LatexKey>>,
    /// Formulas whose compile failed. The ASCII fallback stays and the next
    /// frame does not retry them.
    failed: Mutex<HashSet<LatexKey>>,
    queue: Mutex<Queue>,
    wake: Condvar,
}

impl Shared {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            latex: Mutex::new(HashMap::new()),
            known: Mutex::new(HashSet::new()),
            discovered: Mutex::new(VecDeque::new()),
            failed: Mutex::new(HashSet::new()),
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
        }
    }

    /// PNG bytes from memory or disk. Missing means "needs a compile".
    fn cached_png(
        &self,
        source: &str,
        color: Option<Color>,
        inline: bool,
    ) -> Option<Result<Vec<u8>, String>> {
        let key = (source.to_string(), color, inline);
        if let Ok(cache) = self.latex.lock() {
            if let Some(cached) = cache.get(&key) {
                return Some(cached.clone());
            }
        }
        let path = disk_path(&self.settings, source, color, inline)?;
        let png = read_cached_png(&path)?;
        let cached = Ok(png);
        if let Ok(mut cache) = self.latex.lock() {
            cache.insert(key, cached.clone());
        }
        Some(cached)
    }

    fn is_ready(&self, source: &str, color: Option<Color>, inline: bool) -> bool {
        self.latex
            .lock()
            .ok()
            .and_then(|cache| {
                cache
                    .get(&(source.to_string(), color, inline))
                    .map(Result::is_ok)
            })
            .unwrap_or(false)
    }

    /// Queue one formula. Returns whether new work was added.
    fn enqueue(
        &self,
        source: &str,
        color: Option<Color>,
        inline: bool,
        priority: Priority,
    ) -> bool {
        let key = (source.to_string(), color, inline);
        if !self.settings.math_images || self.is_ready(source, color, inline) {
            return false;
        }
        if self
            .failed
            .lock()
            .map(|failed| failed.contains(&key))
            .unwrap_or(false)
        {
            return false;
        }
        let Ok(mut queue) = self.queue.lock() else {
            return false;
        };
        if queue.shutdown {
            return false;
        }
        if queue.active.contains(&key) {
            // A visible request must not sit behind warmed formulas.
            if priority == Priority::Visible {
                if let Some(index) = queue.prewarm.iter().position(|job| {
                    job.source == source && job.color == color && job.inline == inline
                }) {
                    if let Some(job) = queue.prewarm.remove(index) {
                        queue.visible.push_back(job);
                    }
                }
            }
            return false;
        }
        queue.active.insert(key);
        let job = Job {
            source: source.to_string(),
            color,
            inline,
        };
        match priority {
            Priority::Visible => queue.visible.push_back(job),
            Priority::Prewarm => queue.prewarm.push_back(job),
        }
        drop(queue);
        self.wake.notify_one();
        true
    }
}

/// Owns the compiler threads. Dropping the service (for example `/math
/// reload`) asks them to stop after the current formula.
struct Workers {
    shared: Arc<Shared>,
}

impl Workers {
    fn spawn(shared: Arc<Shared>) -> Self {
        let count = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .clamp(1, 4);
        for index in 0..count {
            let worker = Arc::clone(&shared);
            let _ = std::thread::Builder::new()
                .name(format!("eix-math-{index}"))
                .spawn(move || compile_worker(worker));
        }
        Self { shared }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.shutdown = true;
            queue.visible.clear();
            queue.prewarm.clear();
        }
        self.shared.wake.notify_all();
    }
}

fn compile_worker(shared: Arc<Shared>) {
    loop {
        let job = {
            let Ok(mut queue) = shared.queue.lock() else {
                return;
            };
            loop {
                if queue.shutdown {
                    return;
                }
                if let Some(job) = queue
                    .visible
                    .pop_front()
                    .or_else(|| queue.prewarm.pop_front())
                {
                    break job;
                }
                queue = match shared.wake.wait(queue) {
                    Ok(queue) => queue,
                    Err(_) => return,
                };
            }
        };
        let key = (job.source.clone(), job.color, job.inline);
        if shared
            .cached_png(&job.source, job.color, job.inline)
            .is_none()
        {
            let rendered = latex_png(&job.source, job.color, &shared.settings, job.inline);
            match &rendered {
                Ok(png) => {
                    if let Some(path) =
                        disk_path(&shared.settings, &job.source, job.color, job.inline)
                    {
                        store_cached_png(&path, png);
                    }
                }
                Err(error) => {
                    record_error(&job.source, error);
                    if let Ok(mut failed) = shared.failed.lock() {
                        failed.insert(key.clone());
                    }
                }
            }
            if let Ok(mut cache) = shared.latex.lock() {
                cache.insert(key.clone(), rendered);
            }
        } else if !shared.is_ready(&job.source, job.color, job.inline) {
            // A cached failure (for example a corrupt on-disk entry) must not
            // be retried on every frame.
            if let Ok(mut failed) = shared.failed.lock() {
                failed.insert(key.clone());
            }
        }
        if let Ok(mut queue) = shared.queue.lock() {
            queue.active.remove(&key);
        }
    }
}

struct Service {
    picker: Picker,
    settings: Settings,
    cache: Mutex<HashMap<CacheKey, Option<MathImage>>>,
    files: Mutex<HashMap<FileKey, Option<MathImage>>>,
    shared: Arc<Shared>,
    _workers: Workers,
}

static SERVICE: OnceLock<Mutex<Option<Service>>> = OnceLock::new();
/// The user disabled math (`/math off` or `[math] images = false`) or the
/// `EIX_MATH_IMAGES=0` kill switch is set. Formulas keep their literal TeX
/// instead of using an image or the 2D ASCII grid.
static RAW_SOURCE: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<Status> = Mutex::new(Status {
    enabled: false,
    protocol: String::new(),
    reason: None,
    dpi: 200,
    last_error: None,
});
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Probe the terminal and apply `[math]` settings. Returns whether real images
/// are usable. Safe to call again to re-probe after switching terminal or
/// changing configuration.
pub fn init(config: &MathConfig) -> bool {
    let env_disabled = std::env::var("EIX_MATH_IMAGES").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    });
    RAW_SOURCE.store(env_disabled || !config.images, Ordering::Relaxed);
    let settings = Settings::from_config(config);
    let dpi = settings.dpi;

    let (service, protocol, reason) = if env_disabled {
        (
            None,
            "off".to_string(),
            Some("EIX_MATH_IMAGES=0".to_string()),
        )
    } else {
        match Service::new(settings) {
            Ok(service) => {
                let protocol = protocol_name(service.picker.protocol_type()).to_string();
                let reason = (!config.images).then(|| "math.images = false".to_string());
                (Some(service), protocol, reason)
            }
            Err(reason) => (None, "off".to_string(), Some(reason)),
        }
    };
    let enabled = service.is_some();
    if let Ok(mut status) = STATUS.lock() {
        *status = Status {
            enabled,
            protocol,
            reason,
            dpi,
            last_error: status.last_error.clone(),
        };
    }
    let slot = SERVICE.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = slot.lock() {
        *slot = service;
    }
    enabled
}

/// Re-run terminal detection and apply fresh settings.
pub fn reload(config: &MathConfig) -> bool {
    init(config)
}

pub fn enabled() -> bool {
    SERVICE
        .get()
        .and_then(|slot| slot.lock().ok().map(|slot| slot.is_some()))
        .unwrap_or(false)
}

/// True when the user turned math off, so formulas should stay as literal
/// TeX (`$...$`, `$$...$$`) instead of an image or the 2D ASCII grid.
pub fn raw_source() -> bool {
    RAW_SOURCE.load(Ordering::Relaxed)
}

pub fn status() -> Status {
    let mut status = STATUS
        .lock()
        .map(|status| status.clone())
        .unwrap_or_else(|_| Status {
            enabled: false,
            protocol: "off".into(),
            reason: Some("status lock poisoned".into()),
            dpi: 200,
            last_error: None,
        });
    status.last_error = LAST_ERROR
        .lock()
        .ok()
        .and_then(|error| error.clone())
        .or(status.last_error);
    status
}

static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn protocol_name(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::Kitty => "kitty",
        ProtocolType::Sixel => "sixel",
        ProtocolType::Iterm2 => "iterm2",
        ProtocolType::Halfblocks => "halfblocks",
    }
}

/// Probe the terminal for a graphics protocol and a usable cell size.
///
/// `ratatui-image` only keeps a detected protocol when the terminal also
/// answers the XTWINOPS cell-size query (`CSI 16 t`). Terminals such as `st`
/// answer the Kitty query but not `CSI 16 t`, so they are reported as
/// `Halfblocks` even though they can display images. Fall back to probing the
/// capability directly and taking the cell size from `TIOCGWINSZ`, which `st`
/// fills in correctly.
fn probe_picker() -> Result<Picker, String> {
    let picker = Picker::from_query_stdio().map_err(|error| format!("terminal probe: {error}"))?;
    if !matches!(picker.protocol_type(), ProtocolType::Halfblocks) {
        return Ok(picker);
    }
    #[cfg(unix)]
    {
        let Some(responses) = probe_graphics() else {
            return Ok(picker);
        };
        if detected_kitty(&responses) {
            let Some((width, height)) = cell_pixels() else {
                return Err(
                    "Kitty graphics detected, but the terminal reports no pixel cell size".into(),
                );
            };
            return Ok(kitty_picker(width, height));
        }
    }
    Ok(picker)
}

#[cfg(unix)]
fn detected_kitty(responses: &[Response]) -> bool {
    responses
        .iter()
        .any(|response| matches!(response, Response::Kitty))
}

/// Build a Kitty picker with an explicit cell size, bypassing the
/// `ratatui-image` detection path that requires `CSI 16 t`.
fn kitty_picker(width: u16, height: u16) -> Picker {
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(FontSize::new(width.max(1), height.max(1)));
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

/// Re-run the graphics part of the `ratatui-image` probe, ignoring the cell
/// size. The query is built by the same parser so tmux passthrough handling
/// stays consistent.
#[cfg(unix)]
fn probe_graphics() -> Option<Vec<Response>> {
    let is_tmux = std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux");
    let query = Parser::query(is_tmux, QueryStdioOptions::default());
    let _raw = RawMode::enable(libc::STDIN_FILENO)?;
    write_all(libc::STDOUT_FILENO, query.as_bytes())?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut parser = Parser::new();
    let mut responses = Vec::new();
    let mut buffer = [0u8; 128];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis() as i32) };
        if ready <= 0 {
            break;
        }
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count <= 0 {
            break;
        }
        for byte in &buffer[..count as usize] {
            responses.extend(parser.push(char::from(*byte)));
        }
        if responses
            .last()
            .is_some_and(|response| matches!(response, Response::Status))
        {
            break;
        }
    }
    (!responses.is_empty()).then_some(responses)
}

#[cfg(unix)]
fn write_all(fd: i32, mut bytes: &[u8]) -> Option<()> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if written == 0 {
            return None;
        }
        bytes = &bytes[written as usize..];
    }
    Some(())
}

/// Cell size in pixels from `TIOCGWINSZ`. Several terminals (notably `st`)
/// only report it here, not through the XTWINOPS query.
#[cfg(unix)]
fn cell_pixels() -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    if size.ws_xpixel == 0 || size.ws_ypixel == 0 || size.ws_col == 0 || size.ws_row == 0 {
        return None;
    }
    Some((
        (size.ws_xpixel / size.ws_col).max(1),
        (size.ws_ypixel / size.ws_row).max(1),
    ))
}

/// Temporarily switch stdin to non-canonical, non-echoing mode while waiting
/// for probe replies, restoring whatever mode was active before (the TUI runs
/// this with raw mode already enabled).
#[cfg(unix)]
struct RawMode {
    fd: i32,
    original: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    fn enable(fd: i32) -> Option<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

/// Lay out `source` as a display-math image that fits in `width` cells. Only
/// cached PNGs are used: a miss returns `None` immediately and queues
/// background work, so the caller keeps the ASCII fallback for this frame.
pub fn render(source: &str, width: usize, color: Option<Color>) -> Option<MathImage> {
    render_mode(source, width, color, false)
}

/// Lay out `source` as a single-row image embedded in a text line. Returns
/// `None` when the formula would be too tall or too wide to fit.
pub fn render_inline(source: &str, width: usize, color: Option<Color>) -> Option<MathImage> {
    render_mode(source, width, color, true)
}

fn render_mode(
    source: &str,
    width: usize,
    color: Option<Color>,
    inline: bool,
) -> Option<MathImage> {
    let slot = SERVICE.get()?.lock().ok()?;
    let service = slot.as_ref()?;
    if !service.settings.math_images {
        return None;
    }
    let key = CacheKey {
        source: source.to_string(),
        width,
        color,
        inline,
    };
    if let Some(cached) = service.cache.lock().ok()?.get(&key) {
        return cached.clone();
    }
    let Some(png) = service.shared.cached_png(source, color, inline) else {
        // Remember the formula so `prewarm` can fill it after the first
        // frame without blocking the layout that discovered it.
        let latex_key = (source.to_string(), color, inline);
        let fresh = service
            .shared
            .known
            .lock()
            .map(|mut known| known.insert(latex_key.clone()))
            .unwrap_or(false);
        if fresh {
            if let Ok(mut discovered) = service.shared.discovered.lock() {
                discovered.push_back(latex_key);
            }
        }
        return None;
    };
    let rendered = match png.and_then(|bytes| service.encode_png(&bytes, width, inline)) {
        Ok(image) => Some(image),
        Err(error) => {
            record_error(source, &error);
            None
        }
    };
    if let Ok(mut cache) = service.cache.lock() {
        cache.insert(key, rendered.clone());
    }
    rendered
}

/// True when a re-layout would now draw this formula: either its PNG is ready
/// and this width is not encoded yet, or another pane already encoded it while
/// this pane still holds the ASCII fallback.
pub fn displayable(source: &str, width: usize, color: Option<Color>, inline: bool) -> bool {
    let Some(slot) = SERVICE.get() else {
        return false;
    };
    let Ok(slot) = slot.lock() else {
        return false;
    };
    let Some(service) = slot.as_ref() else {
        return false;
    };
    let key = CacheKey {
        source: source.to_string(),
        width,
        color,
        inline,
    };
    let encoded = service
        .cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned());
    match encoded {
        Some(Some(_)) => true,
        // A cached failure would only fall back to ASCII again.
        Some(None) => false,
        None => service.shared.is_ready(source, color, inline),
    }
}

/// True when a miss for this exact layout is still waiting for a compile, so
/// the transcript should keep a pending entry for it.
pub fn pending(source: &str, width: usize, color: Option<Color>, inline: bool) -> bool {
    let Some(slot) = SERVICE.get() else {
        return false;
    };
    let Ok(slot) = slot.lock() else {
        return false;
    };
    let Some(service) = slot.as_ref() else {
        return false;
    };
    if !service.settings.math_images {
        return false;
    }
    let key = CacheKey {
        source: source.to_string(),
        width,
        color,
        inline,
    };
    service
        .cache
        .lock()
        .map(|cache| !cache.contains_key(&key))
        .unwrap_or(false)
}

/// Queue a formula for background compilation.
pub fn request(source: &str, color: Option<Color>, inline: bool, priority: Priority) {
    let Some(shared) = current_shared() else {
        return;
    };
    shared.enqueue(source, color, inline, priority);
}

/// Offer formulas discovered by the last render to the low-priority queue.
/// Called after every frame; only newly seen formulas cost anything, so the
/// first frame can paint immediately and the rest follow in the background.
pub fn prewarm() {
    let Some(shared) = current_shared() else {
        return;
    };
    if !shared.settings.math_images {
        return;
    }
    let discovered: Vec<LatexKey> = shared
        .discovered
        .lock()
        .map(|mut discovered| discovered.drain(..).collect())
        .unwrap_or_default();
    for (source, color, inline) in discovered {
        shared.enqueue(&source, color, inline, Priority::Prewarm);
    }
}

fn current_shared() -> Option<Arc<Shared>> {
    let slot = SERVICE.get()?.lock().ok()?;
    slot.as_ref().map(|service| Arc::clone(&service.shared))
}

const THUMBNAIL_COLS: u16 = 40;
const THUMBNAIL_ROWS: u16 = 12;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;

/// Render a local raster image as a thumbnail. Supports png/jpeg/gif/webp when
/// the corresponding `image` features are enabled. Returns `None` when images
/// are disabled, the file is unreadable, or the format is unsupported.
///
/// `base` is the directory a relative path is resolved against (the session's
/// cwd); absolute paths and `file://` URIs ignore it.
pub fn render_file(
    path: &Path,
    base: Option<&Path>,
    max_cols: Option<u16>,
    max_rows: Option<u16>,
) -> Option<MathImage> {
    let max_cols = max_cols.unwrap_or(THUMBNAIL_COLS).max(1);
    let max_rows = max_rows.unwrap_or(THUMBNAIL_ROWS).max(1);
    let resolved = resolve_image_path(path, base)?;
    let slot = SERVICE.get()?.lock().ok()?;
    let service = slot.as_ref()?;
    let metadata = fs::metadata(&resolved).ok()?;
    // Do not decode an enormous file into memory just to preview it.
    if metadata.len() > MAX_THUMBNAIL_BYTES {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    let key = FileKey {
        path: resolved.to_string_lossy().into_owned(),
        max_cols,
        max_rows,
        modified,
    };
    if let Some(cached) = service.files.lock().ok()?.get(&key) {
        return cached.clone();
    }
    let rendered = service.render_file_uncached(&resolved, max_cols, max_rows);
    if rendered.is_none() {
        record_error(&key.path, "could not decode image for the terminal");
    }
    if let Ok(mut cache) = service.files.lock() {
        cache.insert(key, rendered.clone());
    }
    rendered
}

/// Try the path as written, then relative to `base` (the session cwd) and
/// finally the process working directory. `file://` URIs from drag-and-drop
/// are decoded as well.
fn resolve_image_path(path: &Path, base: Option<&Path>) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let text = path.to_string_lossy();
    if let Some(uri) = text.strip_prefix("file://") {
        let uri = uri.strip_prefix("localhost").unwrap_or(uri);
        let decoded = percent_decode(uri);
        let path = PathBuf::from(decoded);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(base) = base {
        let relative = base.join(path);
        if relative.is_file() {
            return Some(relative);
        }
    }
    let relative = std::env::current_dir().ok()?.join(path);
    relative.is_file().then_some(relative)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn record_error(source: &str, error: &str) {
    let line = source.lines().next().unwrap_or_default();
    let message = format!("{error} (source: {line})");
    if let Ok(mut last) = LAST_ERROR.lock() {
        *last = Some(message.clone());
    }
    log_failure(source, error);
}

/// Append a compact diagnostic to `<data>/eix/math.log`. TeX failures are
/// cached, so this writes once per distinct formula.
fn log_failure(source: &str, error: &str) {
    let Some(dir) = dirs::data_dir().map(|dir| dir.join("eix")) else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("math.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > 1024 * 1024) {
        let _ = fs::rename(&path, dir.join("math.log.1"));
    }
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let first = source.lines().next().unwrap_or_default();
    let _ = writeln!(file, "[{seconds}] {error}\n  {first}");
}

/// Rendered PNGs live in the OS cache directory, so reopening a session can
/// skip both TeX and `dvipng`. The file name is a digest of everything that
/// affects the PNG, which makes the cache safe to share between panes,
/// processes and terminal sizes.
fn math_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join("eix").join("math"))
}

fn cache_digest(source: &str, color: Option<Color>, inline: bool, settings: &Settings) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_VERSION.to_le_bytes());
    hasher.update([u8::from(inline)]);
    hasher.update(settings.dpi.to_le_bytes());
    hasher.update((source.len() as u64).to_le_bytes());
    hasher.update(source.as_bytes());
    hasher.update((settings.preamble.len() as u64).to_le_bytes());
    hasher.update(settings.preamble.as_bytes());
    hasher.update(color_hex(color).as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn cache_path(
    source: &str,
    color: Option<Color>,
    inline: bool,
    settings: &Settings,
) -> Option<PathBuf> {
    let dir = math_cache_dir()?;
    Some(dir.join(format!(
        "{}.png",
        cache_digest(source, color, inline, settings)
    )))
}

/// Cache file for a formula, or `None` when persistence is disabled.
fn disk_path(
    settings: &Settings,
    source: &str,
    color: Option<Color>,
    inline: bool,
) -> Option<PathBuf> {
    settings
        .cache
        .then(|| cache_path(source, color, inline, settings))
        .flatten()
}

/// Read a cached PNG, discarding entries that are not complete PNG files
/// (for example left behind by an interrupted older version).
fn read_cached_png(path: &Path) -> Option<Vec<u8>> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let bytes = fs::read(path).ok()?;
    if !bytes.starts_with(PNG_SIGNATURE) {
        let _ = fs::remove_file(path);
        return None;
    }
    Some(bytes)
}

/// Write through a unique temporary file so a concurrent reader never sees a
/// partial PNG.
fn store_cached_png(path: &Path, png: &[u8]) {
    let Some(dir) = path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{}.{}.tmp", std::process::id(), seq));
    if fs::write(&temp, png).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    if fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    if !CACHE_PRUNED.swap(true, Ordering::Relaxed) {
        prune_cache(dir);
    }
}

/// Keep the cache under [`CACHE_MAX_BYTES`], dropping the oldest entries.
/// Runs once per process after the first new render.
fn prune_cache(dir: &Path) {
    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        total += meta.len();
        files.push((modified(&path), meta.len(), path));
    }
    if total <= CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(time, _, _)| *time);
    let target = CACHE_MAX_BYTES / 4 * 3;
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

impl Service {
    fn new(settings: Settings) -> Result<Self, String> {
        let picker = probe_picker()?;
        if matches!(picker.protocol_type(), ProtocolType::Halfblocks) {
            return Err("no Kitty/Sixel/iTerm2 graphics protocol detected".into());
        }
        let shared = Arc::new(Shared::new(settings.clone()));
        let workers = Workers::spawn(Arc::clone(&shared));
        Ok(Self {
            picker,
            settings,
            cache: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            shared,
            _workers: workers,
        })
    }

    /// Decode a compiled PNG and encode it for this terminal and width.
    fn encode_png(&self, image: &[u8], width: usize, inline: bool) -> Result<MathImage, String> {
        let width = width.clamp(1, u16::MAX as usize) as u16;
        let decoded = image::load_from_memory(image)
            .map_err(|error| format!("could not decode LaTeX PNG: {error}"))?;
        if inline {
            return self.encode_inline(decoded.to_rgba8(), width);
        }
        self.fit(
            decoded,
            width,
            self.settings.max_rows,
            self.settings.min_scale,
            self.settings.min_scale,
        )
    }

    /// Compose an inline formula onto a transparent canvas the size of one
    /// text row. The formula is scaled to the surrounding prose and placed so
    /// its baseline matches the text baseline; letting `ratatui-image` fit the
    /// bare PNG would top-align it and make short formulas float.
    fn encode_inline(&self, decoded: RgbaImage, max_cols: u16) -> Result<MathImage, String> {
        let font = self.picker.font_size();
        let cell_w = f64::from(font.width.max(1));
        let cell_h = f64::from(font.height.max(1));
        let font_width = u32::from(font.width.max(1));
        let font_height = u32::from(font.height.max(1));

        // The marker rule is ordinary ink, so the tight crop keeps it; its top
        // edge is the TeX baseline. Without it a descender-less formula would
        // be aligned by its ink bottom and sit too low.
        let (formula, baseline) = match inline_marker(&decoded, self.settings.dpi)
            .and_then(|marker| strip_inline_marker(&decoded, &marker))
        {
            Some((formula, baseline)) => (formula, f64::from(baseline)),
            None => {
                // A formula compiled through the display fallback has no
                // marker; bottom-align it so it still sits on the baseline.
                let formula = crop_ink(&decoded).unwrap_or(decoded);
                let baseline = f64::from(formula.height());
                (formula, baseline)
            }
        };

        // `standalone` uses a 10pt base font; one em in the PNG is this many
        // pixels. Terminals normally set the cell height to about 1.2 em, so
        // 0.84 of a cell makes the formula match the surrounding prose text.
        let em_px = f64::from(self.settings.dpi) * 10.0 / 72.27;
        let mut scale = (cell_h * 0.84 / em_px).min(1.0);
        scale = scale.min(f64::from(max_cols) * cell_w / f64::from(formula.width().max(1)));
        scale = scale.min(cell_h / f64::from(formula.height().max(1)));
        if scale <= 0.0 {
            return Err("formula does not fit on one text row".into());
        }
        let width = (f64::from(formula.width()) * scale).round().max(1.0) as u32;
        let height = (f64::from(formula.height()) * scale).round().max(1.0) as u32;
        let cols = width.div_ceil(font_width).max(1);
        let mut canvas = RgbaImage::from_pixel(cols * font_width, font_height, Rgba([0, 0, 0, 0]));
        let resized = imageops::resize(&formula, width, height, FilterType::Lanczos3);
        // Center the ink in its cell-aligned canvas. Reserving cells is
        // unavoidable, and splitting the leftover pixels keeps the formula
        // from looking detached from the text that follows it.
        let x = i64::from((cols * font_width).saturating_sub(width) / 2);
        let cell_baseline = (cell_h * 0.8).round();
        let top = (cell_baseline - baseline * scale).round();
        let top = top.clamp(0.0, (cell_h - f64::from(height)).max(0.0));
        imageops::overlay(&mut canvas, &resized, x, top as i64);
        let protocol = self
            .picker
            .new_protocol(
                DynamicImage::ImageRgba8(canvas),
                Size::new(cols as u16, 1),
                Resize::Fit(Some(FilterType::Lanczos3)),
            )
            .map_err(|error| format!("terminal protocol: {error}"))?;
        let size = protocol.size();
        Ok(MathImage {
            protocol: Arc::new(protocol),
            cols: size.width.max(1),
            rows: 1,
        })
    }

    fn render_file_uncached(&self, path: &Path, max_cols: u16, max_rows: u16) -> Option<MathImage> {
        let bytes = fs::read(path).ok()?;
        let decoded = image::load_from_memory(&bytes).ok()?;
        self.fit(decoded, max_cols, max_rows.into(), 0.0, 0.0).ok()
    }

    /// Scale `decoded` into the cell box. Formula images refuse to shrink below
    /// `min_width_scale` so a very wide expression falls back to the character
    /// renderer; `min_height_scale` guards the vertical shrink. Raster
    /// thumbnails pass `0.0` for both and fit freely.
    fn fit(
        &self,
        decoded: image::DynamicImage,
        max_cols: u16,
        max_rows: u32,
        min_width_scale: f64,
        min_height_scale: f64,
    ) -> Result<MathImage, String> {
        let font = self.picker.font_size();
        let font_width = u32::from(font.width.max(1));
        let font_height = u32::from(font.height.max(1));
        let natural_cols = decoded.width().div_ceil(font_width).max(1);
        let natural_rows = decoded.height().div_ceil(font_height).max(1);
        let (cols, rows) = layout_size(
            natural_cols,
            natural_rows,
            max_cols,
            max_rows,
            min_width_scale,
            min_height_scale,
        )?;
        let protocol = self
            .picker
            .new_protocol(
                decoded,
                Size::new(cols, rows),
                Resize::Fit(Some(FilterType::Lanczos3)),
            )
            .map_err(|error| format!("terminal protocol: {error}"))?;
        let size = protocol.size();
        Ok(MathImage {
            protocol: Arc::new(protocol),
            cols: size.width.max(1),
            rows: size.height.max(1),
        })
    }
}

/// Fit a natural cell size into `max_cols` x `max_rows`.
///
/// Raster thumbnails shrink freely. Formula images refuse to shrink below
/// `min_width_scale`/`min_height_scale` so an over-wide or over-tall
/// expression falls back to the character renderer instead of becoming
/// unreadable.
fn layout_size(
    natural_cols: u32,
    natural_rows: u32,
    max_cols: u16,
    max_rows: u32,
    min_width_scale: f64,
    min_height_scale: f64,
) -> Result<(u16, u16), String> {
    let natural_cols = f64::from(natural_cols.max(1));
    let natural_rows = f64::from(natural_rows.max(1));
    let mut scale = (f64::from(max_cols) / natural_cols).min(1.0);
    if min_width_scale > 0.0 && scale < min_width_scale && natural_cols > f64::from(max_cols) {
        return Err(format!(
            "formula is {natural_cols:.0} cells wide; would shrink below {:.0}%",
            min_width_scale * 100.0
        ));
    }
    // Very tall formulas are scaled to the pane height. If that would take
    // them below `min_height_scale`, keep the character renderer instead of
    // drawing an unreadable thumbnail.
    if max_rows > 0 && natural_rows * scale > f64::from(max_rows) {
        let height_scale = f64::from(max_rows) / natural_rows;
        if min_height_scale > 0.0 && height_scale < min_height_scale {
            return Err(format!(
                "formula is {natural_rows:.0} cells tall; would shrink below {:.0}%",
                min_height_scale * 100.0
            ));
        }
        scale = height_scale;
    }
    Ok((
        ((natural_cols * scale).ceil() as u16).max(1),
        ((natural_rows * scale).ceil() as u16).max(1),
    ))
}

/// Optional packages shared by both engines. Each one is only loaded when
/// the local TeX installation provides it, so a minimal TeX Live still works.
const OPTIONAL_PACKAGES: &[&str] = &[
    "mathtools",
    "bm",
    "cancel",
    "mathrsfs",
    "mhchem",
    "siunitx",
    "dsfont",
    "bbm",
    "braket",
    "upgreek",
];

/// Packages loaded only for sources that need them. pgfplots is slow to load,
/// so a normal fraction should not pay for it.
fn conditional_packages(source: &str) -> Vec<&'static str> {
    let lower = source.to_ascii_lowercase();
    let mut packages = Vec::new();
    if lower.contains("tikz") || lower.contains("\\begin{axis}") {
        packages.push("pgfplots");
    }
    packages
}

/// Compile one formula to a transparent PNG. The document uses `standalone`
/// so the output is tightly cropped around the formula.
///
/// `latex` + `dvipng` is the fast path. When it fails (CJK, literal Unicode,
/// TikZ, or a macro from the configured preamble) retry through `xelatex` +
/// `pdftocairo`, which understands UTF-8 and CJK fonts.
fn latex_png(
    source: &str,
    color: Option<Color>,
    settings: &Settings,
    inline: bool,
) -> Result<Vec<u8>, String> {
    if source.trim().is_empty() {
        return Err("empty formula".into());
    }
    if source.len() > settings.max_source_bytes {
        return Err(format!(
            "formula is {} bytes; math.max_source_bytes is {}",
            source.len(),
            settings.max_source_bytes
        ));
    }
    if unsafe_latex(source) {
        return Err("formula uses a command that is blocked for safety".into());
    }
    let dir = TempDir::create().ok_or_else(|| "could not create a temp directory".to_string())?;
    let dpi = settings.dpi.to_string();
    let raw = source.trim();
    let bodies = if inline {
        // Text style keeps fractions and operators at prose size. The short
        // rule before the formula is rendered as ordinary ink, so the tight
        // crop keeps it: its top edge marks the TeX baseline and lets the UI
        // align the PNG with the surrounding text. A zero-width strut would
        // be cropped away. The display form below is only a fallback for
        // constructs that reject inline math.
        let body = format!(
            "$\\rule[-0.22em]{{1pt}}{{0.22em}}{}$",
            inline_math_source(raw)
        );
        vec![body, format!("\\[ {} \\]", inline_math_source(raw))]
    } else if is_display_environment(raw) {
        // `\[...\]` is a display environment, so `\tag`, `\label` and most
        // constructs behave like they do in the model's own document. Raw
        // display environments (`align`, `gather`, TikZ, ...) cannot be
        // nested inside it, so try those verbatim first.
        vec![raw.to_string(), format!("\\[ {raw} \\]")]
    } else {
        vec![format!("\\[ {raw} \\]"), raw.to_string()]
    };
    let classic = compile_dvi(&dir, &bodies, color, &dpi, settings);
    match classic {
        Ok(png) => Ok(png),
        Err(classic_error) => match compile_unicode(&dir, &bodies, color, &dpi, settings) {
            Ok(png) => Ok(png),
            Err(unicode_error) => Err(format!("latex: {classic_error}; xelatex: {unicode_error}")),
        },
    }
}

/// The baseline marker prepended to inline formulas by [`latex_png`]. It is a
/// solid rule whose top edge is the TeX baseline; `dvipng -T tight` crops to
/// ink, so a zero-width strut would disappear while this rule survives.
struct InlineMarker {
    /// Horizontal position of the rule's left edge.
    left: u32,
    /// Rule width in pixels.
    width: u32,
    /// Baseline row in the full PNG.
    baseline: u32,
}

/// Find the inline baseline marker at the left edge of a compiled PNG.
///
/// Returns `None` when the image has no marker (for example a display-style
/// fallback), so the caller can fall back to bottom alignment.
fn inline_marker(image: &RgbaImage, dpi: u32) -> Option<InlineMarker> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let alpha = |x: u32, y: u32| image.get_pixel(x, y)[3];
    let left = (0..width).find(|&x| (0..height).any(|y| alpha(x, y) > 0))?;
    let baseline = (0..height).find(|&y| alpha(left, y) > 0)?;
    let bottom = (0..height).rev().find(|&y| alpha(left, y) > 0)?;
    // The rule is 0.22em tall. Rasterization rounds its edges, so allow a few
    // pixels of slack and reject anything that is clearly a glyph instead.
    let expected_height = (f64::from(dpi) * 0.22 * 10.0 / 72.27).round().max(1.0) as u32;
    let marker_height = bottom - baseline + 1;
    if marker_height + 1 < expected_height || marker_height > expected_height + 3 {
        return None;
    }
    let mut marker_width = 0;
    while left + marker_width < width
        && (baseline..=bottom).all(|y| alpha(left + marker_width, y) > 0)
    {
        marker_width += 1;
    }
    let expected_width = (f64::from(dpi) / 72.27).ceil().max(1.0) as u32;
    if marker_width == 0 || marker_width > expected_width + 3 {
        return None;
    }
    Some(InlineMarker {
        left,
        width: marker_width,
        baseline,
    })
}

/// Crop the marker off and trim to the formula's ink, keeping the row that
/// `baseline` refers to. Returns the formula image and its baseline row.
fn strip_inline_marker(image: &RgbaImage, marker: &InlineMarker) -> Option<(RgbaImage, u32)> {
    let (width, height) = image.dimensions();
    let start = marker.left + marker.width;
    if start >= width {
        return None;
    }
    let mut top = height;
    let mut bottom = 0;
    let mut right = 0;
    let mut found = false;
    for y in 0..height {
        for x in start..width {
            if image.get_pixel(x, y)[3] > 0 {
                found = true;
                top = top.min(y);
                bottom = bottom.max(y);
                right = right.max(x);
            }
        }
    }
    if !found || marker.baseline < top || marker.baseline > bottom + 1 {
        return None;
    }
    let cropped =
        imageops::crop_imm(image, start, top, right + 1 - start, bottom + 1 - top).to_image();
    Some((cropped, marker.baseline - top))
}

/// Trim transparent edges from an image that has no baseline marker.
fn crop_ink(image: &RgbaImage) -> Option<RgbaImage> {
    let (width, height) = image.dimensions();
    let mut left = width;
    let mut top = height;
    let mut right = 0;
    let mut bottom = 0;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if image.get_pixel(x, y)[3] > 0 {
                found = true;
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
            }
        }
    }
    if !found {
        return None;
    }
    Some(imageops::crop_imm(image, left, top, right + 1 - left, bottom + 1 - top).to_image())
}

/// Rewrite display-only TeX so an inline formula keeps text metrics: display
/// fractions and binomials become their `t...` variants and forced limits are
/// dropped.
fn inline_math_source(source: &str) -> String {
    source
        .replace("\\displaystyle", "")
        .replace("\\dfrac", "\\tfrac")
        .replace("\\dbinom", "\\tbinom")
        .replace("\\limits", "\\nolimits")
}

/// LaTeX document preamble. `unicode` selects the `xelatex` variant, which
/// swaps `amssymb` for `unicode-math` and adds CJK font support.
fn preamble(color: Option<Color>, unicode: bool, settings: &Settings, source: &str) -> String {
    let mut preamble = String::from("\\documentclass[border=1pt,varwidth]{standalone}\n");
    if unicode {
        preamble.push_str("\\usepackage{amsmath}\n\\usepackage{unicode-math}\n");
        // `bm` is incompatible with `unicode-math`, so provide the command
        // through its bold-symbol equivalent. The same applies to the
        // `doublestroke`/`bbm` blackboard-bold spellings.
        preamble.push_str("\\providecommand{\\bm}[1]{\\symbf{#1}}\n");
        preamble.push_str("\\providecommand{\\mathds}[1]{\\symbb{#1}}\n");
        preamble.push_str("\\providecommand{\\mathbbm}[1]{\\symbb{#1}}\n");
    } else {
        preamble.push_str("\\usepackage{amsmath,amssymb}\n");
    }
    preamble.push_str("\\usepackage{xcolor}\n");
    for package in OPTIONAL_PACKAGES {
        // `bm`, `dsfont` and `bbm` clash with `unicode-math`; the preamble
        // above already supplies compatible aliases for their commands.
        if unicode && matches!(*package, "bm" | "dsfont" | "bbm") {
            continue;
        }
        preamble.push_str(&format!(
            "\\IfFileExists{{{package}.sty}}{{\\usepackage{{{package}}}}}{{}}\n"
        ));
    }
    for package in conditional_packages(source) {
        preamble.push_str(&format!(
            "\\IfFileExists{{{package}.sty}}{{\\usepackage{{{package}}}}}{{}}\n"
        ));
    }
    if unicode {
        // CJK text in `\text{...}` needs an explicit font. Pick the first
        // common Noto/Source Han family available on the system.
        preamble.push_str(
            "\\IfFileExists{xeCJK.sty}{\\usepackage{xeCJK}\
             \\IfFontExistsTF{Noto Sans CJK SC}{\\setCJKmainfont{Noto Sans CJK SC}}{\
             \\IfFontExistsTF{Noto Sans CJK TC}{\\setCJKmainfont{Noto Sans CJK TC}}{\
             \\IfFontExistsTF{Source Han Sans CN}{\\setCJKmainfont{Source Han Sans CN}}{\
             \\IfFontExistsTF{Source Han Sans SC}{\\setCJKmainfont{Source Han Sans SC}}{}}}}}{}\n",
        );
        // `amssymb` aliases that `unicode-math` spells differently.
        preamble.push_str(
            "\\providecommand{\\square}{\\mdlgwhtsquare}\n\
             \\providecommand{\\blacksquare}{\\smblksquare}\n\
             \\providecommand{\\lozenge}{\\mdlgwhtlozenge}\n\
             \\providecommand{\\blacklozenge}{\\mdlgblklozenge}\n\
             \\providecommand{\\centerdot}{\\mathbin{\\cdot}}\n\
             \\providecommand{\\smallsetminus}{\\setminus}\n\
             \\providecommand{\\nleqslant}{\\nleq}\n\
             \\providecommand{\\ngeqslant}{\\ngeq}\n\
             \\providecommand{\\ntriangleleft}{\\not\\triangleleft}\n\
             \\providecommand{\\shortmid}{\\mid}\n\
             \\providecommand{\\shortparallel}{\\parallel}\n\
             \\providecommand{\\circledR}{\\textregistered}\n\
             \\providecommand{\\yen}{\\mathyen}\n",
        );
    }
    // `preamble` is trusted config (project layers cannot set it). It may
    // define project macros or load additional packages.
    if !settings.preamble.trim().is_empty() {
        preamble.push_str(&settings.preamble);
        preamble.push('\n');
    }
    preamble.push_str(&format!(
        "\\begin{{document}}\n\\color[HTML]{{{}}}\n",
        color_hex(color)
    ));
    preamble
}

/// Fast path: `latex` + `dvipng`.
fn compile_dvi(
    dir: &TempDir,
    bodies: &[String],
    color: Option<Color>,
    dpi: &str,
    settings: &Settings,
) -> Result<Vec<u8>, String> {
    let preamble = preamble(color, false, settings, &bodies.join("\n"));
    let mut last_error = String::from("latex produced no output");
    for body in bodies {
        let document = format!("{preamble}{body}\n\\end{{document}}\n");
        if fs::write(dir.path.join("formula.tex"), document).is_err() {
            last_error = "could not write formula.tex".into();
            continue;
        }
        let _ = fs::remove_file(dir.path.join("formula.png"));
        let latex = Command::new("latex")
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-no-shell-escape",
                "formula.tex",
            ])
            .current_dir(&dir.path)
            .output()
            .map_err(|error| format!("could not run latex: {error}"))?;
        if !latex.status.success() {
            last_error = tex_error(&dir.path, "formula.log");
            continue;
        }
        let dvipng = Command::new("dvipng")
            .args([
                "-T",
                "tight",
                "-D",
                dpi,
                "-bg",
                "Transparent",
                "-o",
                "formula.png",
                "formula.dvi",
            ])
            .current_dir(&dir.path)
            .output()
            .map_err(|error| format!("could not run dvipng: {error}"))?;
        if !dvipng.status.success() {
            last_error = "dvipng failed".into();
            continue;
        }
        if let Ok(png) = fs::read(dir.path.join("formula.png")) {
            return Ok(png);
        }
        last_error = "dvipng produced no PNG".into();
    }
    Err(last_error)
}

/// UTF-8 path: `xelatex` + PDF-to-PNG conversion.
fn compile_unicode(
    dir: &TempDir,
    bodies: &[String],
    color: Option<Color>,
    dpi: &str,
    settings: &Settings,
) -> Result<Vec<u8>, String> {
    let preamble = preamble(color, true, settings, &bodies.join("\n"));
    let mut last_error = String::from("xelatex produced no output");
    for body in bodies {
        let document = format!("{preamble}{body}\n\\end{{document}}\n");
        if fs::write(dir.path.join("formula.tex"), document).is_err() {
            last_error = "could not write formula.tex".into();
            continue;
        }
        let _ = fs::remove_file(dir.path.join("formula.png"));
        let xelatex = Command::new("xelatex")
            .args([
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-no-shell-escape",
                "formula.tex",
            ])
            .current_dir(&dir.path)
            .output()
            .map_err(|error| format!("could not run xelatex: {error}"))?;
        if !xelatex.status.success() {
            last_error = tex_error(&dir.path, "formula.log");
            continue;
        }
        // poppler is the usual provider; prefer `pdftocairo` (transparent
        // background) and fall back to `pdftoppm` for lean installations.
        let converters: [(&str, &[&str]); 2] = [
            (
                "pdftocairo",
                &[
                    "-png",
                    "-transp",
                    "-singlefile",
                    "-r",
                    dpi,
                    "formula.pdf",
                    "formula",
                ],
            ),
            (
                "pdftoppm",
                &["-png", "-singlefile", "-r", dpi, "formula.pdf", "formula"],
            ),
        ];
        let converted = converters.iter().any(|(tool, args)| {
            Command::new(tool)
                .args(*args)
                .current_dir(&dir.path)
                .output()
                .is_ok_and(|output| output.status.success())
        });
        if !converted {
            last_error = "pdftocairo/pdftoppm failed".into();
            continue;
        }
        if let Ok(png) = fs::read(dir.path.join("formula.png")) {
            return Ok(png);
        }
        last_error = "PDF conversion produced no PNG".into();
    }
    Err(last_error)
}

/// Extract the first useful TeX error from a log file.
fn tex_error(dir: &Path, name: &str) -> String {
    let Ok(log) = fs::read_to_string(dir.join(name)) else {
        return format!("{name} missing");
    };
    for line in log.lines() {
        if let Some(error) = line.strip_prefix("! ") {
            return error.trim().to_string();
        }
    }
    // dvipng/pdflatex sometimes only leave a generic "Emergency stop".
    log.lines()
        .rev()
        .find(|line| line.starts_with('!'))
        .map(|line| line.trim_start_matches('!').trim().to_string())
        .unwrap_or_else(|| "TeX failed without an error line".into())
}

/// Display environments that must be emitted directly instead of inside
/// `\[...\]`. `aligned`, `cases` and matrix environments still need the math
/// wrapper and deliberately are not listed here.
fn is_display_environment(source: &str) -> bool {
    const ENVIRONMENTS: &[&str] = &[
        "align",
        "align*",
        "equation",
        "equation*",
        "gather",
        "gather*",
        "multline",
        "multline*",
        "eqnarray",
        "eqnarray*",
        "displaymath",
        "math",
    ];
    let source = source.trim_start();
    ENVIRONMENTS
        .iter()
        .any(|environment| source.starts_with(&format!("\\begin{{{environment}}}")))
}

/// Formula text is model output. Keep it from escaping the math expression or
/// reading local files even though shell escape is disabled.
fn unsafe_latex(source: &str) -> bool {
    const DENY: &[&str] = &[
        "\\input",
        "\\include",
        "\\openin",
        "\\openout",
        "\\write",
        "\\read",
        "\\catcode",
        "\\special",
        "\\end{document}",
        "\\begin{document}",
    ];
    let lower = source.to_ascii_lowercase();
    DENY.iter().any(|needle| lower.contains(needle))
}

fn color_hex(color: Option<Color>) -> String {
    match color {
        Some(Color::Rgb(red, green, blue)) => format!("{red:02X}{green:02X}{blue:02X}"),
        // Transparent themes use the terminal foreground; a light gray stays
        // readable on the dark backgrounds those themes usually run on.
        _ => "D8DEE9".to_string(),
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create() -> Option<Self> {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("eix-math-{}-{seq}", std::process::id()));
        fs::create_dir_all(&path).ok()?;
        Some(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when any of `tools` answers a version probe. In CI the math job
    /// sets `EIX_REQUIRE_TEX`, which turns a missing toolchain into a failure
    /// instead of a silent skip.
    fn require_tool(tools: &[&str]) -> bool {
        let found = tools.iter().any(|tool| {
            ["--version", "-v"].iter().any(|flag| {
                Command::new(tool)
                    .arg(flag)
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
        });
        if !found && std::env::var_os("EIX_REQUIRE_TEX").is_some() {
            panic!("EIX_REQUIRE_TEX is set but none of {tools:?} is installed");
        }
        found
    }

    #[test]
    fn rejects_latex_file_access() {
        assert!(unsafe_latex("\\input{/etc/passwd}"));
        assert!(unsafe_latex("x \\end{document} y"));
        assert!(unsafe_latex("\\write18{rm -rf /}"));
        assert!(!unsafe_latex("\\frac{a}{b}"));
    }

    /// Terminals that answer the Kitty query without `CSI 16 t` must still
    /// come out as a Kitty picker, sized from the kernel's cell pixels.
    #[cfg(unix)]
    #[test]
    fn kitty_probe_keeps_protocol_without_cell_size() {
        assert!(detected_kitty(&[Response::Kitty]));
        assert!(!detected_kitty(&[Response::Sixel]));
        assert!(!detected_kitty(&[]));

        let picker = kitty_picker(17, 33);
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty);
        assert_eq!(picker.font_size().width, 17);
        assert_eq!(picker.font_size().height, 33);
    }

    #[test]
    fn layout_refuses_unreadable_formulas() {
        // A 200-cell-wide formula in a 40-cell pane would be 20% scale.
        let error = layout_size(200, 5, 40, 60, 0.5, 0.5).unwrap_err();
        assert!(error.contains("wide"), "{error}");
        // Tall formulas shrink to the height limit instead of being clipped.
        assert_eq!(layout_size(10, 100, 40, 60, 0.5, 0.5).unwrap(), (6, 60));
        // But not below the minimum readable scale.
        let error = layout_size(10, 100, 40, 10, 0.5, 0.5).unwrap_err();
        assert!(error.contains("tall"), "{error}");
        // Thumbnails have no minimum scale and fit both axes.
        assert_eq!(layout_size(100, 100, 40, 10, 0.0, 0.0).unwrap(), (10, 10));
        // Inline math has no height floor: a two-row fraction is compressed
        // into the single text row instead of falling back to raw `$...$`.
        assert_eq!(layout_size(10, 2, 40, 1, 0.5, 0.0).unwrap(), (5, 1));
        // A too-wide inline formula still keeps the width floor.
        let error = layout_size(200, 2, 40, 1, 0.5, 0.0).unwrap_err();
        assert!(error.contains("wide"), "{error}");
    }

    #[test]
    fn resolves_relative_images_against_the_session_cwd() {
        let dir = TempDir::create().expect("temp dir");
        fs::write(dir.path.join("plot.png"), b"not a real image").expect("write");
        assert_eq!(
            resolve_image_path(Path::new("plot.png"), Some(&dir.path)),
            Some(dir.path.join("plot.png"))
        );
        assert!(resolve_image_path(Path::new("missing.png"), Some(&dir.path)).is_none());
    }

    #[test]
    fn formats_rgb_colors_for_xcolor() {
        assert_eq!(color_hex(Some(Color::Rgb(1, 2, 3))), "010203");
        assert_eq!(color_hex(None), "D8DEE9");
    }

    #[test]
    fn cache_key_tracks_render_inputs() {
        let base = Settings::default();
        let source = r"\frac{a}{b}";
        let color = Some(Color::Rgb(1, 2, 3));
        let digest = cache_digest(source, color, false, &base);
        assert_eq!(digest, cache_digest(source, color, false, &base));
        assert_ne!(digest, cache_digest(r"\frac{a}{c}", color, false, &base));
        assert_ne!(
            digest,
            cache_digest(source, Some(Color::Rgb(1, 2, 4)), false, &base)
        );
        // Inline math is compiled differently, so it must not share a file.
        assert_ne!(digest, cache_digest(source, color, true, &base));

        let mut changed = base.clone();
        changed.dpi += 1;
        assert_ne!(digest, cache_digest(source, color, false, &changed));

        let mut changed = base.clone();
        changed.preamble.push_str("\\newcommand{\\R}{\\mathbb{R}}");
        assert_ne!(digest, cache_digest(source, color, false, &changed));
    }

    #[test]
    fn disk_cache_round_trips_and_rejects_junk() {
        let dir = TempDir::create().expect("temp dir");
        let path = dir.path.join("formula.png");
        let png = b"\x89PNG\r\n\x1a\nrendered";
        store_cached_png(&path, png);
        assert_eq!(read_cached_png(&path).as_deref(), Some(png.as_slice()));

        fs::write(&path, b"junk").expect("write");
        assert!(read_cached_png(&path).is_none());
        assert!(!path.exists(), "corrupt entries must be removed");
    }

    #[test]
    fn latex_pipeline_handles_common_math_constructs() {
        if !require_tool(&["latex"]) {
            return;
        }
        let samples = [
            r"\frac{a}{b}",
            r"\begin{align} a &= b \\ c &= d \end{align}",
            r"\begin{aligned} a &= b \\ c &= d \end{aligned}",
            r"\bm{x} + \operatorname{rank}(A)",
            r"\cancel{x}",
            r"\ce{H2O}",
            r"\SI{5}{\meter}",
            r"\tag{1} x = 1",
            r"\mathds{1} + \square + \blacksquare",
            r"\braket{a|b}",
        ];
        for sample in samples {
            assert!(
                latex_png(
                    sample,
                    Some(Color::Rgb(220, 223, 228)),
                    &Settings::default(),
                    false,
                )
                .is_ok(),
                "failed to render {sample}"
            );
        }
    }

    #[test]
    fn latex_pipeline_handles_unicode_and_cjk_text() {
        let has_xelatex = require_tool(&["xelatex"]);
        let has_convert = require_tool(&["pdftocairo", "pdftoppm"]);
        if !has_xelatex || !has_convert {
            return;
        }
        let mut samples = vec![
            r"x \le y \text{ and } z \ge 0".to_string(),
            r"\begin{align} \alpha &= \beta \\ \gamma &= \delta \end{align}".to_string(),
            r"\bm{x} + \mathds{1}".to_string(),
        ];
        let has_cjk = Command::new("fc-list")
            .args([":lang=zh", "family"])
            .output()
            .is_ok_and(|output| output.status.success() && !output.stdout.is_empty());
        if has_cjk {
            samples.push(r"\zeta(s)=\prod_{p\ \text{为素数}}\frac{1}{1-p^{-s}}".to_string());
            samples.push(r"f(x) \le 1 \text{ 当 } x \to 0".to_string());
        }
        for sample in samples {
            assert!(
                latex_png(
                    &sample,
                    Some(Color::Rgb(220, 223, 228)),
                    &Settings::default(),
                    false,
                )
                .is_ok(),
                "failed to render {sample}"
            );
        }
    }

    #[test]
    fn inline_math_source_uses_text_metrics() {
        let source = inline_math_source(
            r"\displaystyle \dfrac{a}{b} + \dbinom{n}{k} + \sum_{i=1}^n x_i \limits",
        );
        assert!(!source.contains("\\displaystyle"), "{source}");
        assert!(!source.contains("\\dfrac"), "{source}");
        assert!(!source.contains("\\dbinom"), "{source}");
        assert!(!source.contains("\\limits"), "{source}");
        assert!(source.contains("\\tfrac"), "{source}");
        assert!(source.contains("\\tbinom"), "{source}");
    }

    #[test]
    fn inline_marker_locates_the_baseline_and_is_stripped() {
        let mut image = RgbaImage::from_pixel(40, 30, Rgba([0, 0, 0, 0]));
        // Baseline marker: two pixels wide, rows 20..=26 below the baseline.
        for y in 20..=26 {
            for x in 0..2 {
                image.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        // Formula ink: a glyph above the baseline and a descender below it.
        for y in 8..=22 {
            image.put_pixel(5, y, Rgba([255, 255, 255, 255]));
        }
        let marker = inline_marker(&image, 200).expect("marker");
        assert_eq!(marker.left, 0);
        assert_eq!(marker.width, 2);
        assert_eq!(marker.baseline, 20);

        let (formula, baseline) = strip_inline_marker(&image, &marker).expect("strip");
        assert_eq!(formula.dimensions(), (4, 15));
        assert_eq!(baseline, 12);
    }

    #[test]
    fn inline_png_carries_a_baseline_marker() {
        if !require_tool(&["latex", "dvipng"]) {
            return;
        }
        let png = latex_png(
            r"\frac{a}{b}+x^2",
            Some(Color::Rgb(220, 223, 228)),
            &Settings::default(),
            true,
        )
        .expect("inline png");
        let image = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert!(inline_marker(&image, Settings::default().dpi).is_some());
    }

    #[test]
    fn inline_math_is_shorter_than_display_math() {
        if !require_tool(&["latex"]) {
            return;
        }
        let settings = Settings::default();
        let source = r"\frac{a}{b} + \sum_{i=1}^n x_i";
        let display = latex_png(source, None, &settings, false).expect("display png");
        let inline = latex_png(source, None, &settings, true).expect("inline png");
        let display = image::load_from_memory(&display).expect("decode display");
        let inline = image::load_from_memory(&inline).expect("decode inline");
        assert!(
            inline.height() < display.height(),
            "inline {}px should be shorter than display {}px",
            inline.height(),
            display.height()
        );
    }

    #[test]
    fn latex_pipeline_produces_a_png_when_tex_is_installed() {
        if !require_tool(&["latex"]) {
            return;
        }
        let png = latex_png(
            "\\frac{a}{b}",
            Some(Color::Rgb(220, 223, 228)),
            &Settings::default(),
            false,
        );
        assert!(
            png.is_ok_and(|bytes| bytes.starts_with(b"\x89PNG")),
            "expected a PNG from latex + dvipng"
        );
    }
}
