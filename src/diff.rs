// Dirty-rect display flushing: compare the freshly drawn frame against what
// the panel currently shows and push only the changed horizontal runs.
// From the assistant (same panel, same driver); the technique is generic.

use lcd_async::{interface::Interface, models::GC9A01, Display};

use crate::pins::{DISPLAY_HEIGHT, DISPLAY_WIDTH};

const W: usize = DISPLAY_WIDTH as usize;
const H: usize = DISPLAY_HEIGHT as usize;

/// Bytes in one full frame: 240 * 240 * 2 (Rgb565).
pub const FRAME_BYTES: usize = W * H * 2;

/// Changed-byte count at which run flushing loses to one full transfer.
const FULL_FLUSH_THRESHOLD: usize = FRAME_BYTES / 3;

pub async fn flush_changed<DI, RST>(
    display: &mut Display<DI, GC9A01, RST>,
    prev: &mut [u8],
    frame: &mut [u8],
) where
    DI: Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    if prev.len() != FRAME_BYTES || frame.len() != FRAME_BYTES {
        display
            .show_raw_data(0, 0, W as u16, H as u16, frame)
            .await
            .ok();
        return;
    }

    let row_words = W * 2 / 4;
    debug_assert_eq!(row_words * 4, W * 2);

    let mut changed_bytes = 0usize;
    for y in 0..H {
        let row_off = y * W * 2;
        let frow: &[u8] = &frame[row_off..row_off + W * 2];
        let prow: &[u8] = &prev[row_off..row_off + W * 2];
        for wi in 0..row_words {
            if frow[wi * 4..wi * 4 + 4] != prow[wi * 4..wi * 4 + 4] {
                changed_bytes += 4;
            }
        }
    }

    if changed_bytes == 0 {
        return;
    }

    if changed_bytes >= FULL_FLUSH_THRESHOLD {
        display
            .show_raw_data(0, 0, W as u16, H as u16, frame)
            .await
            .ok();
        prev.copy_from_slice(frame);
        return;
    }

    for y in 0..H {
        let row_off = y * W * 2;
        let frow: &[u8] = &frame[row_off..row_off + W * 2];
        let prow: &[u8] = &prev[row_off..row_off + W * 2];

        let mut run_start: Option<usize> = None;
        for wi in 0..row_words {
            let differs = frow[wi * 4..wi * 4 + 4] != prow[wi * 4..wi * 4 + 4];
            match (differs, run_start) {
                (true, _) if run_start.is_none() => run_start = Some(wi),
                (false, Some(start)) => {
                    emit_run(display, frame, y, start, wi).await;
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(start) = run_start {
            emit_run(display, frame, y, start, row_words).await;
        }
    }

    prev.copy_from_slice(frame);
}

async fn emit_run<DI, RST>(
    display: &mut Display<DI, GC9A01, RST>,
    frame: &[u8],
    y: usize,
    start: usize,
    end: usize,
) where
    DI: Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    let x0 = start * 2;
    let width = (end - start) * 2;
    let byte0 = y * W * 2 + x0 * 2;
    let byte1 = byte0 + width * 2;
    display
        .show_raw_data(x0 as u16, y as u16, width as u16, 1, &frame[byte0..byte1])
        .await
        .ok();
}
