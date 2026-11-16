//! 804B IMA ADPCM 块与序号跟踪。
//!
//! 块结构：`[int16 LE predictor][uint8 step_index][uint8 保留] + 800B 4bit 数据`。
//! 块头的 predictor **就是该块的第 0 个输出样本**，数据区只承载第 1..1599 个样本
//! （1599 个 nibble，低半字节先）。因此解码时用现有 `ImaAdpcmDecoder` 解出前
//! 1599 个结果，再补上块头样本——直接整块解码会多出第 1600 个样本并整体错位。

use crate::adpcm::{ImaAdpcmDecoder, NibbleOrder};

pub const BLOCK_HEADER_BYTES: usize = 4;
pub const BLOCK_SAMPLES: usize = 1600;
pub const BLOCK_SAMPLES_PER_SECOND: u32 = 16_000;
/// 4B 块头 + 1600 样本 / 2（4bit/样本）
pub const BLOCK_BYTES: usize = BLOCK_HEADER_BYTES + BLOCK_SAMPLES / 2; // 804
/// AUDIO 载荷 = `[seq][0x80]` + 804B 块；整包 807B（含 1B 类型）
pub const AUDIO_PAYLOAD_BYTES: usize = 2 + BLOCK_BYTES; // 806

/// 一个 AUDIO 报文里的单块。UDP 下恒为"单片 + 末片"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioBlock<'a> {
    pub seq: u8,
    pub block: &'a [u8],
}

/// 解析 AUDIO 载荷（不含类型字节）。
pub fn parse_audio(payload: &[u8]) -> Option<AudioBlock<'_>> {
    if payload.len() != AUDIO_PAYLOAD_BYTES {
        return None;
    }
    // [seq][0x80] 是 BLE 时代遗留的分片头：UDP 下 0x80 恒置位。
    if payload[1] & 0x80 == 0 {
        return None;
    }
    Some(AudioBlock {
        seq: payload[0],
        block: &payload[2..],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    BadLength,
    BadStepIndex,
}

#[derive(Debug, Clone)]
pub struct BlockDecoder {
    inner: ImaAdpcmDecoder,
}

impl Default for BlockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockDecoder {
    pub fn new() -> Self {
        Self {
            inner: ImaAdpcmDecoder::new(NibbleOrder::LowFirst),
        }
    }

    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<i16>, BlockError> {
        if block.len() != BLOCK_BYTES {
            return Err(BlockError::BadLength);
        }
        let first = i16::from_le_bytes([block[0], block[1]]);
        let step_index = block[2];
        if step_index > 88 {
            return Err(BlockError::BadStepIndex);
        }
        self.inner.reset(i32::from(first), i32::from(step_index));
        let decoded = self.inner.decode(&block[BLOCK_HEADER_BYTES..]);
        if decoded.len() < BLOCK_SAMPLES - 1 {
            return Err(BlockError::BadLength);
        }
        let mut samples = Vec::with_capacity(BLOCK_SAMPLES);
        samples.push(first);
        samples.extend_from_slice(&decoded[..BLOCK_SAMPLES - 1]);
        Ok(samples)
    }
}

/// 与参考实现同口径：`0` 或 `>=128` 视为重复/乱序；间隙计入丢块。
#[derive(Debug, Clone, Default)]
pub struct SequenceTracker {
    last: Option<u8>,
    lost: u64,
    out_of_order: u64,
}

impl SequenceTracker {
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// 返回 false 表示该包应丢弃（重复或迟到）。
    pub fn accept(&mut self, seq: u8) -> bool {
        if let Some(previous) = self.last {
            let gap = seq.wrapping_sub(previous);
            if gap == 0 || gap >= 128 {
                self.out_of_order += 1;
                return false;
            }
            if gap > 1 {
                self.lost += u64::from(gap - 1);
            }
        }
        self.last = Some(seq);
        true
    }

    pub fn lost(&self) -> u64 {
        self.lost
    }

    pub fn out_of_order(&self) -> u64 {
        self.out_of_order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 夹具由 FoloToy `adpcm.encode_block` 生成（合成信号，非用户语音）。
    const BLOCK: &[u8] = include_bytes!("../../tests/fixtures/passport/block.bin");
    const EXPECTED_PCM: &[u8] = include_bytes!("../../tests/fixtures/passport/expected.pcm");

    #[test]
    fn payload_requires_exact_length_and_last_fragment_flag() {
        let mut payload = vec![0u8; AUDIO_PAYLOAD_BYTES];
        payload[1] = 0x80;
        assert!(parse_audio(&payload).is_some());
        assert!(parse_audio(&payload[..AUDIO_PAYLOAD_BYTES - 1]).is_none());
        payload[1] = 0x00;
        assert!(parse_audio(&payload).is_none());
    }

    #[test]
    fn decodes_reference_block_bit_exact() {
        let mut decoder = BlockDecoder::new();
        let samples = decoder.decode(BLOCK).unwrap();
        assert_eq!(samples.len(), BLOCK_SAMPLES);
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for sample in &samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        assert_eq!(pcm, EXPECTED_PCM);
    }

    #[test]
    fn rejects_wrong_length_and_out_of_range_step_index() {
        let mut decoder = BlockDecoder::new();
        assert_eq!(decoder.decode(&BLOCK[..100]), Err(BlockError::BadLength));
        let mut broken = BLOCK.to_vec();
        broken[2] = 200;
        assert_eq!(decoder.decode(&broken), Err(BlockError::BadStepIndex));
    }

    #[test]
    fn sequence_tracker_counts_loss_and_drops_late_packets() {
        let mut tracker = SequenceTracker::default();
        assert!(tracker.accept(0));
        assert!(tracker.accept(1));
        assert!(tracker.accept(4)); // 丢 2、3
        assert_eq!(tracker.lost(), 2);
        assert!(!tracker.accept(4)); // 重复
        assert!(!tracker.accept(2)); // 迟到
        assert_eq!(tracker.out_of_order(), 2);
        assert!(tracker.accept(5));
    }
}
