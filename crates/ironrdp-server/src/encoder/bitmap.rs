use core::num::NonZeroUsize;

use ironrdp_core::{Encode as _, WriteCursor, cast_int, invalid_field_err};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::rdp6::{
    ABgrChannels, ARgbChannels, BgrAChannels, BitmapEncodeError, BitmapStreamEncoder, RgbAChannels,
};
use ironrdp_pdu::bitmap::{BitmapData, BitmapUpdateData, Compression};
use ironrdp_pdu::geometry::InclusiveRectangle;

use crate::BitmapUpdate;

// PERF: we could also remove the need for this buffer
#[derive(Clone)]
pub(crate) struct BitmapEncoder {
    buffer: Vec<u8>,
}

#[allow(dead_code)]
impl BitmapEncoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; usize::from(u16::MAX)],
        }
    }

    pub(crate) fn encode(
        &mut self,
        bitmap: &BitmapUpdate,
        output: &mut [u8],
    ) -> Result<usize, BitmapEncodeError> {
        // FIXME: support non-multiple of 4 widths.
        //
        // It’s not clear how to achieve that yet, but generally, server uses multiple of 4-widths,
        // and client has surface capabilities, so this path is unlikely.
        if !bitmap.width.get().is_multiple_of(4) {
            return Err(BitmapEncodeError::Encode(invalid_field_err!(
                "bitmap",
                "Width must be a multiple of 4"
            )));
        }

        let bytes_per_pixel = u16::from(bitmap.format.bytes_per_pixel());
        let row_len = bitmap.width.get() * bytes_per_pixel;
        let chunk_height = u16::MAX / row_len;

        let mut cursor = WriteCursor::new(output);
        let stride = bitmap.stride.get();
        let chunks = bitmap.data.chunks(stride * usize::from(chunk_height));

        let total = cast_int!("chunks length lower bound", chunks.size_hint().0)
            .map_err(BitmapEncodeError::Encode)?;
        BitmapUpdateData::encode_header(total, &mut cursor).map_err(BitmapEncodeError::Encode)?;

        for (i, chunk) in chunks.enumerate() {
            let height = cast_int!("bitmap height", chunk.len() / stride)
                .map_err(BitmapEncodeError::Encode)?;
            let i: u16 = cast_int!("chunk idx", i).map_err(BitmapEncodeError::Encode)?;
            let top = bitmap.y + i * chunk_height;

            let encoder = BitmapStreamEncoder::new(
                NonZeroUsize::from(bitmap.width).get(),
                usize::from(height),
            );

            let len = {
                let pixels = chunk
                    .chunks(stride)
                    .map(|row| &row[..usize::from(row_len).min(row.len())])
                    .rev()
                    .flat_map(|row| row.chunks(usize::from(bytes_per_pixel)));

                Self::encode_iter(encoder, bitmap.format, pixels, self.buffer.as_mut_slice())?
            };

            let data = BitmapData {
                rectangle: InclusiveRectangle {
                    left: bitmap.x,
                    top,
                    right: bitmap.x + bitmap.width.get() - 1,
                    bottom: top + height - 1,
                },
                width: u16::from(bitmap.width),
                height,
                bits_per_pixel: 32,
                compression_flags: Compression::BITMAP_COMPRESSION
                    | Compression::NO_BITMAP_COMPRESSION_HDR,
                compressed_data_header: None,
                bitmap_data: &self.buffer[..len],
            };

            data.encode(&mut cursor)
                .map_err(BitmapEncodeError::Encode)?;
        }

        Ok(cursor.pos())
    }

    fn encode_iter<'a, P>(
        mut encoder: BitmapStreamEncoder,
        format: PixelFormat,
        src: P,
        dst: &mut [u8],
    ) -> Result<usize, BitmapEncodeError>
    where
        P: Iterator<Item = &'a [u8]> + Clone,
    {
        let written = match format {
            PixelFormat::ARgb32 | PixelFormat::XRgb32 => {
                encoder.encode_pixels_stream::<_, ARgbChannels>(src, dst, true)?
            }
            PixelFormat::RgbA32 | PixelFormat::RgbX32 => {
                encoder.encode_pixels_stream::<_, RgbAChannels>(src, dst, true)?
            }
            PixelFormat::ABgr32 | PixelFormat::XBgr32 => {
                encoder.encode_pixels_stream::<_, ABgrChannels>(src, dst, true)?
            }
            PixelFormat::BgrA32 | PixelFormat::BgrX32 => {
                encoder.encode_pixels_stream::<_, BgrAChannels>(src, dst, true)?
            }
        };

        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ironrdp_core::{Decode, ReadCursor};
    use ironrdp_graphics::rdp6::BitmapStreamDecoder;
    use std::num::NonZeroU16;

    #[test]
    fn test_bitmap_encoder_decode() {
        let width = 1920u16;
        let height = 64u16;
        let stride = (width as usize) * 4;
        let mut raw = vec![0u8; stride * (height as usize)];
        for (i, px) in raw.chunks_exact_mut(4).enumerate() {
            px[0] = (i % 255) as u8; // B
            px[1] = ((i * 2) % 255) as u8; // G
            px[2] = ((i * 3) % 255) as u8; // R
            px[3] = 255; // A
        }

        let update = BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            stride: NonZeroUsize::new(stride).unwrap(),
            format: PixelFormat::BgrA32,
            data: Bytes::from(raw.clone()),
        };

        let mut encoder = BitmapEncoder::new();
        let mut output = vec![0u8; 1024 * 1024];
        let len = encoder.encode(&update, &mut output).unwrap();
        assert!(len > 0);

        // Now decode BitmapUpdateData
        let mut cursor = ReadCursor::new(&output[..len]);
        let update_data = ironrdp_pdu::bitmap::BitmapUpdateData::decode(&mut cursor).unwrap();
        assert_eq!(update_data.rectangles.len(), 8);

        // Decode each chunk
        let mut decoder = BitmapStreamDecoder::default();
        let mut decoded = vec![0u8; stride * (height as usize)];
        for rect in update_data.rectangles {
            let chunk_h = rect.height as usize;
            let chunk_w = rect.width as usize;
            let top = rect.rectangle.top as usize;
            let mut chunk_out = vec![0u8; chunk_w * chunk_h * 4];
            decoder
                .decode_bitmap_stream_to_rgb24(rect.bitmap_data, &mut chunk_out, chunk_w, chunk_h)
                .unwrap();
        }
    }
}
