//! Pictures that push a frame out of the Amlogic H.264 decoder.
//!
//! The decoder returns a picture only after `max_num_ref_frames + 2` later
//! pictures have entered it, so a frame stays inside until the phone has sent
//! that many more. A P picture made only of skipped macroblocks repeats the
//! previous picture exactly, and one that is not a reference changes nothing
//! later frames decode from. Following each frame with enough of them makes
//! the decoder return the frame straight away.
//!
//! Only the stream shape this was measured with is handled: Baseline, one
//! reference frame, picture order count type 2, frame macroblocks only,
//! CAVLC, one slice group and no weighted prediction. Any other stream is
//! passed through untouched.

#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-backend")),
    allow(dead_code)
)]

use std::collections::VecDeque;

use crate::{EncryptedVideoFrame, VideoCodecConfiguration};

/// Pictures the decoder keeps back beyond the stream's reference frames.
const HELD_BEYOND_REFERENCE_FRAMES: u32 = 2;
const BASELINE_PROFILE: u8 = 66;
const MAXIMUM_MACROBLOCKS: u32 = 36_864;
/// Enough of a slice to cover its header up to `frame_num`.
const SLICE_HEADER_PREFIX_BYTES: usize = 24;

/// The pictures to send to the decoder after one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlushPictures {
    /// One skipped picture, framed like the stream's own NAL units.
    pub(crate) picture: Vec<u8>,
    /// How many copies release the frame.
    pub(crate) count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SequenceParameters {
    id: u32,
    frame_num_bits: u32,
    reference_frames: u32,
    macroblocks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PictureParameters {
    id: u32,
    sequence_id: u32,
    deblocking_control: bool,
    redundant_picture_count: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceStart {
    picture_parameters_id: u32,
    frame_num: u32,
    reference: bool,
}

/// Builds the follower pictures for each frame of one stream.
#[derive(Debug, Default)]
pub(crate) struct DecoderFlush {
    nal_length_bytes: usize,
    sequence: Option<SequenceParameters>,
    picture: Option<PictureParameters>,
    held_beyond_reference_frames: u32,
}

impl DecoderFlush {
    /// Starts from the parameter sets in the stream's `avcC` record.
    pub(crate) fn new(configuration: &VideoCodecConfiguration) -> Self {
        Self::with_held_frames(configuration, HELD_BEYOND_REFERENCE_FRAMES)
    }

    /// Starts with a custom number of pictures held beyond reference frames.
    pub(crate) fn with_held_frames(
        configuration: &VideoCodecConfiguration,
        held_beyond_reference_frames: u32,
    ) -> Self {
        let (sequence, picture) = match parameter_sets(&configuration.codec_data) {
            Some((sequence, picture)) => (
                sequence_parameters(sequence),
                picture_parameters(picture),
            ),
            None => (None, None),
        };
        Self {
            nal_length_bytes: usize::from(configuration.nal_length_bytes),
            sequence,
            picture,
            held_beyond_reference_frames,
        }
    }

    /// The pictures that release `frame` from the decoder, or `None` when the
    /// stream is not one this is known to work for.
    pub(crate) fn followers(&mut self, frame: &EncryptedVideoFrame) -> Option<FlushPictures> {
        let mut slice = None;
        for descriptor in &frame.nal_units {
            let end = descriptor.payload_offset.checked_add(descriptor.payload_length)?;
            let unit = frame.data.get(descriptor.payload_offset..end)?;
            match descriptor.unit_type {
                7 => self.sequence = sequence_parameters(unit),
                8 => self.picture = picture_parameters(unit),
                1 | 5 if slice.is_none() => slice = Some(unit),
                _ => {}
            }
        }
        let sequence = self.sequence?;
        let picture = self.picture?;
        if picture.sequence_id != sequence.id || !matches!(self.nal_length_bytes, 1 | 2 | 4) {
            return None;
        }
        let slice = slice_start(slice?, sequence)?;
        if slice.picture_parameters_id != picture.id {
            return None;
        }
        // A picture that is not a reference shares its frame number with the
        // next one; after a reference the number moves on by one.
        let frame_num = if slice.reference {
            (slice.frame_num + 1) % (1 << sequence.frame_num_bits)
        } else {
            slice.frame_num
        };
        let unit = skipped_picture(sequence, picture, frame_num);
        let length = u32::try_from(unit.len()).ok()?.to_be_bytes();
        let mut framed = Vec::with_capacity(self.nal_length_bytes + unit.len());
        framed.extend_from_slice(&length[length.len() - self.nal_length_bytes..]);
        framed.extend_from_slice(&unit);
        Some(FlushPictures {
            picture: framed,
            count: usize::try_from(sequence.reference_frames + self.held_beyond_reference_frames)
                .ok()?,
        })
    }
}

/// Whether a decoded picture with this presentation time is one of the
/// skipped pictures in `pending`, which lists them oldest first. Forgets it
/// and every older one, which the decoder can no longer return.
pub(crate) fn take_flush_output(pending: &mut VecDeque<u64>, presentation_time_ns: u64) -> bool {
    match pending
        .iter()
        .position(|flush| *flush == presentation_time_ns)
    {
        Some(position) => {
            pending.drain(..=position);
            true
        }
        None => false,
    }
}

/// The single sequence and picture parameter set of an `avcC` record.
fn parameter_sets(codec_data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (sequence, rest) = only_unit(codec_data.get(5..)?, 0x1f)?;
    let (picture, _) = only_unit(rest, 0xff)?;
    Some((sequence, picture))
}

/// The unit of an `avcC` list that holds exactly one, and what follows it.
fn only_unit(list: &[u8], count_mask: u8) -> Option<(&[u8], &[u8])> {
    let (&count, list) = list.split_first()?;
    if count & count_mask != 1 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([*list.first()?, *list.get(1)?]));
    let end = length.checked_add(2)?;
    Some((list.get(2..end)?, list.get(end..)?))
}

fn sequence_parameters(unit: &[u8]) -> Option<SequenceParameters> {
    if unit.first()? & 0x1f != 7 || *unit.get(1)? != BASELINE_PROFILE {
        return None;
    }
    let payload = without_emulation_prevention(unit.get(4..)?);
    let mut bits = BitReader::new(&payload);
    let id = bits.unsigned_golomb()?;
    let frame_num_bits = bits.unsigned_golomb()?.checked_add(4)?;
    if frame_num_bits > 16 || bits.unsigned_golomb()? != 2 {
        return None;
    }
    let reference_frames = bits.unsigned_golomb()?;
    bits.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width = bits.unsigned_golomb()?.checked_add(1)?;
    let height = bits.unsigned_golomb()?.checked_add(1)?;
    let frame_macroblocks_only = bits.bit()?;
    let macroblocks = width.checked_mul(height)?;
    if reference_frames != 1 || !frame_macroblocks_only || macroblocks > MAXIMUM_MACROBLOCKS {
        return None;
    }
    Some(SequenceParameters {
        id,
        frame_num_bits,
        reference_frames,
        macroblocks,
    })
}

fn picture_parameters(unit: &[u8]) -> Option<PictureParameters> {
    if unit.first()? & 0x1f != 8 {
        return None;
    }
    let payload = without_emulation_prevention(unit.get(1..)?);
    let mut bits = BitReader::new(&payload);
    let id = bits.unsigned_golomb()?;
    let sequence_id = bits.unsigned_golomb()?;
    let arithmetic_coding = bits.bit()?;
    bits.bit()?; // bottom_field_pic_order_in_frame_present_flag
    let slice_groups = bits.unsigned_golomb()?.checked_add(1)?;
    let default_references = bits.unsigned_golomb()?.checked_add(1)?;
    bits.unsigned_golomb()?; // num_ref_idx_l1_default_active_minus1
    let weighted_prediction = bits.bit()?;
    bits.bits(2)?; // weighted_bipred_idc
    bits.unsigned_golomb()?; // pic_init_qp_minus26
    bits.unsigned_golomb()?; // pic_init_qs_minus26
    bits.unsigned_golomb()?; // chroma_qp_index_offset
    let deblocking_control = bits.bit()?;
    bits.bit()?; // constrained_intra_pred_flag
    let redundant_picture_count = bits.bit()?;
    if arithmetic_coding || slice_groups != 1 || default_references != 1 || weighted_prediction {
        return None;
    }
    Some(PictureParameters {
        id,
        sequence_id,
        deblocking_control,
        redundant_picture_count,
    })
}

fn slice_start(unit: &[u8], sequence: SequenceParameters) -> Option<SliceStart> {
    let header = *unit.first()?;
    let prefix = unit.get(1..)?;
    let payload = without_emulation_prevention(
        &prefix[..prefix.len().min(SLICE_HEADER_PREFIX_BYTES)],
    );
    let mut bits = BitReader::new(&payload);
    let first_macroblock = bits.unsigned_golomb()?;
    let slice_type = bits.unsigned_golomb()?;
    let picture_parameters_id = bits.unsigned_golomb()?;
    let frame_num = bits.bits(sequence.frame_num_bits)?;
    // Only I and P slices exist in the streams this handles.
    if first_macroblock != 0 || !matches!(slice_type % 5, 0 | 2) {
        return None;
    }
    Some(SliceStart {
        picture_parameters_id,
        frame_num,
        reference: header & 0x60 != 0,
    })
}

/// A P picture that is not a reference and whose every macroblock is
/// skipped, as one NAL unit.
fn skipped_picture(
    sequence: SequenceParameters,
    picture: PictureParameters,
    frame_num: u32,
) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.unsigned_golomb(0); // first_mb_in_slice
    bits.unsigned_golomb(0); // slice_type: P
    bits.unsigned_golomb(picture.id);
    bits.bits(frame_num, sequence.frame_num_bits);
    if picture.redundant_picture_count {
        bits.unsigned_golomb(0);
    }
    bits.bit(false); // num_ref_idx_active_override_flag
    bits.bit(false); // ref_pic_list_modification_flag_l0
    bits.unsigned_golomb(0); // slice_qp_delta
    if picture.deblocking_control {
        bits.unsigned_golomb(1); // disable_deblocking_filter_idc: off
    }
    bits.unsigned_golomb(sequence.macroblocks); // mb_skip_run
    let mut unit = vec![0x01]; // not a reference, coded slice of a non-IDR picture
    unit.extend(with_emulation_prevention(&bits.finish()));
    unit
}

fn without_emulation_prevention(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut zeros = 0_u8;
    for &byte in bytes {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        plain.push(byte);
        zeros = if byte == 0 { zeros.saturating_add(1) } else { 0 };
    }
    plain
}

fn with_emulation_prevention(bytes: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(bytes.len() + 2);
    let mut zeros = 0_u8;
    for &byte in bytes {
        if zeros >= 2 && byte <= 3 {
            escaped.push(3);
            zeros = 0;
        }
        escaped.push(byte);
        zeros = if byte == 0 { zeros.saturating_add(1) } else { 0 };
    }
    escaped
}

struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.bytes.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1 == 1;
        self.position += 1;
        Some(bit)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.bit()?);
        }
        Some(value)
    }

    fn unsigned_golomb(&mut self) -> Option<u32> {
        let mut zeros = 0_u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        ((1_u32 << zeros) - 1).checked_add(self.bits(zeros)?)
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    position: usize,
}

impl BitWriter {
    fn bit(&mut self, bit: bool) {
        if self.position % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.position % 8);
        }
        self.position += 1;
    }

    fn bits(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            self.bit((value >> shift) & 1 == 1);
        }
    }

    fn unsigned_golomb(&mut self, value: u32) {
        let value = u64::from(value) + 1;
        let length = 64 - value.leading_zeros();
        for _ in 1..length {
            self.bit(false);
        }
        for shift in (0..length).rev() {
            self.bit((value >> shift) & 1 == 1);
        }
    }

    /// Ends the payload with its stop bit and alignment.
    fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        while self.position % 8 != 0 {
            self.bit(false);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NalUnitDescriptor, WireVideoCodec};

    // The parameter sets a Pixel 9 Pro XL sends for 800x480 Android Auto.
    const PHONE_SEQUENCE: &str = "6742001fda0320f69a80808083c50aa8";
    const PHONE_PICTURE: &str = "68ce0d88";
    // The parameter sets of the x264 stream the decoder was measured with.
    const X264_SEQUENCE: &str = "6742c01fda0320f6c044000003000400000300f23c60ca80";
    const X264_PICTURE: &str = "68ce3c80";

    fn configuration(sequence: &str, picture: &str) -> VideoCodecConfiguration {
        let sequence = hex::decode(sequence).expect("sequence parameter set");
        let picture = hex::decode(picture).expect("picture parameter set");
        let mut codec_data = vec![1, sequence[1], sequence[2], sequence[3], 0xff, 0xe1];
        codec_data.extend_from_slice(&(sequence.len() as u16).to_be_bytes());
        codec_data.extend_from_slice(&sequence);
        codec_data.push(1);
        codec_data.extend_from_slice(&(picture.len() as u16).to_be_bytes());
        codec_data.extend_from_slice(&picture);
        VideoCodecConfiguration {
            codec: WireVideoCodec::H264,
            codec_data,
            nal_length_bytes: 4,
        }
    }

    fn frame(units: &[&str]) -> EncryptedVideoFrame {
        let mut data = Vec::new();
        let mut nal_units = Vec::new();
        for unit in units {
            let unit = hex::decode(unit).expect("NAL unit");
            data.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            nal_units.push(NalUnitDescriptor {
                payload_offset: data.len(),
                payload_length: unit.len(),
                unit_type: unit[0] & 0x1f,
            });
            data.extend_from_slice(&unit);
        }
        EncryptedVideoFrame {
            data,
            codec: WireVideoCodec::H264,
            nal_units,
            presentation_time_ns: None,
            nonce_counter: 1,
        }
    }

    #[test]
    fn custom_held_frames_configures_flush_count() {
        let mut flush =
            DecoderFlush::with_held_frames(&configuration(X264_SEQUENCE, X264_PICTURE), 0);
        let after_key_frame = flush
            .followers(&frame(&["6588840a7c4222a20c085f87"]))
            .expect("followers");
        assert_eq!(after_key_frame.count, 1);
    }

    #[test]
    fn reference_frame_is_followed_by_three_pictures_with_the_next_frame_number() {
        let mut flush = DecoderFlush::new(&configuration(X264_SEQUENCE, X264_PICTURE));
        // Slices of the measured stream: an IDR with frame_num 0 and the P
        // picture with frame_num 1 that follows it.
        let after_key_frame = flush
            .followers(&frame(&["6588840a7c4222a20c085f87"]))
            .expect("followers");
        assert_eq!(after_key_frame.count, 3);
        assert_eq!(hex::encode(&after_key_frame.picture), "0000000601e250017760");
        let after_second = flush
            .followers(&frame(&["419a20107f3c2133606a4099"]))
            .expect("followers");
        assert_eq!(hex::encode(&after_second.picture), "0000000601e450017760");
    }

    #[test]
    fn skipped_picture_matches_the_one_decoded_on_the_device() {
        let sequence = sequence_parameters(&hex::decode(X264_SEQUENCE).expect("hex"))
            .expect("sequence parameters");
        let picture = picture_parameters(&hex::decode(X264_PICTURE).expect("hex"))
            .expect("picture parameters");
        assert_eq!(
            hex::encode(skipped_picture(sequence, picture, 5)),
            "01ea50017760"
        );
    }

    #[test]
    fn phone_parameter_sets_are_supported() {
        let sequence = sequence_parameters(&hex::decode(PHONE_SEQUENCE).expect("hex"))
            .expect("sequence parameters");
        assert_eq!(
            sequence,
            SequenceParameters {
                id: 0,
                frame_num_bits: 4,
                reference_frames: 1,
                macroblocks: 1_500,
            }
        );
        let picture = picture_parameters(&hex::decode(PHONE_PICTURE).expect("hex"))
            .expect("picture parameters");
        assert_eq!(
            picture,
            PictureParameters {
                id: 0,
                sequence_id: 0,
                deblocking_control: false,
                redundant_picture_count: false,
            }
        );
        for (frame_num, expected) in [(1, "01e2400bbb"), (0, "01e0400bbb"), (15, "01fe400bbb")] {
            assert_eq!(
                hex::encode(skipped_picture(sequence, picture, frame_num)),
                expected
            );
        }
    }

    #[test]
    fn frame_number_wraps_and_stays_put_after_a_picture_that_is_not_a_reference() {
        let mut flush = DecoderFlush::new(&configuration(PHONE_SEQUENCE, PHONE_PICTURE));
        // first_mb 0, slice_type 5, pps 0, frame_num 15, as a reference.
        let last_of_cycle = flush.followers(&frame(&["419be0"])).expect("followers");
        assert_eq!(hex::encode(&last_of_cycle.picture), "0000000501e0400bbb");
        // The same slice start marked as not a reference.
        let unreferenced = flush.followers(&frame(&["019be0"])).expect("followers");
        assert_eq!(hex::encode(&unreferenced.picture), "0000000501fe400bbb");
    }

    #[test]
    fn parameter_sets_inside_a_frame_replace_the_configured_ones() {
        let mut flush = DecoderFlush::new(&configuration(X264_SEQUENCE, X264_PICTURE));
        let followers = flush
            .followers(&frame(&[PHONE_SEQUENCE, PHONE_PICTURE, "6588840a7c42"]))
            .expect("followers");
        assert_eq!(hex::encode(&followers.picture), "0000000501e2400bbb");
    }

    #[test]
    fn streams_outside_the_measured_shape_are_left_alone() {
        let key_frame = frame(&["6588840a7c4222a20c085f87"]);
        // High profile.
        let mut high = DecoderFlush::new(&configuration(
            "6764001fda0320f69a80808083c50aa8",
            PHONE_PICTURE,
        ));
        assert_eq!(high.followers(&key_frame), None);
        // The phone's sequence with max_num_ref_frames raised to 2.
        let mut two_references = DecoderFlush::new(&configuration(
            "6742001fdb0320f69a80808083c50aa8",
            PHONE_PICTURE,
        ));
        assert_eq!(two_references.followers(&key_frame), None);
        // CABAC picture parameter set.
        let mut arithmetic =
            DecoderFlush::new(&configuration(PHONE_SEQUENCE, "68ee0d88"));
        assert_eq!(arithmetic.followers(&key_frame), None);
        // A frame without a slice, and a slice that does not start a picture.
        let mut supported = DecoderFlush::new(&configuration(PHONE_SEQUENCE, PHONE_PICTURE));
        assert_eq!(supported.followers(&frame(&[PHONE_PICTURE])), None);
        assert_eq!(supported.followers(&frame(&["4130"])), None);
        // Truncated codec data.
        let mut truncated = DecoderFlush::new(&VideoCodecConfiguration {
            codec: WireVideoCodec::H264,
            codec_data: vec![1, 66, 0, 31, 0xff, 0xe1, 0, 9, 0x67],
            nal_length_bytes: 4,
        });
        assert_eq!(truncated.followers(&frame(&["419be0"])), None);
    }

    #[test]
    fn decoded_skipped_pictures_are_recognised_and_frames_are_not() {
        let mut pending = VecDeque::from([1_001, 1_002, 1_003, 2_001]);
        assert!(!take_flush_output(&mut pending, 1_000));
        assert!(take_flush_output(&mut pending, 1_001));
        // The decoder never returned 1_002, so it is forgotten with 1_003.
        assert!(take_flush_output(&mut pending, 1_003));
        assert!(!take_flush_output(&mut pending, 1_002));
        assert!(!take_flush_output(&mut pending, 2_000));
        assert_eq!(pending, VecDeque::from([2_001]));
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let plain = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        let escaped = with_emulation_prevention(&plain);
        assert!(escaped.windows(3).any(|window| window == [0, 0, 3]));
        assert_eq!(without_emulation_prevention(&escaped), plain);
    }
}
