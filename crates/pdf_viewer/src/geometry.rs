use crate::{PAGE_GAP, raster::PageSize};
use std::ops::Range;

pub struct PageGeometry {
    offsets: Vec<f32>,
    pub max_width: f32,
}

impl PageGeometry {
    pub fn new(pages: &[PageSize]) -> Self {
        let mut offsets = Vec::with_capacity(pages.len() + 1);
        let mut height = 0.0;
        let mut max_width = 1.0_f32;
        for page in pages {
            offsets.push(height);
            height += page.height;
            max_width = max_width.max(page.width);
        }
        offsets.push(height);
        Self { offsets, max_width }
    }

    pub fn top(&self, index: usize, scale: f32) -> f32 {
        self.offsets.get(index).copied().unwrap_or_default() * scale + (index + 1) as f32 * PAGE_GAP
    }

    pub fn height(&self, scale: f32) -> f32 {
        self.top(self.offsets.len().saturating_sub(1), scale)
    }

    pub fn visible(&self, scale: f32, top: f32, height: f32) -> Range<usize> {
        let count = self.offsets.len().saturating_sub(1);
        let start = partition_point(count, |index| self.top(index + 1, scale) - PAGE_GAP <= top);
        let end = partition_point(count, |index| self.top(index, scale) < top + height);
        start..end.max(start)
    }

    pub fn page_at(&self, scale: f32, offset: f32) -> usize {
        let count = self.offsets.len().saturating_sub(1);
        partition_point(count, |index| self.top(index + 1, scale) <= offset)
            .min(count.saturating_sub(1))
    }
}

fn partition_point(count: usize, predicate: impl Fn(usize) -> bool) -> usize {
    let mut start = 0;
    let mut end = count;
    while start < end {
        let middle = start + (end - start) / 2;
        if predicate(middle) {
            start = middle + 1;
        } else {
            end = middle;
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_visibility_matches_linear_layout() {
        let pages = (0..1000)
            .map(|index| PageSize {
                width: 200.0 + index as f32 % 70.0,
                height: 100.0 + index as f32 % 300.0,
            })
            .collect::<Vec<_>>();
        let geometry = PageGeometry::new(&pages);
        for scale in [0.1, 0.75, 1.0, 4.0] {
            for top in [
                0.0,
                16.0,
                300.0,
                30_000.0,
                geometry.height(scale) - 200.0,
                geometry.height(scale) + 1.0,
            ] {
                let mut offset = PAGE_GAP;
                let mut expected = Vec::new();
                for (index, page) in pages.iter().enumerate() {
                    let bottom = offset + page.height * scale;
                    if bottom > top && offset < top + 200.0 {
                        expected.push(index);
                    }
                    offset = bottom + PAGE_GAP;
                }
                assert_eq!(
                    geometry.visible(scale, top, 200.0).collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }

    #[test]
    fn page_lookup_is_logarithmic() {
        let visits = std::cell::Cell::new(0);
        assert_eq!(
            partition_point(1_000_000, |index| {
                visits.set(visits.get() + 1);
                index < 999_999
            }),
            999_999
        );
        assert!(visits.get() <= 20);
    }
}
