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

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use eix_core::config::MathConfig;
use ratatui::layout::Size;
use ratatui::style::Color;
#[cfg(unix)]
use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions, Response};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Resize};

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

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    source: String,
    width: usize,
    color: Option<Color>,
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

type LatexKey = (String, Option<Color>);
type LatexCache = HashMap<LatexKey, Result<Vec<u8>, String>>;

#[derive(Clone)]
struct Settings {
    /// `[math].images`; general image thumbnails stay available regardless.
    math_images: bool,
    dpi: u32,
    max_source_bytes: usize,
    min_scale: f64,
    max_rows: u32,
    preamble: String,
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
        Self {
            math_images: config.images,
            dpi,
            max_source_bytes: config.max_source_bytes,
            min_scale: f64::from(config.min_scale),
            max_rows: config.max_rows,
            preamble: config.preamble.clone(),
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

struct Service {
    picker: Picker,
    settings: Settings,
    cache: Mutex<HashMap<CacheKey, Option<MathImage>>>,
    files: Mutex<HashMap<FileKey, Option<MathImage>>>,
    /// PNG bytes keyed by formula and colour. Width only affects how the image
    /// is scaled, so resizing the terminal must not re-run LaTeX.
    latex: Mutex<LatexCache>,
}

static SERVICE: OnceLock<Mutex<Option<Service>>> = OnceLock::new();
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

/// Lay out `source` as an image that fits in `width` cells. Returns `None`
/// when images are disabled or rendering failed, so the caller can use ASCII.
pub fn render(source: &str, width: usize, color: Option<Color>) -> Option<MathImage> {
    let slot = SERVICE.get()?.lock().ok()?;
    let service = slot.as_ref()?;
    if !service.settings.math_images {
        return None;
    }
    let key = CacheKey {
        source: source.to_string(),
        width,
        color,
    };
    if let Some(cached) = service.cache.lock().ok()?.get(&key) {
        return cached.clone();
    }
    let rendered = match service.render_uncached(source, width, color) {
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

impl Service {
    fn new(settings: Settings) -> Result<Self, String> {
        let picker = probe_picker()?;
        if matches!(picker.protocol_type(), ProtocolType::Halfblocks) {
            return Err("no Kitty/Sixel/iTerm2 graphics protocol detected".into());
        }
        Ok(Self {
            picker,
            settings,
            cache: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            latex: Mutex::new(HashMap::new()),
        })
    }

    /// Compile a formula once per colour, independently of the pane width.
    fn latex_cached(&self, source: &str, color: Option<Color>) -> Result<Vec<u8>, String> {
        let key = (source.to_string(), color);
        if let Ok(cache) = self.latex.lock() {
            if let Some(cached) = cache.get(&key) {
                return cached.clone();
            }
        }
        let rendered = latex_png(source, color, &self.settings);
        if let Ok(mut cache) = self.latex.lock() {
            cache.insert(key, rendered.clone());
        }
        rendered
    }

    fn render_uncached(
        &self,
        source: &str,
        width: usize,
        color: Option<Color>,
    ) -> Result<MathImage, String> {
        let width = width.clamp(1, u16::MAX as usize) as u16;
        let image = self.latex_cached(source, color)?;
        let decoded = image::load_from_memory(&image)
            .map_err(|error| format!("could not decode LaTeX PNG: {error}"))?;
        self.fit(decoded, width, self.settings.max_rows, true)
    }

    fn render_file_uncached(&self, path: &Path, max_cols: u16, max_rows: u16) -> Option<MathImage> {
        let bytes = fs::read(path).ok()?;
        let decoded = image::load_from_memory(&bytes).ok()?;
        self.fit(decoded, max_cols, max_rows.into(), false).ok()
    }

    /// Scale `decoded` into the cell box. Formula images are width-limited and
    /// refuse to shrink below `min_scale`; raster thumbnails fit both axes.
    fn fit(
        &self,
        decoded: image::DynamicImage,
        max_cols: u16,
        max_rows: u32,
        math: bool,
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
            if math { self.settings.min_scale } else { 0.0 },
        )?;
        let protocol = self
            .picker
            .new_protocol(decoded, Size::new(cols, rows), Resize::Fit(None))
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
/// Raster thumbnails shrink freely. Formula images additionally refuse to
/// shrink below `min_scale` (physical width) so a very wide expression falls
/// back to the character renderer instead of becoming unreadable, and bail out
/// when even the scaled image is taller than `max_rows`.
fn layout_size(
    natural_cols: u32,
    natural_rows: u32,
    max_cols: u16,
    max_rows: u32,
    min_scale: f64,
) -> Result<(u16, u16), String> {
    let natural_cols = f64::from(natural_cols.max(1));
    let natural_rows = f64::from(natural_rows.max(1));
    let mut scale = (f64::from(max_cols) / natural_cols).min(1.0);
    if min_scale > 0.0 && scale < min_scale && natural_cols > f64::from(max_cols) {
        return Err(format!(
            "formula is {natural_cols:.0} cells wide; would shrink below {:.0}%",
            min_scale * 100.0
        ));
    }
    // Very tall formulas are scaled to the pane height. If that would take
    // them below `min_scale`, keep the character renderer instead of drawing
    // an unreadable thumbnail.
    if max_rows > 0 && natural_rows * scale > f64::from(max_rows) {
        let height_scale = f64::from(max_rows) / natural_rows;
        if min_scale > 0.0 && height_scale < min_scale {
            return Err(format!(
                "formula is {natural_rows:.0} cells tall; would shrink below {:.0}%",
                min_scale * 100.0
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
fn latex_png(source: &str, color: Option<Color>, settings: &Settings) -> Result<Vec<u8>, String> {
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
    // `\[...\]` is a display environment, so `\tag`, `\label` and most
    // constructs behave like they do in the model's own document. Raw display
    // environments (`align`, `gather`, TikZ, ...) cannot be nested inside it,
    // so try those verbatim first.
    let raw = source.trim();
    let wrapped = format!("\\[ {raw} \\]");
    let bodies = if is_display_environment(raw) {
        [raw.to_string(), wrapped]
    } else {
        [wrapped, raw.to_string()]
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
        let error = layout_size(200, 5, 40, 60, 0.5).unwrap_err();
        assert!(error.contains("wide"), "{error}");
        // Tall formulas shrink to the height limit instead of being clipped.
        assert_eq!(layout_size(10, 100, 40, 60, 0.5).unwrap(), (6, 60));
        // But not below the minimum readable scale.
        let error = layout_size(10, 100, 40, 10, 0.5).unwrap_err();
        assert!(error.contains("tall"), "{error}");
        // Thumbnails have no minimum scale and fit both axes.
        assert_eq!(layout_size(100, 100, 40, 10, 0.0).unwrap(), (10, 10));
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
                    &Settings::default()
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
                    &Settings::default()
                )
                .is_ok(),
                "failed to render {sample}"
            );
        }
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
        );
        assert!(
            png.is_ok_and(|bytes| bytes.starts_with(b"\x89PNG")),
            "expected a PNG from latex + dvipng"
        );
    }
}
