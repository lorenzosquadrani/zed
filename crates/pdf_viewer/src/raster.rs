use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use hayro::{RenderCache, RenderSettings, hayro_interpret::InterpreterSettings, hayro_syntax::Pdf};

// Keep each output bitmap within the texture atlas limits and 16 MiB.
pub const MAX_DIMENSION: u16 = 2048;
pub const MAX_FILE_SIZE: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

pub fn open(bytes: Arc<Vec<u8>>) -> Result<Pdf> {
    Pdf::new(bytes)
        .map_err(|error| anyhow!("Cannot open PDF (it may require a password): {error:?}"))
}

pub fn metadata(document: &Pdf) -> Result<Vec<PageSize>> {
    ensure!(!document.pages().is_empty(), "This PDF has no pages");
    document
        .pages()
        .iter()
        .map(|page| {
            let (width, height) = page.render_dimensions();
            ensure!(
                width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
                "This PDF contains invalid page dimensions"
            );
            Ok(PageSize { width, height })
        })
        .collect()
}

pub fn bounded_scale(size: PageSize, scale: f32, max_dimension: u16) -> f32 {
    scale
        .min(f32::from(max_dimension) / size.width)
        .min(f32::from(max_dimension) / size.height)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterSize {
    pub width: u16,
    pub height: u16,
}

impl RasterSize {
    pub fn for_page(page: PageSize, scale: f32, max_dimension: u16) -> Self {
        let max_dimension = max_dimension.clamp(1, MAX_DIMENSION);
        let scale = bounded_scale(page, scale, max_dimension);
        Self {
            width: (page.width * scale)
                .ceil()
                .clamp(1.0, f32::from(max_dimension)) as u16,
            height: (page.height * scale)
                .ceil()
                .clamp(1.0, f32::from(max_dimension)) as u16,
        }
    }

    pub fn pixels(self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }

    pub fn covers(self, other: Self) -> bool {
        self.width >= other.width && self.height >= other.height
    }
}

pub fn render_page<'a>(
    document: &'a Pdf,
    cache: &RenderCache<'a>,
    index: usize,
    size: RasterSize,
) -> Result<hayro::vello_cpu::Pixmap> {
    let page = document
        .pages()
        .get(index)
        .ok_or_else(|| anyhow!("PDF page does not exist"))?;
    let (width, height) = page.render_dimensions();
    ensure!(
        width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
        "Invalid PDF page dimensions"
    );
    ensure!(
        size.width > 0
            && size.height > 0
            && size.width <= MAX_DIMENSION
            && size.height <= MAX_DIMENSION,
        "Invalid PDF raster dimensions"
    );
    // Key the render by its actual pixel dimensions, including at the cap.
    // This also makes nearby zoom levels with identical dimensions reusable.
    let scale = (f32::from(size.width) / width).min(f32::from(size.height) / height);
    Ok(hayro::render(
        page,
        cache,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(size.width),
            height: Some(size.height),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    ))
}
