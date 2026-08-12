use core::num::NonZeroU16;
use core::time::Duration;

use ironrdp_pdu::geometry::{InclusiveRectangle, Rectangle as _};
use ironrdp_session::SessionResult;
use ironrdp_session::image::DecodedImage;

use crate::rdp::RdpOutputEvent;

const GRAPHICS_ACCUMULATOR_QUIET_WINDOW: Duration = Duration::from_millis(2);
const GRAPHICS_ACCUMULATOR_MAX_WINDOW: Duration = Duration::from_millis(8);
const GRAPHICS_ACCUMULATOR_FULL_FRAME_AREA_DIVISOR: u64 = 3;

/// Selects how decoded desktop graphics are published by [`RdpClient`](crate::rdp::RdpClient).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphicsOutputMode {
    /// Publish a complete framebuffer snapshot for every graphics update.
    #[default]
    FullFrame,
    /// Publish a complete base frame followed by packed BGRA dirty rectangles.
    DirtyRegions,
}

/// A packed image region relative to the full remote framebuffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RdpImageRegion {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug)]
pub(crate) struct GraphicsOutputState {
    mode: GraphicsOutputMode,
    needs_base: bool,
    pending_region: Option<RdpImageRegion>,
    first_update_at: Option<tokio::time::Instant>,
    quiet_until: Option<tokio::time::Instant>,
}

impl GraphicsOutputState {
    pub(crate) fn new(mode: GraphicsOutputMode) -> Self {
        Self {
            mode,
            needs_base: true,
            pending_region: None,
            first_update_at: None,
            quiet_until: None,
        }
    }

    pub(crate) fn mode(&self) -> GraphicsOutputMode {
        self.mode
    }

    pub(crate) fn reset(&mut self) {
        self.needs_base = true;
        self.clear_pending();
    }

    pub(crate) fn queue_region(&mut self, image: &DecodedImage, region: InclusiveRectangle) {
        let Some(region) = normalize_region(image, region) else {
            return;
        };

        self.queue_normalized_region(image, region, tokio::time::Instant::now());
    }

    fn queue_normalized_region(&mut self, image: &DecodedImage, region: RdpImageRegion, now: tokio::time::Instant) {
        if self.pending_region.is_none() {
            self.first_update_at = Some(now);
        }
        self.quiet_until = Some(now + GRAPHICS_ACCUMULATOR_QUIET_WINDOW);
        self.pending_region = Some(match self.pending_region {
            Some(pending) => union_regions(pending, region).unwrap_or(RdpImageRegion {
                x: 0,
                y: 0,
                width: image.width(),
                height: image.height(),
            }),
            None => region,
        });
    }

    pub(crate) fn next_flush_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending_region?;
        let quiet_until = self.quiet_until?;
        Some(match self.first_update_at {
            Some(first_update_at) => quiet_until.min(first_update_at + GRAPHICS_ACCUMULATOR_MAX_WINDOW),
            None => quiet_until,
        })
    }

    pub(crate) fn take_immediate_event(&mut self, image: &DecodedImage) -> SessionResult<Option<RdpOutputEvent>> {
        if self.pending_region.is_none() {
            return Ok(None);
        }
        if !self.needs_base {
            return Ok(None);
        }

        self.clear_pending();
        self.needs_base = false;
        full_frame_event(image).map(Some)
    }

    pub(crate) fn take_pending_event(&mut self, image: &DecodedImage) -> SessionResult<Option<RdpOutputEvent>> {
        let Some(region) = self.pending_region else {
            return Ok(None);
        };
        self.clear_pending();

        if self.needs_base || region_requires_full_frame(region, image) {
            self.needs_base = false;
            return full_frame_event(image).map(Some);
        }

        region_event(image, region).map(Some)
    }

    fn clear_pending(&mut self) {
        self.pending_region = None;
        self.first_update_at = None;
        self.quiet_until = None;
    }
}

pub(crate) fn full_frame_event(image: &DecodedImage) -> SessionResult<RdpOutputEvent> {
    let width = NonZeroU16::new(image.width()).ok_or_else(|| ironrdp_session::general_err!("width is zero"))?;
    let height = NonZeroU16::new(image.height()).ok_or_else(|| ironrdp_session::general_err!("height is zero"))?;
    let [red, green, blue, _alpha] = image.pixel_format().channel_offsets();
    let buffer = image
        .data()
        .chunks_exact(image.bytes_per_pixel())
        .map(|pixel| u32::from_be_bytes([0, pixel[red], pixel[green], pixel[blue]]))
        .collect();

    Ok(RdpOutputEvent::Image { buffer, width, height })
}

fn region_event(image: &DecodedImage, region: RdpImageRegion) -> SessionResult<RdpOutputEvent> {
    let width = NonZeroU16::new(image.width()).ok_or_else(|| ironrdp_session::general_err!("width is zero"))?;
    let height = NonZeroU16::new(image.height()).ok_or_else(|| ironrdp_session::general_err!("height is zero"))?;
    let bytes_per_pixel = image.bytes_per_pixel();
    let stride = image.stride();
    let [red, green, blue, _alpha] = image.pixel_format().channel_offsets();
    let bgra = copy_region_bgra(image.data(), stride, bytes_per_pixel, [red, green, blue], region)?;

    Ok(RdpOutputEvent::ImageRegion {
        bgra,
        width,
        height,
        region,
    })
}

fn copy_region_bgra(
    image_data: &[u8],
    stride: usize,
    bytes_per_pixel: usize,
    [red, green, blue]: [usize; 3],
    region: RdpImageRegion,
) -> SessionResult<Vec<u8>> {
    if red >= bytes_per_pixel || green >= bytes_per_pixel || blue >= bytes_per_pixel {
        return Err(ironrdp_session::general_err!(
            "pixel channel offset is outside the pixel"
        ));
    }

    let region_width = usize::from(region.width);
    let region_height = usize::from(region.height);
    let row_bytes = region_width
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| ironrdp_session::general_err!("dirty region row size overflow"))?;
    let output_len = region_width
        .checked_mul(region_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ironrdp_session::general_err!("dirty region output size overflow"))?;
    let mut bgra = Vec::with_capacity(output_len);

    for row in 0..region_height {
        let start = usize::from(region.y)
            .checked_add(row)
            .and_then(|y| y.checked_mul(stride))
            .and_then(|offset| {
                usize::from(region.x)
                    .checked_mul(bytes_per_pixel)
                    .and_then(|x| offset.checked_add(x))
            })
            .ok_or_else(|| ironrdp_session::general_err!("dirty region offset overflow"))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| ironrdp_session::general_err!("dirty region row end overflow"))?;
        let row = image_data
            .get(start..end)
            .ok_or_else(|| ironrdp_session::general_err!("dirty region is outside the decoded image"))?;
        for pixel in row.chunks_exact(bytes_per_pixel) {
            bgra.extend_from_slice(&[pixel[blue], pixel[green], pixel[red], 0xff]);
        }
    }

    Ok(bgra)
}

fn normalize_region(image: &DecodedImage, region: InclusiveRectangle) -> Option<RdpImageRegion> {
    if region.left > region.right
        || region.top > region.bottom
        || region.right >= image.width()
        || region.bottom >= image.height()
    {
        return None;
    }

    Some(RdpImageRegion {
        x: region.left,
        y: region.top,
        width: region.width(),
        height: region.height(),
    })
}

fn union_regions(left: RdpImageRegion, right: RdpImageRegion) -> Option<RdpImageRegion> {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let right_edge = u32::from(left.x)
        .checked_add(u32::from(left.width))?
        .max(u32::from(right.x).checked_add(u32::from(right.width))?);
    let bottom_edge = u32::from(left.y)
        .checked_add(u32::from(left.height))?
        .max(u32::from(right.y).checked_add(u32::from(right.height))?);

    Some(RdpImageRegion {
        x,
        y,
        width: u16::try_from(right_edge.checked_sub(u32::from(x))?).ok()?,
        height: u16::try_from(bottom_edge.checked_sub(u32::from(y))?).ok()?,
    })
}

fn region_requires_full_frame(region: RdpImageRegion, image: &DecodedImage) -> bool {
    let region_area = u64::from(region.width).saturating_mul(u64::from(region.height));
    let image_area = u64::from(image.width()).saturating_mul(u64::from(image.height()));
    region_covers_image(region, image)
        || region_area.saturating_mul(GRAPHICS_ACCUMULATOR_FULL_FRAME_AREA_DIVISOR) >= image_area
}

fn region_covers_image(region: RdpImageRegion, image: &DecodedImage) -> bool {
    region.x == 0 && region.y == 0 && region.width == image.width() && region.height == image.height()
}

#[cfg(test)]
mod tests {
    use ironrdp_graphics::image_processing::PixelFormat;

    use super::*;

    #[test]
    fn normalizes_inclusive_single_pixel_region() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 4, 3);

        assert_eq!(
            normalize_region(
                &image,
                InclusiveRectangle {
                    left: 2,
                    top: 1,
                    right: 2,
                    bottom: 1,
                },
            ),
            Some(RdpImageRegion {
                x: 2,
                y: 1,
                width: 1,
                height: 1,
            })
        );
    }

    #[test]
    fn rejects_stale_or_invalid_regions() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 4, 3);

        for region in [
            InclusiveRectangle {
                left: 3,
                top: 0,
                right: 2,
                bottom: 0,
            },
            InclusiveRectangle {
                left: 0,
                top: 2,
                right: 0,
                bottom: 1,
            },
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 4,
                bottom: 0,
            },
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 0,
                bottom: 3,
            },
        ] {
            assert_eq!(normalize_region(&image, region), None);
        }
    }

    #[test]
    fn unions_regions_using_exclusive_right_and_bottom_edges() {
        assert_eq!(
            union_regions(
                RdpImageRegion {
                    x: 1,
                    y: 2,
                    width: 2,
                    height: 3,
                },
                RdpImageRegion {
                    x: 4,
                    y: 1,
                    width: 3,
                    height: 2,
                },
            ),
            Some(RdpImageRegion {
                x: 1,
                y: 1,
                width: 6,
                height: 4,
            })
        );
    }

    #[test]
    fn packs_multiple_rows_as_bgra_without_stride_gaps() {
        let image_data = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, //
            17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
        ];

        let bgra = copy_region_bgra(
            &image_data,
            16,
            4,
            [0, 1, 2],
            RdpImageRegion {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();

        assert_eq!(
            bgra,
            [
                7, 6, 5, 0xff, 11, 10, 9, 0xff, //
                23, 22, 21, 0xff, 27, 26, 25, 0xff,
            ]
        );
    }

    #[test]
    fn first_update_is_an_immediate_base_frame() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 2, 2);
        let mut output = GraphicsOutputState::new(GraphicsOutputMode::DirtyRegions);
        output.queue_region(
            &image,
            InclusiveRectangle {
                left: 1,
                top: 1,
                right: 1,
                bottom: 1,
            },
        );

        assert!(matches!(
            output.take_immediate_event(&image).unwrap(),
            Some(RdpOutputEvent::Image { .. })
        ));
        assert!(output.take_immediate_event(&image).unwrap().is_none());
    }

    #[test]
    fn quiet_and_max_windows_bound_the_flush_deadline() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 8, 8);
        let mut output = GraphicsOutputState::new(GraphicsOutputMode::DirtyRegions);
        output.needs_base = false;
        let started_at = tokio::time::Instant::now();
        let region = RdpImageRegion {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };

        output.queue_normalized_region(&image, region, started_at);
        assert_eq!(
            output.next_flush_deadline(),
            Some(started_at + GRAPHICS_ACCUMULATOR_QUIET_WINDOW)
        );

        output.queue_normalized_region(
            &image,
            region,
            started_at + GRAPHICS_ACCUMULATOR_MAX_WINDOW - Duration::from_millis(1),
        );
        assert_eq!(
            output.next_flush_deadline(),
            Some(started_at + GRAPHICS_ACCUMULATOR_MAX_WINDOW)
        );
    }

    #[test]
    fn large_accumulated_region_promotes_to_full_frame() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 6, 3);
        let mut output = GraphicsOutputState::new(GraphicsOutputMode::DirtyRegions);
        output.needs_base = false;
        output.queue_region(
            &image,
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 1,
                bottom: 2,
            },
        );

        assert!(matches!(
            output.take_pending_event(&image).unwrap(),
            Some(RdpOutputEvent::Image { .. })
        ));
    }

    #[test]
    fn reset_requires_another_base_frame() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 2, 2);
        let mut output = GraphicsOutputState::new(GraphicsOutputMode::DirtyRegions);
        output.needs_base = false;
        output.reset();
        output.queue_region(
            &image,
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        );

        assert!(matches!(
            output.take_immediate_event(&image).unwrap(),
            Some(RdpOutputEvent::Image { .. })
        ));
    }
}
