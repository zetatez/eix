//! Render display math through the terminal's graphics protocol.
//!
//! The renderer shells out to a local LaTeX installation (`latex` + `dvipng`),
//! then hands the PNG to `ratatui-image`, which speaks Kitty, Sixel or iTerm2.
//! When no protocol is available, or the external tools are missing, callers
//! fall back to the character-grid renderer in [`super::math`].
//!
//! Images are cached by formula, available width and colour so scrolling and
//! redraws do not re-run LaTeX.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

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

type LatexKey = (String, Option<Color>);
type LatexCache = HashMap<LatexKey, Option<Vec<u8>>>;

struct Service {
    picker: Picker,
    cache: Mutex<HashMap<CacheKey, Option<MathImage>>>,
    /// PNG bytes keyed by formula and colour. Width only affects how the image
    /// is scaled, so resizing the terminal must not re-run LaTeX.
    latex: Mutex<LatexCache>,
}

static SERVICE: OnceLock<Option<Service>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Probe the terminal once at startup. Returns whether real images are usable.
///
/// Set `EIX_MATH_IMAGES=0` to force the ASCII renderer even on a terminal that
/// supports graphics.
pub fn init() -> bool {
    let disabled = std::env::var("EIX_MATH_IMAGES").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    });
    let service = if disabled { None } else { Service::new() };
    let enabled = service.is_some();
    let _ = SERVICE.set(service);
    enabled
}

pub fn enabled() -> bool {
    SERVICE.get().and_then(Option::as_ref).is_some()
}

/// Lay out `source` as an image that fits in `width` cells. Returns `None`
/// when images are disabled or rendering failed, so the caller can use ASCII.
pub fn render(source: &str, width: usize, color: Option<Color>) -> Option<MathImage> {
    let service = SERVICE.get()?.as_ref()?;
    let key = CacheKey {
        source: source.to_string(),
        width,
        color,
    };
    if let Some(cached) = service.cache.lock().ok()?.get(&key) {
        return cached.clone();
    }
    let rendered = service.render_uncached(source, width, color);
    if let Ok(mut cache) = service.cache.lock() {
        cache.insert(key, rendered.clone());
    }
    rendered
}

impl Service {
    fn new() -> Option<Self> {
        let picker = Picker::from_query_stdio().ok()?;
        if matches!(picker.protocol_type(), ProtocolType::Halfblocks) {
            return None;
        }
        Some(Self {
            picker,
            cache: Mutex::new(HashMap::new()),
            latex: Mutex::new(HashMap::new()),
        })
    }

    /// Compile a formula once per colour, independently of the pane width.
    fn latex_cached(&self, source: &str, color: Option<Color>) -> Option<Vec<u8>> {
        let key = (source.to_string(), color);
        if let Some(cached) = self.latex.lock().ok()?.get(&key) {
            return cached.clone();
        }
        let rendered = latex_png(source, color);
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
    ) -> Option<MathImage> {
        let width = width.clamp(1, u16::MAX as usize) as u16;
        let image = self.latex_cached(source, color)?;
        let decoded = image::load_from_memory(&image).ok()?;
        let font = self.picker.font_size();
        let font_width = u32::from(font.width.max(1));
        let font_height = u32::from(font.height.max(1));
        let natural_cols = decoded.width().div_ceil(font_width).max(1) as f64;
        let natural_rows = decoded.height().div_ceil(font_height).max(1) as f64;
        let scale = (f64::from(width) / natural_cols).min(1.0);
        let cols = ((natural_cols * scale).ceil() as u16).max(1);
        let rows = ((natural_rows * scale).ceil() as u16).max(1);
        let protocol = self
            .picker
            .new_protocol(decoded, Size::new(cols, rows), Resize::Fit(None))
            .ok()?;
        let size = protocol.size();
        Some(MathImage {
            protocol: Arc::new(protocol),
            cols: size.width.max(1),
            rows: size.height.max(1),
        })
    }
}

/// Compile one formula to a transparent PNG. The document uses `standalone`
/// so the output is tightly cropped around the formula.
fn latex_png(source: &str, color: Option<Color>) -> Option<Vec<u8>> {
    if source.trim().is_empty() || source.len() > 8_000 || unsafe_latex(source) {
        return None;
    }
    let dir = TempDir::create()?;
    let document = format!(
        "\\documentclass[border=1pt]{{standalone}}\n\
         \\usepackage{{amsmath,amssymb}}\n\
         \\usepackage{{xcolor}}\n\
         \\begin{{document}}\n\
         \\color[HTML]{{{color}}}$\\displaystyle {source}$\n\
         \\end{{document}}\n",
        color = color_hex(color),
    );
    fs::write(dir.path.join("formula.tex"), document).ok()?;
    let latex = Command::new("latex")
        .args([
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-no-shell-escape",
            "formula.tex",
        ])
        .current_dir(&dir.path)
        .output()
        .ok()?;
    if !latex.status.success() {
        return None;
    }
    let dpi = std::env::var("EIX_MATH_DPI").unwrap_or_else(|_| "200".into());
    let dvipng = Command::new("dvipng")
        .args([
            "-T",
            "tight",
            "-D",
            &dpi,
            "-bg",
            "Transparent",
            "-o",
            "formula.png",
            "formula.dvi",
        ])
        .current_dir(&dir.path)
        .output()
        .ok()?;
    if !dvipng.status.success() {
        return None;
    }
    fs::read(dir.path.join("formula.png")).ok()
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

    #[test]
    fn rejects_latex_file_access() {
        assert!(unsafe_latex("\\input{/etc/passwd}"));
        assert!(unsafe_latex("x \\end{document} y"));
        assert!(unsafe_latex("\\write18{rm -rf /}"));
        assert!(!unsafe_latex("\\frac{a}{b}"));
    }

    #[test]
    fn formats_rgb_colors_for_xcolor() {
        assert_eq!(color_hex(Some(Color::Rgb(1, 2, 3))), "010203");
        assert_eq!(color_hex(None), "D8DEE9");
    }

    #[test]
    fn latex_pipeline_produces_a_png_when_tex_is_installed() {
        let ok = Command::new("latex")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !ok {
            return;
        }
        let png = latex_png("\\frac{a}{b}", Some(Color::Rgb(220, 223, 228)));
        assert!(
            png.is_some_and(|bytes| bytes.starts_with(b"\x89PNG")),
            "expected a PNG from latex + dvipng"
        );
    }
}
