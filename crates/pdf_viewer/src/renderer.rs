use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{BackgroundExecutor, RenderImage, Task};
use hayro::RenderCache;
use image::Frame;
use smallvec::smallvec;

use crate::raster;
pub use raster::{MAX_DIMENSION, MAX_FILE_SIZE, PageSize, RasterSize};

struct Request {
    index: usize,
    size: RasterSize,
    response: oneshot::Sender<Result<Arc<RenderImage>>>,
}

pub struct Renderer {
    requests: async_channel::Sender<Request>,
    _worker: Task<()>,
}

impl Renderer {
    pub async fn new(
        bytes: Arc<Vec<u8>>,
        executor: &BackgroundExecutor,
    ) -> Result<(Arc<Self>, Vec<PageSize>)> {
        let (requests, receiver) = async_channel::bounded::<Request>(1);
        let (initialized, ready) = oneshot::channel();
        // Hayro's cache borrows the document and contains Rc/RefCell values.
        // Keep both on the same dedicated executor, including across awaits;
        // neither is sent between threads or protected by unsafe Send impls.
        let worker = executor.spawn_dedicated(move |_| async move {
            let document = match raster::open(bytes) {
                Ok(document) => document,
                Err(error) => {
                    if initialized.send(Err(error)).is_err() {
                        return;
                    }
                    return;
                }
            };
            let pages = raster::metadata(&document);
            let valid = pages.is_ok();
            if initialized.send(pages).is_err() || !valid {
                return;
            }
            let cache = RenderCache::new();
            while let Ok(request) = receiver.recv().await {
                if request.response.is_canceled() {
                    continue;
                }
                let result = raster::render_page(&document, &cache, request.index, request.size)
                    .and_then(render_image);
                // A view may close during synchronous interpretation. Its
                // canceled response is expected, not a worker failure.
                if request.response.send(result).is_err() {
                    continue;
                }
            }
        });
        let renderer = Arc::new(Self {
            requests,
            _worker: worker,
        });
        let pages = ready
            .await
            .context("PDF renderer stopped while opening document")??;
        Ok((renderer, pages))
    }

    pub async fn render(&self, index: usize, size: RasterSize) -> Result<Arc<RenderImage>> {
        let (response, result) = oneshot::channel();
        self.requests
            .send(Request {
                index,
                size,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("PDF renderer stopped"))?;
        result
            .await
            .context("PDF renderer stopped while rendering page")?
    }
}

fn render_image(pixmap: hayro::vello_cpu::Pixmap) -> Result<Arc<RenderImage>> {
    // GPUI uploads premultiplied BGRA; Hayro produces premultiplied RGBA.
    let pixels = pixmap
        .data()
        .iter()
        .flat_map(|pixel| [pixel.b, pixel.g, pixel.r, pixel.a])
        .collect();
    let buffer = image::RgbaImage::from_raw(pixmap.width().into(), pixmap.height().into(), pixels)
        .context("Invalid rendered PDF bitmap")?;
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use raster::{bounded_scale, open};

    #[test]
    fn capped_zoom_uses_identical_raster_dimensions() {
        let page = PageSize {
            width: 640.0,
            height: 800.0,
        };
        let capped = RasterSize::for_page(page, 3.0, MAX_DIMENSION);
        assert_eq!(capped, RasterSize::for_page(page, 4.0, MAX_DIMENSION));
        assert!(capped.covers(RasterSize::for_page(page, 1.0, MAX_DIMENSION)));
        assert!(capped.pixels() <= usize::from(MAX_DIMENSION).pow(2));
    }

    #[gpui::test]
    async fn worker_reuses_document_and_shuts_down(cx: &mut gpui::TestAppContext) {
        let (renderer, pages) = Renderer::new(sample_pdf(), &cx.executor())
            .await
            .expect("worker opens PDF");
        let size = RasterSize::for_page(pages[0], 1.0, MAX_DIMENSION);
        let first = renderer.render(0, size).await.expect("first render");
        let second = renderer
            .render(0, size)
            .await
            .expect("cached interpreter render");
        assert_eq!(first.as_bytes(0), second.as_bytes(0));
        assert!(renderer.render(100, size).await.is_err());
        let weak = Arc::downgrade(&renderer);
        drop(renderer);
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "worker must not retain its owner");
    }

    fn metadata(bytes: Arc<Vec<u8>>) -> Result<Vec<PageSize>> {
        raster::metadata(&open(bytes)?)
    }

    fn render_page(
        bytes: Arc<Vec<u8>>,
        index: usize,
        scale: f32,
        max_dimension: u16,
    ) -> Result<Arc<RenderImage>> {
        let document = open(bytes)?;
        let pages = raster::metadata(&document)?;
        let page = pages.get(index).context("page does not exist")?;
        render_image(raster::render_page(
            &document,
            &RenderCache::new(),
            index,
            RasterSize::for_page(*page, scale, max_dimension),
        )?)
    }

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
