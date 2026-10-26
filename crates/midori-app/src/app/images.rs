//! Plugin image decoding helpers.

use eframe::egui;

use midori_plugin::ImageFormat;

/// Convert a plugin image event into an egui texture source.
pub(crate) fn decode_image_update(
    format: ImageFormat,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> Result<egui::ColorImage, String> {
    match format {
        ImageFormat::Png => decode_png(bytes),
        ImageFormat::Rgba8 => {
            if width == 0 || height == 0 {
                return Err("RGBA image has no dimensions".to_string());
            }
            let expected = width as usize * height as usize * 4;
            if bytes.len() != expected {
                return Err(format!(
                    "RGBA buffer is {} bytes, expected {expected}",
                    bytes.len()
                ));
            }
            Ok(egui::ColorImage::from_rgba_unmultiplied(
                [width as usize, height as usize],
                bytes,
            ))
        }
        ImageFormat::Jpeg => Err("JPEG display is not supported yet".to_string()),
    }
}

pub(crate) fn decode_png(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "PNG is too large to decode".to_string())?;
    let mut buffer = vec![0u8; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    let data = &buffer[..info.buffer_size()];
    let width = info.width as usize;
    let height = info.height as usize;

    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::Grayscale => data
            .iter()
            .flat_map(|&value| [value, value, value, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Indexed => {
            return Err("indexed PNG was not expanded".to_string());
        }
    };

    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        &rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
        }
        bytes
    }

    #[test]
    fn decodes_png_display_update() {
        let png = test_png();
        let image = decode_image_update(ImageFormat::Png, 2, 1, &png).unwrap();
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(0, 255, 0));
    }

    #[test]
    fn decodes_rgba8_display_update() {
        let image =
            decode_image_update(ImageFormat::Rgba8, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255])
                .unwrap();
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(0, 255, 0));
    }

    #[test]
    fn rejects_bad_rgba8_length() {
        assert!(decode_image_update(ImageFormat::Rgba8, 2, 1, &[0; 4]).is_err());
    }
}
