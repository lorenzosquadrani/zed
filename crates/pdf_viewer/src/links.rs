use std::collections::{HashMap, HashSet};

use hayro::hayro_syntax::{
    Pdf,
    object::{Array, Dict, Name, ObjRef, Object, Rect},
    page::Page,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Destination {
    pub page: usize,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct PageLink {
    pub bounds: Rect,
    pub destination: Destination,
}

pub fn read(document: &Pdf) -> Vec<Vec<PageLink>> {
    let destinations = NamedDestinations::new(document);
    let page_indices = document
        .pages()
        .iter()
        .enumerate()
        .filter_map(|(index, page)| Some((ObjRef::from(page.raw().obj_id()?), index)))
        .collect::<HashMap<_, _>>();
    document
        .pages()
        .iter()
        .map(|page| {
            page.raw()
                .get::<Array<'_>>(b"Annots")
                .into_iter()
                .flat_map(|annotations| annotations.iter::<Dict<'_>>())
                .filter_map(|annotation| {
                    if annotation.get::<Name<'_>>(b"Subtype")?.as_ref() != b"Link"
                        || annotation.get::<i32>(b"F").unwrap_or(0) & (2 | 32) != 0
                    {
                        return None;
                    }
                    let target = annotation.get::<Object<'_>>(b"Dest").or_else(|| {
                        let action = annotation.get::<Dict<'_>>(b"A")?;
                        if action.get::<Name<'_>>(b"S")?.as_ref() != b"GoTo" {
                            return None;
                        }
                        action.get::<Object<'_>>(b"D")
                    })?;
                    let destination = destinations.resolve(target)?;
                    let destination = destination_position(document, &page_indices, destination)?;
                    let bounds = link_bounds(page, annotation.get::<Rect>(b"Rect")?)?;
                    Some(PageLink {
                        bounds,
                        destination,
                    })
                })
                .collect()
        })
        .collect()
}

#[derive(Default)]
struct NamedDestinations<'a> {
    names: HashMap<Vec<u8>, Object<'a>>,
    strings: HashMap<Vec<u8>, Object<'a>>,
}

impl<'a> NamedDestinations<'a> {
    fn new(document: &'a Pdf) -> Self {
        let mut destinations = Self::default();
        let Some(catalog) = document.xref().get::<Dict<'_>>(document.xref().root_id()) else {
            return destinations;
        };
        if let Some(names) = catalog.get::<Dict<'_>>(b"Dests") {
            for name in names.keys() {
                if let Some(destination) = names.get::<Object<'_>>(name.as_ref()) {
                    destinations
                        .names
                        .insert(name.as_ref().to_vec(), destination);
                }
            }
        }
        let root = catalog
            .get::<Dict<'_>>(b"Names")
            .and_then(|names| names.get::<Dict<'_>>(b"Dests"));
        let mut pending = root.into_iter().collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some(node) = pending.pop() {
            if let Some(identifier) = node.obj_id()
                && !visited.insert(ObjRef::from(identifier))
            {
                continue;
            }
            if let Some(names) = node.get::<Array<'_>>(b"Names") {
                let mut entries = names.iter::<Object<'_>>();
                while let (Some(name), Some(destination)) = (entries.next(), entries.next()) {
                    if let Some(name) = name.into_string() {
                        destinations
                            .strings
                            .insert(name.as_bytes().to_vec(), destination);
                    }
                }
            }
            if let Some(children) = node.get::<Array<'_>>(b"Kids") {
                pending.extend(children.iter::<Dict<'_>>());
            }
        }
        destinations
    }

    fn resolve(&self, mut destination: Object<'a>) -> Option<Array<'a>> {
        // Malformed PDFs can contain cycles through names or /D dictionaries.
        for _ in 0..32 {
            destination = match destination {
                Object::Array(array) => return Some(array),
                Object::Dict(dictionary) => dictionary.get::<Object<'_>>(b"D")?,
                Object::Name(name) => self.names.get(name.as_ref())?.clone(),
                Object::String(name) => self.strings.get(name.as_bytes())?.clone(),
                _ => return None,
            };
        }
        None
    }
}

fn destination_position(
    document: &Pdf,
    page_indices: &HashMap<ObjRef, usize>,
    destination: Array<'_>,
) -> Option<Destination> {
    let page_index = *page_indices.get(&destination.raw_iter().next()?.as_obj_ref()?)?;
    let page = document.pages().get(page_index)?;
    let crop = page.intersected_crop_box();
    let mut values = destination.iter::<Object<'_>>().skip(1);
    let mode = values.next()?.into_name()?;
    let (x, y) = match mode.as_ref() {
        b"Fit" | b"FitB" => {
            return Some(Destination {
                page: page_index,
                x: 0.0,
                y: 0.0,
            });
        }
        b"XYZ" => (
            coordinate(values.next(), crop.x0 as f32)?,
            coordinate(values.next(), crop.y1 as f32)?,
        ),
        b"FitH" | b"FitBH" => (crop.x0 as f32, coordinate(values.next(), crop.y1 as f32)?),
        b"FitV" | b"FitBV" => (coordinate(values.next(), crop.x0 as f32)?, crop.y1 as f32),
        b"FitR" => {
            let left = values.next()?.into_f32()?;
            let bottom = values.next()?.into_f32()?;
            let right = values.next()?.into_f32()?;
            let top = values.next()?.into_f32()?;
            let bounds = link_bounds(
                page,
                Rect::new(left as f64, bottom as f64, right as f64, top as f64),
            )?;
            return Some(Destination {
                page: page_index,
                x: bounds.x0 as f32,
                y: bounds.y0 as f32,
            });
        }
        _ => return None,
    };
    let (x, y) = transform_point(page, x as f64, y as f64)?;
    let (width, height) = page.render_dimensions();
    Some(Destination {
        page: page_index,
        x: x.clamp(0.0, width),
        y: y.clamp(0.0, height),
    })
}

fn coordinate(value: Option<Object<'_>>, default: f32) -> Option<f32> {
    let coordinate = match value {
        None | Some(Object::Null(_)) => default,
        Some(value) => value.into_f32()?,
    };
    coordinate.is_finite().then_some(coordinate)
}

fn transform_point(page: &Page<'_>, x: f64, y: f64) -> Option<(f32, f32)> {
    // Use the rasterizer's transform so links match cropped and rotated images.
    let [scale_x, skew_y, skew_x, scale_y, offset_x, offset_y] =
        page.initial_transform(true).as_coeffs();
    let x_position = (scale_x * x + skew_x * y + offset_x) as f32;
    let y_position = (skew_y * x + scale_y * y + offset_y) as f32;
    (x_position.is_finite() && y_position.is_finite()).then_some((x_position, y_position))
}

fn link_bounds(page: &Page<'_>, bounds: Rect) -> Option<Rect> {
    let (first_x, first_y) = transform_point(page, bounds.x0, bounds.y0)?;
    let (second_x, second_y) = transform_point(page, bounds.x1, bounds.y1)?;
    let (width, height) = page.render_dimensions();
    let bounds = Rect::new(
        first_x.min(second_x).clamp(0.0, width) as f64,
        first_y.min(second_y).clamp(0.0, height) as f64,
        first_x.max(second_x).clamp(0.0, width) as f64,
        first_y.max(second_y).clamp(0.0, height) as f64,
    );
    (bounds.width() > 0.0 && bounds.height() > 0.0).then_some(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::tests::{linked_pdf, pdf_with_objects};

    #[test]
    fn resolves_contents_link_to_named_section() {
        let document = Pdf::new(linked_pdf()).expect("PDF fixture");
        let links = read(&document);
        let link = links[0].first().expect("contents link");
        assert_eq!(
            (
                link.bounds.x0,
                link.bounds.y0,
                link.bounds.x1,
                link.bounds.y1
            ),
            (10.0, 30.0, 110.0, 60.0)
        );
        assert_eq!(
            link.destination,
            Destination {
                page: 1,
                x: 0.0,
                y: 100.0
            }
        );
    }

    #[test]
    fn resolves_direct_legacy_and_name_tree_destinations() {
        let bytes = pdf_with_objects(&[
            "<< /Type /Catalog /Pages 2 0 R /Dests << /legacy << /D [4 0 R /XYZ 20 450 null] >> >> /Names << /Dests << /Kids [6 0 R] >> >> >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Annots [5 0 R << /Subtype /Link /Rect [10 20 30 40] /A << /S /GoTo /D /legacy >> >> << /Subtype /Link /Rect [10 20 30 40] /Dest (chapter) >>] >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] >>".into(),
            "<< /Subtype /Link /Rect [10 20 30 40] /Dest [4 0 R /FitH 500] >>".into(),
            "<< /Names [(chapter) << /D [4 0 R /Fit] >>] >>".into(),
        ]);
        let document = Pdf::new(bytes).expect("PDF fixture");
        let links = read(&document);
        let destinations = links[0]
            .iter()
            .map(|link| link.destination)
            .collect::<Vec<_>>();
        assert_eq!(
            destinations,
            [
                Destination {
                    page: 1,
                    x: 0.0,
                    y: 100.0
                },
                Destination {
                    page: 1,
                    x: 20.0,
                    y: 150.0
                },
                Destination {
                    page: 1,
                    x: 0.0,
                    y: 0.0
                },
            ]
        );
    }

    #[test]
    fn transforms_links_and_destinations_with_page_crop_and_rotation() {
        let bytes = pdf_with_objects(&[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /CropBox [10 20 110 220] /Rotate 90 /Annots [5 0 R] >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /CropBox [20 30 220 330] /Rotate 270 >>".into(),
            "<< /Subtype /Link /Rect [20 160 40 180] /Dest [4 0 R /XYZ 40 300 null] >>".into(),
        ]);
        let document = Pdf::new(bytes).expect("PDF fixture");
        let links = read(&document);
        let link = links[0].first().expect("rotated link");
        assert_eq!(
            (
                link.bounds.x0,
                link.bounds.y0,
                link.bounds.x1,
                link.bounds.y1
            ),
            (140.0, 10.0, 160.0, 30.0)
        );
        assert_eq!(
            link.destination,
            Destination {
                page: 1,
                x: 30.0,
                y: 180.0
            }
        );
    }

    #[test]
    fn rectangle_destinations_land_at_the_rendered_upper_left() {
        for (rotation, x, y) in [
            (0, 20.0, 30.0),
            (90, 170.0, 20.0),
            (180, 120.0, 170.0),
            (270, 30.0, 120.0),
        ] {
            let bytes = pdf_with_objects(&[
                "<< /Type /Catalog /Pages 2 0 R >>".into(),
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Annots [5 0 R] >>".into(),
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /CropBox [20 30 220 330] /Rotate {rotation} >>"
                ),
                "<< /Subtype /Link /Rect [10 20 30 40] /Dest [4 0 R /FitR 40 200 100 300] >>"
                    .into(),
            ]);
            let document = Pdf::new(bytes).expect("PDF fixture");
            let links = read(&document);
            assert_eq!(
                links[0].first().expect("rectangle link").destination,
                Destination { page: 1, x, y },
                "rotation {rotation}"
            );
        }
    }

    #[test]
    fn skips_invalid_hidden_and_non_navigation_links() {
        let bytes = pdf_with_objects(&[
            "<< /Type /Catalog /Pages 2 0 R /Names << /Dests 6 0 R >> >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Annots [5 0 R << /Subtype /Link /Rect [10 20 30 40] /Dest (cycle) >> << /Subtype /Link /Rect [10 20 30 40] /Dest [999 0 R /Fit] >> << /Subtype /Link /Rect [10 20 10 30] /Dest [4 0 R /Fit] >> << /Subtype /Link /Rect [10 20 30 40] /F 2 /Dest [4 0 R /Fit] >> << /Subtype /Link /Rect [10 20 30 40] /F 32 /Dest [4 0 R /Fit] >>] >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 600] >>".into(),
            "<< /Subtype /Link /Rect [10 20 30 40] /A << /S /URI /URI (https://example.com) >> >>".into(),
            "<< /Kids [6 0 R] /Names [(cycle) (cycle)] >>".into(),
        ]);
        let document = Pdf::new(bytes).expect("PDF fixture");
        assert!(read(&document).iter().all(Vec::is_empty));
    }
}
