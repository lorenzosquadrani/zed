use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use gpui::RenderImage;
use hayro::{RenderCache, RenderSettings, hayro_interpret::InterpreterSettings, hayro_syntax::Pdf};
use image::Frame;
use smallvec::smallvec;

// Keep each output bitmap within the texture atlas limits and 16 MiB.
pub const MAX_DIMENSION: u16 = 2048;
pub const MAX_FILE_SIZE: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

fn open(bytes: Arc<Vec<u8>>) -> Result<Pdf> {
    Pdf::new(bytes)
        .map_err(|error| anyhow!("Cannot open PDF (it may require a password): {error:?}"))
}

pub fn metadata(bytes: Arc<Vec<u8>>) -> Result<Vec<PageSize>> {
    let document = open(bytes)?;
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

fn bounded_scale(size: PageSize, scale: f32, max_dimension: u16) -> f32 {
    scale
        .min(f32::from(max_dimension) / size.width)
        .min(f32::from(max_dimension) / size.height)
}

pub fn render_page(
    bytes: Arc<Vec<u8>>,
    index: usize,
    scale: f32,
    max_dimension: u16,
) -> Result<Arc<RenderImage>> {
    let document = open(bytes)?;
    let page = document
        .pages()
        .get(index)
        .ok_or_else(|| anyhow!("PDF page does not exist"))?;
    let (width, height) = page.render_dimensions();
    ensure!(
        width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
        "Invalid PDF page dimensions"
    );
    ensure!(scale.is_finite() && scale > 0.0, "Invalid PDF zoom");
    let max_dimension = max_dimension.clamp(1, MAX_DIMENSION);
    let scale = bounded_scale(PageSize { width, height }, scale, max_dimension);
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some((width * scale).ceil().clamp(1.0, f32::from(max_dimension)) as u16),
            height: Some((height * scale).ceil().clamp(1.0, f32::from(max_dimension)) as u16),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    // GPUI uploads premultiplied BGRA; Hayro produces premultiplied RGBA.
    let pixels = pixmap
        .data()
        .iter()
        .flat_map(|pixel| [pixel.b, pixel.g, pixel.r, pixel.a])
        .collect();
    let buffer = image::RgbaImage::from_raw(pixmap.width().into(), pixmap.height().into(), pixels)
        .ok_or_else(|| anyhow!("Invalid rendered PDF bitmap"))?;
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn sample_pdf() -> Arc<Vec<u8>> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 5 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] /Contents 5 0 R >>".to_string(),
            {
                let stream = "1 0 0 rg 10 10 100 100 re f\n";
                format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len())
            },
        ];
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = bytes.len();
        bytes.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
        for offset in offsets {
            bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        Arc::new(bytes)
    }

    #[test]
    fn reads_mixed_page_sizes_and_renders_content() {
        let bytes = sample_pdf();
        let pages = metadata(bytes.clone()).expect("valid PDF");
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[0].height), (200.0, 300.0));
        assert_eq!((pages[1].width, pages[1].height), (400.0, 200.0));
        let rendered = render_page(bytes.clone(), 0, 1.0, MAX_DIMENSION).expect("rendered page");
        assert_eq!(
            rendered.size(0),
            gpui::size(gpui::DevicePixels(200), gpui::DevicePixels(300))
        );
        let pixels = rendered.as_bytes(0).expect("pixel buffer");
        assert!(
            pixels
                .chunks_exact(4)
                .any(|pixel| pixel == [0, 0, 255, 255]),
            "red PDF rectangle must render as BGRA"
        );
        assert!(
            pixels
                .chunks_exact(4)
                .any(|pixel| pixel == [255, 255, 255, 255]),
            "page background must be opaque white"
        );
        assert!(render_page(bytes, 2, 1.0, MAX_DIMENSION).is_err());
    }

    #[test]
    fn rejects_invalid_documents() {
        assert!(metadata(Arc::new(b"not a PDF".to_vec())).is_err());
    }

    #[test]
    fn limits_raster_size_without_changing_aspect_ratio() {
        let page = PageSize {
            width: 100_000.0,
            height: 1000.0,
        };
        let scale = bounded_scale(page, 4.0, MAX_DIMENSION);
        assert_eq!(page.width * scale, f32::from(MAX_DIMENSION));
        assert!((page.height * scale - 20.48).abs() < 0.001);
    }
}
