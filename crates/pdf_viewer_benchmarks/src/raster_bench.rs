#[allow(dead_code)]
#[path = "../../pdf_viewer/src/raster.rs"]
mod raster;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hayro::RenderCache;
use raster::{PageSize, RasterSize};
use std::{hint::black_box, sync::Arc, time::Duration};

fn fixture(page_count: usize) -> Arc<Vec<u8>> {
    let children = (0..page_count)
        .map(|index| format!("{} 0 R", index + 5))
        .collect::<Vec<_>>()
        .join(" ");
    let mut stream = String::from("BT /F1 12 Tf 32 760 Td 18 TL\n");
    for _ in 0..35 {
        stream.push_str(
            "(PDF preview: reusable fonts, text, plots and document structures.) Tj T*\n",
        );
    }
    stream.push_str("ET 0.1 0.4 0.8 RG 1 w\n");
    for index in 0..200 {
        stream.push_str(&format!(
            "{} 30 m {} {} l S\n",
            index * 3,
            index * 3 + 1,
            40 + index % 90
        ));
    }
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{children}] /Count {page_count} >>"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len()),
    ];
    objects.extend((0..page_count).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 640 800] /Resources << /Font << /F1 3 0 R >> >> /Contents 4 0 R >>".to_string()));
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    Arc::new(bytes)
}

fn benchmark(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("pdf_raster");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    for page_count in [1, 100, 1000] {
        let bytes = fixture(page_count);
        let document = raster::open(bytes.clone()).expect("fixture opens");
        assert_eq!(
            raster::metadata(&document).expect("metadata").len(),
            page_count
        );
        let cache = RenderCache::new();
        let sizes = [1.0, 1.25].map(|scale| {
            RasterSize::for_page(
                PageSize {
                    width: 640.0,
                    height: 800.0,
                },
                scale,
                raster::MAX_DIMENSION,
            )
        });
        for size in sizes {
            let cold = raster::open(bytes.clone()).expect("fixture opens");
            let cold = raster::render_page(&cold, &RenderCache::new(), page_count - 1, size)
                .expect("cold raster");
            let warm =
                raster::render_page(&document, &cache, page_count - 1, size).expect("warm raster");
            assert_eq!(cold.data(), warm.data(), "cache reuse must preserve pixels");
            assert_eq!(
                usize::from(warm.width()) * usize::from(warm.height()),
                size.pixels()
            );
            assert!(warm.data().iter().any(|pixel| pixel.r < 200));
        }
        // These are deliberately distinct cache states: the checkpoint recreated
        // both structures on every zoom; the worker retains them across zooms.
        group.bench_with_input(
            BenchmarkId::new("cold_document_and_cache", page_count),
            &page_count,
            |bench, _| {
                let mut iteration = 0;
                bench.iter(|| {
                    let document = raster::open(bytes.clone()).expect("fixture opens");
                    let image = raster::render_page(
                        &document,
                        &RenderCache::new(),
                        page_count - 1,
                        sizes[iteration % 2],
                    )
                    .expect("raster");
                    iteration += 1;
                    black_box(image);
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("retained_document_and_cache", page_count),
            &page_count,
            |bench, _| {
                let mut iteration = 0;
                bench.iter(|| {
                    let image = raster::render_page(
                        &document,
                        &cache,
                        page_count - 1,
                        sizes[iteration % 2],
                    )
                    .expect("raster");
                    iteration += 1;
                    black_box(image);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
