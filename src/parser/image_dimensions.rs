use std::io::Cursor;

use anyhow::{Context, Result, bail, ensure};
use exif::{In, Reader, Tag};
use imagesize::ImageType;

use crate::types::ImageSize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMetadata {
    pub size: ImageSize,
    pub extension: &'static str,
    pub mime_type: &'static str,
}

/// Reads encoded image metadata without decoding the pixel buffer.
pub fn probe_image(bytes: &[u8]) -> Result<ImageMetadata> {
    let kind = imagesize::image_type(bytes).context("无法识别图片格式")?;
    let (extension, mime_type) = match kind {
        ImageType::Jpeg => ("jpg", "image/jpeg"),
        ImageType::Png => ("png", "image/png"),
        ImageType::Gif => ("gif", "image/gif"),
        ImageType::Webp => ("webp", "image/webp"),
        _ => bail!("Markdown 图片暂不支持此文件格式"),
    };
    let dimensions = imagesize::blob_size(bytes).context("无法读取有效图片尺寸")?;
    let mut size = ImageSize {
        width: u32::try_from(dimensions.width).context("图片宽度超出支持范围")?,
        height: u32::try_from(dimensions.height).context("图片高度超出支持范围")?,
    };
    ensure!(size.width > 0 && size.height > 0, "图片宽高必须大于零");

    // The encoded JPEG/WebP/PNG dimensions precede any display rotation.
    // Browsers apply EXIF orientation, so the declared markdown ratio must do so too.
    if kind != ImageType::Gif {
        let orientation = Reader::new()
            .read_from_container(&mut Cursor::new(bytes))
            .ok()
            .and_then(|metadata| {
                metadata
                    .get_field(Tag::Orientation, In::PRIMARY)
                    .and_then(|field| field.value.get_uint(0))
            });
        if matches!(orientation, Some(5..=8)) {
            std::mem::swap(&mut size.width, &mut size.height);
        }
    }
    Ok(ImageMetadata {
        size,
        extension,
        mime_type,
    })
}

/// Returns metadata only when a partial response already establishes display orientation.
/// A missing EXIF result on a truncated container does not establish its absence.
pub fn probe_image_prefix(bytes: &[u8]) -> Result<Option<ImageMetadata>> {
    let Ok(kind) = imagesize::image_type(bytes) else {
        return Ok(None);
    };
    ensure!(
        matches!(
            kind,
            ImageType::Jpeg | ImageType::Png | ImageType::Gif | ImageType::Webp
        ),
        "Markdown 图片暂不支持此文件格式"
    );
    if kind != ImageType::Gif {
        let Ok(metadata) = Reader::new().read_from_container(&mut Cursor::new(bytes)) else {
            return Ok(None);
        };
        let orientation = metadata
            .get_field(Tag::Orientation, In::PRIMARY)
            .and_then(|field| field.value.get_uint(0));
        if !matches!(orientation, Some(1..=8)) {
            return Ok(None);
        }
    }
    // JPEG dimensions may follow large APP segments beyond the downloaded prefix.
    Ok(probe_image(bytes).ok())
}

/// Restricts display width while preserving aspect ratio and original small sizes.
pub fn markdown_size(size: &ImageSize) -> ImageSize {
    let width = size.width.max(1);
    let height = size.height.max(1);
    let display_width = width.min(750);
    let display_height =
        (u64::from(height) * u64::from(display_width) + u64::from(width) / 2) / u64::from(width);
    ImageSize {
        width: display_width,
        height: display_height.max(1) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const PREFIX_BYTES: usize = 64 * 1024;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&vec![0; (width * height) as usize])
                .unwrap();
        }
        bytes
    }

    // Header fixtures deliberately exercise metadata parsing, without decoding pixels.
    fn tiff_orientation(orientation: u16) -> Vec<u8> {
        let mut exif = b"II\x2a\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]);
        exif.extend_from_slice(&orientation.to_le_bytes());
        exif.extend_from_slice(&[0; 6]);
        exif
    }

    fn jpeg(orientation: u16) -> Vec<u8> {
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff_orientation(orientation));
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
        bytes.extend_from_slice(&exif);
        bytes.extend_from_slice(&[
            0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x02, 0x00, 0x04, 0x03, 0x01, 0x11, 0x00, 0x02,
            0x11, 0x00, 0x03, 0x11, 0x00, 0xff, 0xd9,
        ]);
        bytes
    }

    fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut chunk = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(payload);
        let crc = crate::parser::zipcrypto::crc32(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        chunk
    }

    fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut chunk = kind.to_vec();
        chunk.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        chunk.extend_from_slice(payload);
        if !payload.len().is_multiple_of(2) {
            chunk.push(0);
        }
        chunk
    }

    #[test]
    fn probes_png_gif_and_webp_without_assuming_a_filename() {
        let parsed = probe_image(&png(4, 2)).unwrap();
        assert_eq!(
            parsed.size,
            ImageSize {
                width: 4,
                height: 2
            }
        );
        assert_eq!((parsed.extension, parsed.mime_type), ("png", "image/png"));

        let mut gif = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
            .unwrap();
        gif[6..10].copy_from_slice(&[2, 0, 3, 0]);
        let parsed = probe_image(&gif).unwrap();
        assert_eq!(
            parsed.size,
            ImageSize {
                width: 2,
                height: 3
            }
        );
        assert_eq!((parsed.extension, parsed.mime_type), ("gif", "image/gif"));

        let webp = base64::engine::general_purpose::STANDARD
            .decode("UklGRiIAAABXRUJQVlA4IBYAAAAwAQCdASoBAAEAAUAmJaQAA3AA/v89WAAAAA==")
            .unwrap();
        let parsed = probe_image(&webp).unwrap();
        assert_eq!(
            parsed.size,
            ImageSize {
                width: 1,
                height: 1
            }
        );
        assert_eq!((parsed.extension, parsed.mime_type), ("webp", "image/webp"));
    }

    #[test]
    fn applies_jpeg_display_orientation_before_markdown_scaling() {
        for orientation in 1..=8 {
            let parsed = probe_image(&jpeg(orientation)).unwrap();
            let expected = if orientation >= 5 {
                ImageSize {
                    width: 2,
                    height: 4,
                }
            } else {
                ImageSize {
                    width: 4,
                    height: 2,
                }
            };
            assert_eq!(parsed.size, expected);
            assert_eq!((parsed.extension, parsed.mime_type), ("jpg", "image/jpeg"));
        }
    }

    #[test]
    fn prefixes_require_complete_orientation_but_gif_needs_only_its_header() {
        let jpeg = jpeg(6);
        assert_eq!(
            probe_image_prefix(&jpeg).unwrap(),
            Some(probe_image(&jpeg).unwrap())
        );
        let gif = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
            .unwrap();
        assert_eq!(
            probe_image_prefix(&gif).unwrap(),
            Some(probe_image(&gif).unwrap())
        );
        // PNG's EXIF reader also returns NotFound at a chunk-boundary EOF.
        // That can be an incomplete prefix immediately before a later eXIf chunk.
        let png = png(4, 2);
        assert!(probe_image_prefix(&png[..33]).unwrap().is_none());
        assert!(probe_image_prefix(b"\xff\xd8\xff").unwrap().is_none());
    }

    #[test]
    fn large_png_and_webp_prefixes_do_not_miss_late_exif_rotation() {
        let orientation = tiff_orientation(6);
        let mut png = png(4, 2);
        let mut text = b"Comment\0".to_vec();
        text.extend(std::iter::repeat_n(b'x', 70 * 1024));
        let mut extra = png_chunk(b"tEXt", &text);
        extra.extend(png_chunk(b"eXIf", &orientation));
        let iend = png.len() - 12;
        png.splice(iend..iend, extra);
        assert!(probe_image_prefix(&png[..PREFIX_BYTES]).unwrap().is_none());
        assert_eq!(
            probe_image(&png).unwrap().size,
            ImageSize {
                width: 2,
                height: 4
            }
        );

        // An extended WebP container declares a 4x2 canvas, followed by a large
        // ancillary chunk before EXIF. Pixel decoding is outside this metadata test.
        let mut chunks = webp_chunk(b"VP8X", &[8, 0, 0, 0, 3, 0, 0, 1, 0, 0]);
        chunks.extend(webp_chunk(b"JUNK", &vec![0; 70 * 1024]));
        chunks.extend(webp_chunk(b"EXIF", &orientation));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&u32::try_from(chunks.len() + 4).unwrap().to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend(chunks);
        assert!(probe_image_prefix(&webp[..PREFIX_BYTES]).unwrap().is_none());
        assert_eq!(
            probe_image(&webp).unwrap().size,
            ImageSize {
                width: 2,
                height: 4
            }
        );
    }

    #[test]
    fn large_jpeg_app_segments_request_the_full_file_for_size_or_orientation() {
        let original = jpeg(6);
        let mut application_segments = Vec::new();
        for _ in 0..2 {
            application_segments.extend_from_slice(&[0xff, 0xe2]);
            application_segments.extend_from_slice(&(40u16 * 1024 + 2).to_be_bytes());
            application_segments.extend(std::iter::repeat_n(0, 40 * 1024));
        }
        let mut late_size = vec![0xff, 0xd8];
        late_size.extend_from_slice(&application_segments);
        late_size.extend_from_slice(&original[2..]);
        assert!(
            probe_image_prefix(&late_size[..PREFIX_BYTES])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            probe_image(&late_size).unwrap().size,
            ImageSize {
                width: 2,
                height: 4
            }
        );

        let application_end = 4 + usize::from(u16::from_be_bytes([original[4], original[5]]));
        let mut late_orientation = vec![0xff, 0xd8];
        late_orientation.extend_from_slice(&original[application_end..original.len() - 2]);
        late_orientation.extend(application_segments);
        late_orientation.extend_from_slice(&original[2..application_end]);
        late_orientation.extend_from_slice(&[0xff, 0xd9]);
        assert!(
            probe_image_prefix(&late_orientation[..PREFIX_BYTES])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            probe_image(&late_orientation).unwrap().size,
            ImageSize {
                width: 2,
                height: 4
            }
        );
    }

    #[test]
    fn rejects_missing_dimensions_and_non_image_responses() {
        for bytes in [
            b"".as_slice(),
            b"<html>login required</html>",
            b"\x89PNG\r\n\x1a\n",
        ] {
            assert!(probe_image(bytes).is_err());
        }
        let mut zero_width = png(2, 2);
        zero_width[16..20].fill(0);
        assert!(probe_image(&zero_width).is_err());
        assert!(probe_image(&jpeg(1)[..10]).is_err());
    }

    #[test]
    fn bounds_width_without_stretching_portraits_or_long_images() {
        for (original, expected) in [
            ((3000, 2000), (750, 500)),
            ((1000, 3000), (750, 2250)),
            ((1200, 24000), (750, 15000)),
            ((320, 200), (320, 200)),
            ((1000, 1), (750, 1)),
            ((u32::MAX, u32::MAX), (750, 750)),
        ] {
            assert_eq!(
                markdown_size(&ImageSize {
                    width: original.0,
                    height: original.1
                }),
                ImageSize {
                    width: expected.0,
                    height: expected.1
                },
            );
        }
    }
}
