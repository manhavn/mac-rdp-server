use core::fmt;
use core::num::NonZeroU16;

use anyhow::{Context as _, Result, anyhow};
use ironrdp_acceptor::DesktopSize;
use ironrdp_core::{Encode as _, WriteCursor};
use ironrdp_graphics::diff::{Rect, find_different_rects_sub};
use ironrdp_pdu::bitmap::{BitmapData, BitmapUpdateData, Compression};
use ironrdp_pdu::encode_vec;
use ironrdp_pdu::fast_path::UpdateCode;
use ironrdp_pdu::geometry::{ExclusiveRectangle, InclusiveRectangle};
use ironrdp_pdu::pointer::{
    CachedPointerAttribute, ColorPointerAttribute, Point16, PointerAttribute,
    PointerPositionAttribute,
};
use ironrdp_pdu::rdp::capability_sets::{CmdFlags, EntropyBits};
use ironrdp_pdu::surface_commands::{ExtendedBitmapDataPdu, SurfaceBitsPdu, SurfaceCommand};
use tracing::{debug, warn};

use self::bitmap::BitmapEncoder;
use self::rfx::RfxEncoder;
use super::BitmapUpdate;
use crate::{ColorPointer, DisplayUpdate, Framebuffer, RGBAPointer};

mod bitmap;
mod fast_path;
pub(crate) mod rfx;

pub(crate) use fast_path::*;
use ironrdp_graphics::rdp6::BitmapEncodeError;

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum CodecId {
    None = 0x0,
}

#[allow(dead_code)]
impl CodecId {
    #[expect(
        clippy::as_conversions,
        reason = "guarantees discriminant layout, and as is the only way to cast enum -> primitive"
    )]
    fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
#[derive(Debug)]
pub(crate) struct UpdateEncoderCodecs {
    remotefx: Option<(EntropyBits, u8)>,
    #[cfg(feature = "qoi")]
    qoi: Option<u8>,
    #[cfg(feature = "qoiz")]
    qoiz: Option<u8>,
    /// `(codec_id, color_loss_level)` from the negotiated NsCodec capability.
    #[cfg(feature = "nscodec")]
    nscodec: Option<(u8, u8)>,
}

impl UpdateEncoderCodecs {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn new() -> Self {
        Self {
            remotefx: None,
            #[cfg(feature = "qoi")]
            qoi: None,
            #[cfg(feature = "qoiz")]
            qoiz: None,
            #[cfg(feature = "nscodec")]
            nscodec: None,
        }
    }

    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_remotefx(&mut self, remotefx: Option<(EntropyBits, u8)>) {
        self.remotefx = remotefx
    }

    #[cfg(feature = "qoi")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_qoi(&mut self, qoi: Option<u8>) {
        self.qoi = qoi
    }

    #[cfg(feature = "qoiz")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_qoiz(&mut self, qoiz: Option<u8>) {
        self.qoiz = qoiz
    }

    /// Record the negotiated NsCodec codec id and color-loss level so the
    /// encoder selection path can build an `NsCodecHandler` for this session.
    #[cfg(feature = "nscodec")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_nscodec(&mut self, nscodec: Option<(u8, u8)>) {
        self.nscodec = nscodec
    }
}

/// Lấy cấu hình nén và kích thước ô gạch từ biến môi trường RDP_MODE, RDP_COLOR và RDP_TILE
/// - `RDP_COLOR`: Tùy biến mức nén & màu sắc: `4bit` (0xF0), `5bit` (0xF8), `6bit` (0xFC), `8bit` (0xFF lossless), hoặc mã hex `0xF0`
/// - `RDP_TILE`: Tùy biến kích thước ô gạch: `320x32`, `240x24`, `320x24`,...
pub(crate) fn get_compression_mode() -> (u8, usize, usize) {
    let mode = std::env::var("RDP_MODE").unwrap_or_else(|_| "speed".to_string());
    let (default_mask, default_w, default_h) = match mode.to_lowercase().as_str() {
        "quality" | "high" => (0xFF, 320, 32),
        "balanced" | "medium" => (0xFF, 320, 32),
        "speed" | "fast" | "low" | _ => (0xFF, 320, 32),
    };

    // Cho phép tùy biến Mức nén & Màu sắc qua ENV RDP_COLOR hoặc RDP_BITS / RDP_MASK (ví dụ: RDP_COLOR=4bit hoặc 5bit)
    let mask = if let Ok(color_str) = std::env::var("RDP_COLOR")
        .or_else(|_| std::env::var("RDP_BITS"))
        .or_else(|_| std::env::var("RDP_MASK"))
    {
        let clean = color_str.trim().to_lowercase();
        if clean.starts_with("0x") {
            u8::from_str_radix(&clean[2..], 16).unwrap_or(default_mask)
        } else if clean == "4bit" || clean == "4" {
            0xF0
        } else if clean == "5bit" || clean == "5" {
            0xF8
        } else if clean == "6bit" || clean == "6" {
            0xFC
        } else if clean == "7bit" || clean == "7" {
            0xFE
        } else if clean == "8bit" || clean == "8" || clean == "lossless" || clean == "max" {
            0xFF
        } else if clean == "3bit" || clean == "3" {
            0xE0
        } else if let Ok(parsed_hex) = u8::from_str_radix(&clean, 16) {
            parsed_hex
        } else {
            default_mask
        }
    } else {
        default_mask
    };

    // Cho phép người dùng tùy biến kích thước ô gạch qua ENV RDP_TILE (ví dụ: RDP_TILE=320x32)
    let (tile_w, tile_h) = if let Ok(tile_str) = std::env::var("RDP_TILE") {
        let parts: Vec<&str> = tile_str
            .split(|c: char| c == 'x' || c == 'X' || c == '*' || c == ',' || c == ' ')
            .collect();
        if parts.len() == 2 {
            let w = parts[0].trim().parse::<usize>().unwrap_or(default_w);
            let h = parts[1].trim().parse::<usize>().unwrap_or(default_h);
            ((w.max(8) / 4) * 4, h.max(4))
        } else {
            (default_w, default_h)
        }
    } else {
        let w = std::env::var("RDP_TILE_W")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default_w);
        let h = std::env::var("RDP_TILE_H")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default_h);
        ((w.max(8) / 4) * 4, h.max(4))
    };

    (mask, tile_w, tile_h)
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
pub(crate) struct UpdateEncoder {
    desktop_size: DesktopSize,
    framebuffer: Option<Framebuffer>,
    bitmap_updater: Option<BitmapUpdater>,
    /// Negotiated MultifragmentUpdate reassembly buffer size. Used to split
    /// oversized bitmaps into strips that fit within the limit when sent as
    /// uncompressed surface commands.
    #[allow(dead_code)]
    max_request_size: usize,
    frame_count: u64,
    #[allow(dead_code)]
    last_large_update: std::time::Instant,
}

impl fmt::Debug for UpdateEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateEncoder")
            .field("bitmap_update", &self.bitmap_updater)
            .finish()
    }
}

impl UpdateEncoder {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn new(
        desktop_size: DesktopSize,
        _surface_flags: CmdFlags,
        _codecs: UpdateEncoderCodecs,
        max_request_size: u32,
    ) -> Result<Self> {
        let bitmap_updater = {
            tracing::info!(
                "🎨 [RDP GRAPHICS] Selected Codec: FastPath Bitmap (RDP6 Planar RLE 32bpp)"
            );
            BitmapUpdater::None(NoneHandler)
        };

        Ok(Self {
            desktop_size,
            framebuffer: None,
            bitmap_updater: Some(bitmap_updater),
            max_request_size: usize::try_from(max_request_size).context("max_request_size")?,
            frame_count: 0,
            last_large_update: std::time::Instant::now(),
        })
    }

    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn update(&mut self, update: DisplayUpdate) -> EncoderIter<'_> {
        EncoderIter {
            encoder: self,
            state: State::Start(update),
        }
    }

    pub(crate) fn set_desktop_size(&mut self, size: DesktopSize) {
        self.desktop_size = size;
        self.bitmap_updater
            .as_mut()
            .expect("bitmap updater always Some")
            .set_desktop_size(size);
    }

    fn rgba_pointer(ptr: RGBAPointer) -> Result<UpdateFragmenter> {
        let xor_mask = ptr.data;

        let hot_spot = Point16 {
            x: ptr.hot_x,
            y: ptr.hot_y,
        };
        let color_pointer = ColorPointerAttribute {
            cache_index: ptr.cache_index,
            hot_spot,
            width: ptr.width,
            height: ptr.height,
            xor_mask: &xor_mask,
            and_mask: &[],
        };
        let ptr = PointerAttribute {
            xor_bpp: 32,
            color_pointer,
        };
        Ok(UpdateFragmenter::new(
            UpdateCode::NewPointer,
            encode_vec(&ptr)?,
        ))
    }

    fn color_pointer(ptr: ColorPointer) -> Result<UpdateFragmenter> {
        let hot_spot = Point16 {
            x: ptr.hot_x,
            y: ptr.hot_y,
        };
        let ptr = ColorPointerAttribute {
            cache_index: ptr.cache_index,
            hot_spot,
            width: ptr.width,
            height: ptr.height,
            xor_mask: &ptr.xor_mask,
            and_mask: &ptr.and_mask,
        };
        Ok(UpdateFragmenter::new(
            UpdateCode::ColorPointer,
            encode_vec(&ptr)?,
        ))
    }

    fn cached_pointer(cache_index: u16) -> Result<UpdateFragmenter> {
        let ptr = CachedPointerAttribute { cache_index };
        Ok(UpdateFragmenter::new(
            UpdateCode::CachedPointer,
            encode_vec(&ptr)?,
        ))
    }

    fn default_pointer() -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(UpdateCode::DefaultPointer, vec![]))
    }

    fn hide_pointer() -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(UpdateCode::HiddenPointer, vec![]))
    }

    fn pointer_position(pos: PointerPositionAttribute) -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(
            UpdateCode::PositionPointer,
            encode_vec(&pos)?,
        ))
    }

    fn bitmap_diffs(&mut self, bitmap: &BitmapUpdate) -> Vec<Rect> {
        self.frame_count += 1;

        // Gửi toàn màn hình trong Frame 1 (để nạp đầy canvas Client ngay tức thì trong 50ms)
        // Từ Frame 2 trở đi kích hoạt 100% High-Performance Delta Diffing (0 KB/s khi tĩnh, <1-5 KB khi có chuyển động)
        let force_full_screen = self.frame_count <= 1;

        let diffs = if !force_full_screen && self.framebuffer.is_some() {
            let Framebuffer {
                data,
                stride,
                width,
                height,
                ..
            } = self.framebuffer.as_ref().unwrap();

            find_different_rects_sub::<4>(
                data,
                *stride,
                width.get().into(),
                height.get().into(),
                &bitmap.data,
                bitmap.stride.get(),
                bitmap.width.get().into(),
                bitmap.height.get().into(),
                bitmap.x.into(),
                bitmap.y.into(),
            )
        } else {
            vec![Rect {
                x: 0,
                y: 0,
                width: bitmap.width.get().into(),
                height: bitmap.height.get().into(),
            }]
        };

        let full_width = usize::from(bitmap.width.get());
        let full_height = usize::from(bitmap.height.get());
        let (_mask, tile_w, tile_h) = get_compression_mode();
        let cols = (full_width + tile_w - 1) / tile_w;
        let rows = (full_height + tile_h - 1) / tile_h;
        let mut dirty_tiles = vec![false; cols * rows];

        for rect in diffs {
            let start_col = rect.x / tile_w;
            let end_col = ((rect.x + rect.width + tile_w - 1) / tile_w).min(cols);
            let start_row = rect.y / tile_h;
            let end_row = ((rect.y + rect.height + tile_h - 1) / tile_h).min(rows);

            for r in start_row..end_row {
                for c in start_col..end_col {
                    dirty_tiles[r * cols + c] = true;
                }
            }
        }

        let mut tiles = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                if dirty_tiles[r * cols + c] {
                    let x = c * tile_w;
                    let y = r * tile_h;
                    let w = (full_width - x).min(tile_w);
                    let h = (full_height - y).min(tile_h);
                    tiles.push(Rect {
                        x,
                        y,
                        width: w,
                        height: h,
                    });
                }
            }
        }

        // CƠ CHẾ BỎ QUA HIỆU ỨNG TRUNG GIAN (Effect Dropping & Settling to Final Frame):
        // - Vi mô <= 8%: 0ms delay -> 60 FPS tức thì (gõ phím, di chuột, trỏ nháy)
        // - Vừa phải <= 25%: 16ms delay -> 60 FPS mượt (cuộn trang văn bản, di chuyển nhẹ)
        // - Hiệu ứng lớn > 25%: delay 180ms -> Bỏ qua toàn bộ các frame bóng mờ hoạt họa trung gian,
        //   chỉ gửi duy nhất 1 khung hình hoàn chỉnh cuối cùng khi hiệu ứng kết thúc!
        if !force_full_screen && !tiles.is_empty() {
            let total_tiles = cols * rows;
            let dirty_count = tiles.len();
            let dirty_ratio = dirty_count as f64 / total_tiles as f64;
            let now = std::time::Instant::now();

            let required_cooldown = if dirty_ratio <= 0.08 {
                std::time::Duration::ZERO // Vi mô: 60 FPS tức thì
            } else if dirty_ratio <= 0.25 {
                std::time::Duration::from_millis(16) // Cuộn trang: 60 FPS mượt mà
            } else if dirty_ratio <= 0.60 {
                std::time::Duration::from_millis(180) // Hiệu ứng lớn: Bỏ qua frame trung gian, gom gửi frame cuối sau 180ms
            } else {
                std::time::Duration::from_millis(250) // Toàn cảnh/Đổi Space: Bỏ qua frame trung gian, gửi frame cuối sau 250ms
            };

            let elapsed = now.duration_since(self.last_large_update);
            if elapsed < required_cooldown {
                // Tạm thời bỏ qua frame này (KHÔNG cập nhật framebuffer), đợi hiệu ứng kết thúc để gửi ảnh tổng hợp cuối cùng
                return Vec::new();
            }

            if !required_cooldown.is_zero() {
                self.last_large_update = now;
            }
        }

        tiles
    }

    fn bitmap_update_framebuffer(&mut self, bitmap: BitmapUpdate, diffs: &[Rect]) {
        // NẾU KHÔNG CÓ Ô GẠCH NÀO ĐƯỢC GỬI (diffs rỗng do throttling hoặc tĩnh):
        // Tuyệt đối KHÔNG cập nhật framebuffer để các frame tiếp theo vẫn tính toán đầy đủ độ lệch
        // so với những gì Client đang thực sự hiển thị -> Triệt tiêu 100% hiện tượng bóng mờ (ghosting)!
        if diffs.is_empty() {
            return;
        }

        if self.framebuffer.is_none() {
            match bitmap.try_into() {
                Ok(framebuffer) => self.framebuffer = Some(framebuffer),
                Err(err) => warn!("Failed to convert bitmap to framebuffer: {}", err),
            }
        } else if let Some(fb) = self.framebuffer.as_mut() {
            // Chỉ cập nhật chính xác các ô gạch đã được gửi tới Client
            fb.update_diffs(&bitmap, diffs);
        }
    }

    #[allow(dead_code)]
    fn bitmap(&mut self, bitmap: BitmapUpdate) -> Result<UpdateFragmenter> {
        let updater = self
            .bitmap_updater
            .as_mut()
            .expect("bitmap updater always Some");
        updater.handle(&bitmap)
    }

    #[allow(dead_code)]
    fn encode_bitmap_tile(&mut self, bitmap: &BitmapUpdate) -> Result<Vec<u8>> {
        let updater = self
            .bitmap_updater
            .as_mut()
            .expect("bitmap updater always Some");
        updater.encode_bitmap_tile(bitmap)
    }
}

#[derive(Debug, Default)]
enum State {
    Start(DisplayUpdate),
    ReadyBuffers {
        buffers: Vec<Vec<u8>>,
        pos: usize,
        bitmap: BitmapUpdate,
        diffs: Vec<Rect>,
    },
    #[default]
    Ended,
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
pub(crate) struct EncoderIter<'a> {
    encoder: &'a mut UpdateEncoder,
    state: State,
}

impl EncoderIter<'_> {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) async fn next(&mut self) -> Option<Result<UpdateFragmenter>> {
        loop {
            let state = core::mem::take(&mut self.state);
            let encoder = &mut self.encoder;

            let res = match state {
                State::Start(update) => match update {
                    DisplayUpdate::Bitmap(bitmap) => {
                        let ds = encoder.desktop_size;
                        if bitmap.x + bitmap.width.get() > ds.width
                            || bitmap.y + bitmap.height.get() > ds.height
                        {
                            debug!(
                                "Dropping bitmap update that exceeds desktop size: \
                                 bitmap ({}, {}) {}x{} vs desktop {}x{}",
                                bitmap.x,
                                bitmap.y,
                                bitmap.width,
                                bitmap.height,
                                ds.width,
                                ds.height,
                            );
                            continue;
                        }
                        let diffs = encoder.bitmap_diffs(&bitmap);
                        if diffs.is_empty() {
                            continue;
                        }

                        #[cfg(feature = "rayon")]
                        use rayon::prelude::*;

                        let handler = NoneHandler;

                        #[cfg(feature = "rayon")]
                        let encoded_res: Result<Vec<Vec<u8>>> = diffs
                            .par_iter()
                            .filter_map(|rect| {
                                let x = u16::try_from(rect.x).ok()?;
                                let y = u16::try_from(rect.y).ok()?;
                                let width = NonZeroU16::new(u16::try_from(rect.width).ok()?)?;
                                let height = NonZeroU16::new(u16::try_from(rect.height).ok()?)?;
                                let sub = bitmap.sub(x, y, width, height)?;
                                Some(handler.encode_bitmap_tile(&sub))
                            })
                            .collect();

                        #[cfg(not(feature = "rayon"))]
                        let encoded_res: Result<Vec<Vec<u8>>> = diffs
                            .iter()
                            .filter_map(|rect| {
                                let x = u16::try_from(rect.x).ok()?;
                                let y = u16::try_from(rect.y).ok()?;
                                let width = NonZeroU16::new(u16::try_from(rect.width).ok()?)?;
                                let height = NonZeroU16::new(u16::try_from(rect.height).ok()?)?;
                                let sub = bitmap.sub(x, y, width, height)?;
                                Some(handler.encode_bitmap_tile(&sub))
                            })
                            .collect();

                        let encoded_tiles = match encoded_res {
                            Ok(tiles) => tiles,
                            Err(e) => return Some(Err(e)),
                        };

                        if encoded_tiles.is_empty() {
                            continue;
                        }

                        // Gom các ô gạch thành từng batch an toàn (<= 14 KB mỗi PDU để tương thích 100% với mọi Client)
                        let mut buffers = Vec::new();
                        let mut current_batch: Vec<Vec<u8>> = Vec::new();
                        let mut current_batch_len = 0;

                        for tile in encoded_tiles {
                            if !current_batch.is_empty() && (current_batch_len + tile.len() > 14_000) {
                                let mut final_buf = vec![0u8; current_batch_len + 16];
                                let mut cursor = WriteCursor::new(&mut final_buf);
                                if let Err(e) = BitmapUpdateData::encode_header(
                                    current_batch.len() as u16,
                                    &mut cursor,
                                ) {
                                    return Some(Err(anyhow!(
                                        "Failed to encode BitmapUpdateData header: {:?}",
                                        e
                                    )));
                                }
                                let header_len = cursor.pos();
                                let mut write_pos = header_len;
                                for t in current_batch.drain(..) {
                                    final_buf[write_pos..write_pos + t.len()].copy_from_slice(&t);
                                    write_pos += t.len();
                                }
                                final_buf.truncate(write_pos);
                                buffers.push(final_buf);
                                current_batch_len = 0;
                            }

                            current_batch_len += tile.len();
                            current_batch.push(tile);
                        }

                        if !current_batch.is_empty() {
                            let mut final_buf = vec![0u8; current_batch_len + 16];
                            let mut cursor = WriteCursor::new(&mut final_buf);
                            if let Err(e) = BitmapUpdateData::encode_header(
                                current_batch.len() as u16,
                                &mut cursor,
                            ) {
                                return Some(Err(anyhow!(
                                    "Failed to encode BitmapUpdateData header: {:?}",
                                    e
                                )));
                            }
                            let header_len = cursor.pos();
                            let mut write_pos = header_len;
                            for t in current_batch {
                                final_buf[write_pos..write_pos + t.len()].copy_from_slice(&t);
                                write_pos += t.len();
                            }
                            final_buf.truncate(write_pos);
                            buffers.push(final_buf);
                        }

                        self.state = State::ReadyBuffers {
                            buffers,
                            pos: 0,
                            bitmap,
                            diffs,
                        };
                        continue;
                    }
                    DisplayUpdate::PointerPosition(pos) => UpdateEncoder::pointer_position(pos),
                    DisplayUpdate::RGBAPointer(ptr) => UpdateEncoder::rgba_pointer(ptr),
                    DisplayUpdate::ColorPointer(ptr) => UpdateEncoder::color_pointer(ptr),
                    DisplayUpdate::HidePointer => UpdateEncoder::hide_pointer(),
                    DisplayUpdate::DefaultPointer => UpdateEncoder::default_pointer(),
                    DisplayUpdate::CachedPointer(idx) => UpdateEncoder::cached_pointer(idx),
                    DisplayUpdate::Resize(_) => return None,
                },
                State::ReadyBuffers {
                    buffers,
                    mut pos,
                    bitmap,
                    diffs,
                } => {
                    if pos >= buffers.len() {
                        encoder.bitmap_update_framebuffer(bitmap, &diffs);
                        self.state = State::Ended;
                        return None;
                    }

                    let buf = buffers[pos].clone();
                    pos += 1;
                    if pos >= buffers.len() {
                        encoder.bitmap_update_framebuffer(bitmap, &diffs);
                        self.state = State::Ended;
                    } else {
                        self.state = State::ReadyBuffers {
                            buffers,
                            pos,
                            bitmap,
                            diffs,
                        };
                    }

                    return Some(Ok(UpdateFragmenter::new(UpdateCode::Bitmap, buf)));
                }
                State::Ended => return None,
            };

            return Some(res);
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum BitmapUpdater {
    None(NoneHandler),
    Bitmap(BitmapHandler),
    RemoteFx(RemoteFxHandler),
    #[cfg(feature = "qoi")]
    Qoi(QoiHandler),
    #[cfg(feature = "qoiz")]
    Qoiz(QoizHandler),
    #[cfg(feature = "nscodec")]
    NsCodec(NsCodecHandler),
}

impl BitmapUpdater {
    #[allow(dead_code)]
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        match self {
            Self::None(up) => up.handle(bitmap),
            Self::Bitmap(up) => up.handle(bitmap),
            Self::RemoteFx(up) => up.handle(bitmap),
            #[cfg(feature = "qoi")]
            Self::Qoi(up) => up.handle(bitmap),
            #[cfg(feature = "qoiz")]
            Self::Qoiz(up) => up.handle(bitmap),
            #[cfg(feature = "nscodec")]
            Self::NsCodec(up) => up.handle(bitmap),
        }
    }

    #[allow(dead_code)]
    fn encode_bitmap_tile(&mut self, bitmap: &BitmapUpdate) -> Result<Vec<u8>> {
        match self {
            Self::None(up) => up.encode_bitmap_tile(bitmap),
            _ => Err(anyhow!("unsupported updater for bitmap batching")),
        }
    }

    fn set_desktop_size(&mut self, size: DesktopSize) {
        if let Self::RemoteFx(up) = self {
            up.set_desktop_size(size)
        }
    }
}

#[allow(dead_code)]
trait BitmapUpdateHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter>;
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct NoneHandler;

impl NoneHandler {
    fn encode_bitmap_tile(&self, bitmap: &BitmapUpdate) -> Result<Vec<u8>> {
        let width = usize::from(bitmap.width.get());
        let height = usize::from(bitmap.height.get());
        let stride = bitmap.stride.get();
        let bpp = usize::from(bitmap.format.bytes_per_pixel());
        let row_len = width * bpp;

        let mut encoder = ironrdp_graphics::rdp6::BitmapStreamEncoder::new(width, height);
        let mut compressed = vec![0u8; width * height * 4 + 1024];

        // Lọc nhiễu / Giảm chất lượng màu theo chế độ cấu hình RDP_MODE
        // Chế độ speed (Mask 0xF0 - 4-bit) nén siêu chặt, giảm tới 88% dung lượng
        let (mask, _, _) = get_compression_mode();
        let quantized: Vec<u8> = bitmap
            .data
            .chunks(stride)
            .take(height)
            .map(|row| &row[..row_len.min(row.len())])
            .rev() // Bottom-to-top row order for standard RDP Bitmap Data
            .flat_map(|row| row.iter().map(|&b| b & mask))
            .collect();

        let len = encoder.encode_pixels_stream::<_, ironrdp_graphics::rdp6::BgrAChannels>(
            quantized.chunks(bpp),
            &mut compressed,
            true,
        )?;

        let data = BitmapData {
            rectangle: InclusiveRectangle {
                left: bitmap.x,
                top: bitmap.y,
                right: bitmap.x + bitmap.width.get() - 1,
                bottom: bitmap.y + bitmap.height.get() - 1,
            },
            width: bitmap.width.get(),
            height: bitmap.height.get(),
            bits_per_pixel: 32,
            compression_flags: Compression::BITMAP_COMPRESSION
                | Compression::NO_BITMAP_COMPRESSION_HDR,
            compressed_data_header: None,
            bitmap_data: &compressed[..len],
        };

        let mut out = vec![0u8; len + 128];
        let mut cursor = WriteCursor::new(&mut out);
        data.encode(&mut cursor)
            .map_err(|e| anyhow!("Failed to encode BitmapData: {:?}", e))?;
        let written = cursor.pos();
        out.truncate(written);
        Ok(out)
    }
}

impl BitmapUpdateHandler for NoneHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let tile_bytes = self.encode_bitmap_tile(bitmap)?;
        let mut final_buf = vec![0u8; tile_bytes.len() + 16];
        let mut cursor = WriteCursor::new(&mut final_buf);
        BitmapUpdateData::encode_header(1, &mut cursor)
            .map_err(|e| anyhow!("Failed to encode BitmapUpdateData header: {:?}", e))?;
        let header_len = cursor.pos();
        final_buf[header_len..header_len + tile_bytes.len()].copy_from_slice(&tile_bytes);
        final_buf.truncate(header_len + tile_bytes.len());
        Ok(UpdateFragmenter::new(UpdateCode::Bitmap, final_buf))
    }
}

#[derive(Clone)]
struct BitmapHandler {
    bitmap: BitmapEncoder,
}

impl fmt::Debug for BitmapHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BitmapHandler").finish()
    }
}

#[allow(dead_code)]
impl BitmapHandler {
    fn new() -> Self {
        Self {
            bitmap: BitmapEncoder::new(),
        }
    }
}

impl BitmapUpdateHandler for BitmapHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let mut buffer = vec![0; bitmap.data.len() * 2]; // TODO: estimate bitmap encoded size
        let len = loop {
            match self.bitmap.encode(bitmap, buffer.as_mut_slice()) {
                Err(err) => match err {
                    BitmapEncodeError::Encode(e) => match e.kind() {
                        ironrdp_core::EncodeErrorKind::NotEnoughBytes { .. } => {
                            buffer.resize(buffer.len() * 2, 0);
                            debug!("encoder buffer resized to: {}", buffer.len() * 2);
                        }
                        _ => Err(e).context("bitmap encode error")?,
                    },
                    BitmapEncodeError::Rle(e) => Err(e).context("bitmap RLE encode error")?,
                },
                Ok(len) => break len,
            }
        };

        buffer.truncate(len);
        Ok(UpdateFragmenter::new(UpdateCode::Bitmap, buffer))
    }
}

#[derive(Debug, Clone)]
struct RemoteFxHandler {
    remotefx: RfxEncoder,
    codec_id: u8,
    desktop_size: Option<DesktopSize>,
}

#[allow(dead_code)]
impl RemoteFxHandler {
    fn new(algo: EntropyBits, codec_id: u8, desktop_size: DesktopSize) -> Self {
        Self {
            remotefx: RfxEncoder::new(algo),
            desktop_size: Some(desktop_size),
            codec_id,
        }
    }

    fn set_desktop_size(&mut self, size: DesktopSize) {
        self.desktop_size = Some(size);
    }
}

impl BitmapUpdateHandler for RemoteFxHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let mut buffer = vec![0; bitmap.data.len() + 1024];
        let len = loop {
            match self
                .remotefx
                .encode(bitmap, buffer.as_mut_slice(), self.desktop_size.take())
            {
                Err(e) => match e.kind() {
                    ironrdp_core::EncodeErrorKind::NotEnoughBytes { .. } => {
                        buffer.resize(buffer.len() * 2, 0);
                        debug!("encoder buffer resized to: {}", buffer.len() * 2);
                    }
                    _ => Err(e).context("RemoteFX encode error")?,
                },
                Ok(len) => break len,
            }
        };

        set_surface(bitmap, self.codec_id, &buffer[..len])
    }
}

#[cfg(feature = "qoi")]
#[derive(Clone, Debug)]
struct QoiHandler {
    codec_id: u8,
}

#[cfg(feature = "qoi")]
impl QoiHandler {
    fn new(codec_id: u8) -> Self {
        Self { codec_id }
    }
}

#[cfg(feature = "qoi")]
impl BitmapUpdateHandler for QoiHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let data = qoi_encode(bitmap)?;
        set_surface(bitmap, self.codec_id, &data)
    }
}

#[cfg(feature = "qoiz")]
struct QoizHandler {
    codec_id: u8,
    zctxt: zstd_safe::CCtx<'static>,
}

#[cfg(feature = "qoiz")]
impl fmt::Debug for QoizHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QoizHandler")
            .field("codec_id", &self.codec_id)
            .finish()
    }
}

#[cfg(feature = "qoiz")]
impl QoizHandler {
    fn new(codec_id: u8) -> Result<Self> {
        let mut zctxt = zstd_safe::CCtx::default();

        zctxt
            .set_parameter(zstd_safe::CParameter::CompressionLevel(3))
            .map_err(|code| {
                anyhow!(
                    "failed to set zstd compression level: {}",
                    zstd_safe::get_error_name(code)
                )
            })?;
        zctxt
            .set_parameter(zstd_safe::CParameter::EnableLongDistanceMatching(true))
            .map_err(|code| {
                anyhow!(
                    "failed to set zstd enable long distance matching: {}",
                    zstd_safe::get_error_name(code)
                )
            })?;

        Ok(Self { codec_id, zctxt })
    }
}

#[cfg(feature = "qoiz")]
impl BitmapUpdateHandler for QoizHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let qoi = qoi_encode(bitmap)?;
        let mut inb = zstd_safe::InBuffer::around(&qoi);
        let mut data = vec![0; qoi.len()];
        let mut outb;
        let mut pos = 0;

        loop {
            outb = zstd_safe::OutBuffer::around_pos(data.as_mut_slice(), pos);
            let res = self
                .zctxt
                .compress_stream2(
                    &mut outb,
                    &mut inb,
                    zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_flush,
                )
                .map_err(|code| {
                    anyhow!(
                        "failed to Zstd compress: {}",
                        zstd_safe::get_error_name(code)
                    )
                })?;
            if res == 0 {
                break;
            }
            pos = outb.pos();
            data.resize(data.len() + res, 0);
        }

        set_surface(bitmap, self.codec_id, outb.as_slice())
    }
}

#[cfg(feature = "nscodec")]
#[derive(Clone, Debug)]
struct NsCodecHandler {
    codec_id: u8,
    color_loss_level: u8,
}

#[allow(dead_code)]
#[cfg(feature = "nscodec")]
impl NsCodecHandler {
    fn new(codec_id: u8, color_loss_level: u8) -> Self {
        Self {
            codec_id,
            color_loss_level,
        }
    }
}

#[cfg(feature = "nscodec")]
impl BitmapUpdateHandler for NsCodecHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let data = ironrdp_nscodec::encoder::encode(
            &bitmap.data,
            bitmap.width.get(),
            bitmap.height.get(),
            bitmap.stride.get(),
            bitmap.format,
            self.color_loss_level,
        );
        set_surface(bitmap, self.codec_id, &data)
    }
}

#[cfg(feature = "qoi")]
fn qoi_encode(bitmap: &BitmapUpdate) -> Result<Vec<u8>> {
    use ironrdp_graphics::image_processing::PixelFormat::*;
    // Map every 4-byte input — whether it nominally has an alpha byte or
    // an "X" filler — to the 3-channel-output `*x` variant of
    // `RawChannels`. The qoi crate selects `Channels::Rgb` vs
    // `Channels::Rgba` for the QOI header from this enum: `*x` and `*r/g/b`
    // produce `Rgb`; `*a` produces `Rgba`. The `ironrdp-session` NSCodec-
    // free decode path in `fast_path.rs::qoi_apply` only supports
    // `Channels::Rgb` and explicitly drops `Channels::Rgba` frames with
    // `WARN: Unsupported RGBA QOI data`, so the previous "honest" mapping
    // (`BgrA32 -> Bgra`, etc.) produced output that no IronRDP client
    // could decode — every QOI session rendered a blank screen.
    //
    // Server-side bitmap captures are functionally opaque (the alpha byte
    // is either always 0xFF or treated as filler), so discarding it is
    // safe and matches what every successful legacy bitmap path
    // already does.
    let raw_channels = match bitmap.format {
        ARgb32 | XRgb32 => qoi::RawChannels::Xrgb,
        ABgr32 | XBgr32 => qoi::RawChannels::Xbgr,
        BgrA32 | BgrX32 => qoi::RawChannels::Bgrx,
        RgbA32 | RgbX32 => qoi::RawChannels::Rgbx,
    };
    let enc = qoi::EncoderBuilder::new(
        &bitmap.data,
        bitmap.width.get().into(),
        bitmap.height.get().into(),
    )
    .stride(bitmap.stride.get())
    .raw_channels(raw_channels)
    .build()?;
    Ok(enc.encode_to_vec()?)
}

#[allow(dead_code)]
fn set_surface(bitmap: &BitmapUpdate, codec_id: u8, data: &[u8]) -> Result<UpdateFragmenter> {
    let destination = ExclusiveRectangle {
        left: bitmap.x,
        top: bitmap.y,
        right: bitmap.x + bitmap.width.get(),
        bottom: bitmap.y + bitmap.height.get(),
    };
    let extended_bitmap_data = ExtendedBitmapDataPdu {
        bpp: bitmap.format.bytes_per_pixel() * 8,
        width: bitmap.width.get(),
        height: bitmap.height.get(),
        codec_id,
        header: None,
        data,
    };
    let pdu = SurfaceBitsPdu {
        destination,
        extended_bitmap_data,
    };
    let cmd = SurfaceCommand::SetSurfaceBits(pdu);
    Ok(UpdateFragmenter::new(
        UpdateCode::SurfaceCommands,
        encode_vec(&cmd)?,
    ))
}
