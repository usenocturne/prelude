use std::collections::BTreeMap;

use crate::{
    Error, Result,
    constants::{
        FLAG_ENCRYPTED, FLAG_FIRST, FLAG_LAST, MAX_BUFFERED_WIRE, MAX_PHYSICAL_PAYLOAD,
        MAX_REASSEMBLED_PAYLOAD,
    },
};

const SHORT_HEADER: usize = 4;
const EXTENDED_HEADER: usize = 8;
const MAX_ACTIVE_REASSEMBLIES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalFrame {
    pub channel_id: u8,
    pub flags: u8,
    pub payload: Vec<u8>,
    pub announced_total: Option<usize>,
}

impl PhysicalFrame {
    pub const fn is_first(&self) -> bool {
        self.flags & FLAG_FIRST != 0
    }

    pub const fn is_last(&self) -> bool {
        self.flags & FLAG_LAST != 0
    }
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffered: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<PhysicalFrame>> {
        if chunk.len() > MAX_BUFFERED_WIRE {
            return Err(Error::Oversized {
                kind: "wire chunk",
                actual: chunk.len(),
                limit: MAX_BUFFERED_WIRE,
            });
        }
        if self.buffered.len().saturating_add(chunk.len()) > MAX_BUFFERED_WIRE {
            return Err(Error::Oversized {
                kind: "buffered wire data",
                actual: self.buffered.len().saturating_add(chunk.len()),
                limit: MAX_BUFFERED_WIRE,
            });
        }
        self.buffered.extend_from_slice(chunk);

        let mut frames = Vec::new();
        let mut consumed = 0;
        while self.buffered.len().saturating_sub(consumed) >= SHORT_HEADER {
            let header = &self.buffered[consumed..];
            let flags = header[1];
            let first = flags & FLAG_FIRST != 0;
            let last = flags & FLAG_LAST != 0;
            let header_len = if first && !last {
                EXTENDED_HEADER
            } else {
                SHORT_HEADER
            };
            if header.len() < header_len {
                break;
            }
            let payload_len = usize::from(u16::from_be_bytes([header[2], header[3]]));
            if payload_len > MAX_PHYSICAL_PAYLOAD {
                return Err(Error::Oversized {
                    kind: "physical frame",
                    actual: payload_len,
                    limit: MAX_PHYSICAL_PAYLOAD,
                });
            }
            let frame_len = header_len
                .checked_add(payload_len)
                .ok_or(Error::InvalidFrame("frame length overflow"))?;
            if header.len() < frame_len {
                break;
            }
            let announced_total = if header_len == EXTENDED_HEADER {
                let total = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                let total = usize::try_from(total)
                    .map_err(|_| Error::InvalidFrame("announced total does not fit usize"))?;
                if flags & FLAG_ENCRYPTED == 0 && total < payload_len {
                    return Err(Error::InvalidFrame(
                        "first fragment exceeds its announced total",
                    ));
                }
                if total > MAX_REASSEMBLED_PAYLOAD {
                    return Err(Error::Oversized {
                        kind: "reassembled message",
                        actual: total,
                        limit: MAX_REASSEMBLED_PAYLOAD,
                    });
                }
                Some(total)
            } else {
                None
            };
            frames.push(PhysicalFrame {
                channel_id: header[0],
                flags,
                payload: header[header_len..frame_len].to_vec(),
                announced_total,
            });
            consumed = consumed
                .checked_add(frame_len)
                .ok_or(Error::InvalidFrame("consumed length overflow"))?;
        }

        if consumed != 0 {
            self.buffered.drain(..consumed);
        }
        Ok(frames)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }
}

#[derive(Debug)]
struct Reassembly {
    flags: u8,
    expected: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub(crate) struct MessageAssembler {
    channels: BTreeMap<u8, Reassembly>,
}

impl MessageAssembler {
    /// Whether no fragmented message is waiting for its remaining fragments.
    pub(crate) fn is_idle(&self) -> bool {
        self.channels.is_empty()
    }

    pub(crate) fn push(
        &mut self,
        channel_id: u8,
        flags: u8,
        announced_total: Option<usize>,
        payload: &[u8],
    ) -> Result<Option<(u8, Vec<u8>)>> {
        let first = flags & FLAG_FIRST != 0;
        let last = flags & FLAG_LAST != 0;

        if first && last {
            if announced_total.is_some() {
                return Err(Error::InvalidFrame(
                    "single-frame message has an extended total",
                ));
            }
            if payload.len() > MAX_REASSEMBLED_PAYLOAD {
                return Err(Error::Oversized {
                    kind: "single-frame message",
                    actual: payload.len(),
                    limit: MAX_REASSEMBLED_PAYLOAD,
                });
            }
            return Ok(Some((flags, payload.to_vec())));
        }

        if first {
            let expected = announced_total.ok_or(Error::InvalidFrame(
                "first fragment is missing its announced total",
            ))?;
            if self.channels.contains_key(&channel_id) {
                return Err(Error::InvalidFrame(
                    "new first fragment overlaps an active message",
                ));
            }
            if self.channels.len() >= MAX_ACTIVE_REASSEMBLIES {
                return Err(Error::InvalidFrame("too many active fragmented messages"));
            }
            self.channels.insert(
                channel_id,
                Reassembly {
                    flags,
                    expected,
                    bytes: payload.to_vec(),
                },
            );
            return Ok(None);
        }

        let state = self
            .channels
            .get_mut(&channel_id)
            .ok_or(Error::InvalidFrame("continuation without first fragment"))?;
        let new_len = state
            .bytes
            .len()
            .checked_add(payload.len())
            .ok_or(Error::InvalidFrame("fragmented message length overflow"))?;
        if new_len > state.expected || new_len > MAX_REASSEMBLED_PAYLOAD {
            self.channels.remove(&channel_id);
            return Err(Error::InvalidFrame(
                "fragmented message exceeds its announced total",
            ));
        }
        state.bytes.extend_from_slice(payload);
        if !last {
            return Ok(None);
        }
        let state = self
            .channels
            .remove(&channel_id)
            .ok_or(Error::InvalidFrame("fragment state disappeared"))?;
        if state.bytes.len() != state.expected {
            return Err(Error::InvalidFrame(
                "fragmented message does not match its announced total",
            ));
        }
        Ok(Some((state.flags, state.bytes)))
    }
}

pub(crate) fn encode_plain_frame(
    channel_id: u8,
    flags: u8,
    message_id: u16,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let payload_len = payload
        .len()
        .checked_add(2)
        .ok_or(Error::InvalidFrame("plain frame length overflow"))?;
    let payload_len_u16 = u16::try_from(payload_len).map_err(|_| Error::Oversized {
        kind: "plain frame",
        actual: payload_len,
        limit: MAX_PHYSICAL_PAYLOAD,
    })?;
    let mut frame = Vec::with_capacity(SHORT_HEADER + payload_len);
    frame.push(channel_id);
    frame.push(flags);
    frame.extend_from_slice(&payload_len_u16.to_be_bytes());
    frame.extend_from_slice(&message_id.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub(crate) fn encode_encrypted_frame(
    channel_id: u8,
    flags: u8,
    tls_record: &[u8],
    announced_total: Option<usize>,
) -> Result<Vec<u8>> {
    let payload_len = u16::try_from(tls_record.len()).map_err(|_| Error::Oversized {
        kind: "TLS record",
        actual: tls_record.len(),
        limit: MAX_PHYSICAL_PAYLOAD,
    })?;
    let extended = flags & FLAG_FIRST != 0 && flags & FLAG_LAST == 0;
    if extended != announced_total.is_some() {
        return Err(Error::InvalidFrame("encrypted frame header form mismatch"));
    }
    let mut frame = Vec::with_capacity(
        if extended {
            EXTENDED_HEADER
        } else {
            SHORT_HEADER
        } + tls_record.len(),
    );
    frame.push(channel_id);
    frame.push(flags);
    frame.extend_from_slice(&payload_len.to_be_bytes());
    if let Some(total) = announced_total {
        let total = u32::try_from(total).map_err(|_| Error::Oversized {
            kind: "announced cleartext",
            actual: total,
            limit: u32::MAX as usize,
        })?;
        frame.extend_from_slice(&total.to_be_bytes());
    }
    frame.extend_from_slice(tls_record);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const BULK: u8 = FLAG_FIRST | FLAG_LAST;

    #[test]
    fn livi_single_frame_golden_round_trips_incrementally() {
        let encoded = encode_plain_frame(3, BULK, 0x1234, &[0xaa, 0xbb]).expect("frame");
        assert_eq!(encoded, [3, 3, 0, 4, 0x12, 0x34, 0xaa, 0xbb]);
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&encoded[..3]).expect("prefix").is_empty());
        let frames = decoder.push(&encoded[3..]).expect("suffix");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, [0x12, 0x34, 0xaa, 0xbb]);
    }

    #[test]
    fn multiple_frames_in_one_read_are_preserved() {
        let a = encode_plain_frame(1, BULK, 1, &[1]).expect("a");
        let b = encode_plain_frame(2, BULK, 2, &[2, 2]).expect("b");
        let frames = FrameDecoder::new().push(&[a, b].concat()).expect("frames");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].channel_id, 2);
    }

    #[test]
    fn coalesced_wire_reads_exceeding_single_physical_frame_are_decoded() {
        let a = encode_plain_frame(3, BULK, 1, &vec![0x11; 40_000]).expect("a");
        let b = encode_plain_frame(3, BULK, 2, &vec![0x22; 30_000]).expect("b");
        let combined = [a, b].concat();
        assert!(combined.len() > crate::constants::MAX_PHYSICAL_FRAME);
        let frames = FrameDecoder::new().push(&combined).expect("frames");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload.len(), 40_002);
        assert_eq!(frames[1].payload.len(), 30_002);
    }

    #[test]
    fn strict_reassembly_requires_exact_total() {
        let mut assembler = MessageAssembler::default();
        assert!(
            assembler
                .push(5, FLAG_FIRST, Some(6), &[0, 1, 2])
                .expect("first")
                .is_none()
        );
        let complete = assembler
            .push(5, FLAG_LAST, None, &[3, 4, 5])
            .expect("last")
            .expect("complete");
        assert_eq!(complete.1, [0, 1, 2, 3, 4, 5]);

        assert!(assembler.push(5, FLAG_FIRST, Some(7), &[0, 1, 2]).is_ok());
        assert!(assembler.push(5, FLAG_LAST, None, &[3, 4, 5]).is_err());
    }

    #[test]
    fn continuation_without_first_fails_closed() {
        assert!(
            MessageAssembler::default()
                .push(1, FLAG_LAST, None, &[0, 0])
                .is_err()
        );
    }

    #[test]
    fn oversized_wire_chunk_is_rejected_before_buffering() {
        let input = vec![0_u8; MAX_BUFFERED_WIRE + 1];
        let mut decoder = FrameDecoder::new();
        assert!(matches!(
            decoder.push(&input),
            Err(Error::Oversized {
                kind: "wire chunk",
                ..
            })
        ));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn encrypted_first_total_is_cleartext_not_tls_record_length() {
        let tls_record = vec![0xa5; 16 * 1024 + 29];
        let cleartext_total = 16 * 1024 + 1;
        let encoded = encode_encrypted_frame(
            3,
            crate::constants::FLAG_ENCRYPTED_SPECIFIC_BASE | crate::constants::FRAME_TYPE_FIRST,
            &tls_record,
            Some(cleartext_total),
        )
        .expect("encrypted first");
        let frames = FrameDecoder::new().push(&encoded).expect("decode");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].announced_total, Some(cleartext_total));
        assert!(frames[0].payload.len() > cleartext_total);
    }

    proptest! {
        #[test]
        fn arbitrary_input_never_panics(input in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = FrameDecoder::new().push(&input);
        }

        #[test]
        fn plain_frame_round_trip(channel in any::<u8>(), msg in any::<u16>(), payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let encoded = encode_plain_frame(channel, BULK, msg, &payload).expect("encode");
            let frames = FrameDecoder::new().push(&encoded).expect("decode");
            prop_assert_eq!(frames.len(), 1);
            prop_assert_eq!(frames[0].channel_id, channel);
            prop_assert_eq!(&frames[0].payload[..2], &msg.to_be_bytes());
            prop_assert_eq!(&frames[0].payload[2..], payload.as_slice());
        }
    }
}
