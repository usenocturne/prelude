#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-backend")),
    allow(dead_code)
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{EncryptedVideoFrame, NalUnitDescriptor, VideoCodecConfiguration};

const HELD_BEYOND_REFERENCE_FRAMES: u32 = 2;
const MAXIMUM_REFERENCE_FRAMES: u32 = 4;
const MAXIMUM_MACROBLOCKS: u32 = 36_864;
const MAXIMUM_ORDER_BITS: u32 = 16;
const SLICE_HEADER_PREFIX_BYTES: usize = 24;
const DECODER_PICTURE_BUDGET: Duration = Duration::from_millis(5);
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];
const ARITHMETIC_SLICE_QP: i32 = 26;
const ARITHMETIC_SKIP_STATE: usize = 6;
const ARITHMETIC_STEADY_STATE: usize = 62;
const ARITHMETIC_RANGE: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlushPictures {
    pub(crate) pictures: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PictureOrder {
    FrameNumber,
    Counter { bits: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SequenceParameters {
    id: u32,
    frame_num_bits: u32,
    picture_order: PictureOrder,
    reference_frames: u32,
    macroblocks: u32,
    chroma: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PictureParameters {
    id: u32,
    sequence_id: u32,
    arithmetic_coding: bool,
    bottom_field_order: bool,
    default_references: u32,
    weighted_prediction: bool,
    initial_qp: i32,
    deblocking_control: bool,
    redundant_picture_count: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceStart {
    first_macroblock: u32,
    slice_type: u32,
    picture_parameters_id: u32,
    frame_num: u32,
    order_position: usize,
    reference: bool,
}

#[derive(Debug)]
struct Placement {
    sequence: SequenceParameters,
    starts: Vec<SliceStart>,
    orders: Vec<Option<u32>>,
}

#[derive(Debug, Default)]
pub(crate) struct DecoderFlush {
    nal_length_bytes: usize,
    sequence: Option<SequenceParameters>,
    picture: Option<PictureParameters>,
    order_index: Option<u64>,
    reordered: bool,
    arithmetic_body: Option<(u32, Vec<u8>)>,
}

impl DecoderFlush {
    pub(crate) fn new(configuration: &VideoCodecConfiguration) -> Self {
        let (sequence, picture) = match parameter_sets(&configuration.codec_data) {
            Some((sequence, picture)) => {
                (sequence_parameters(sequence), picture_parameters(picture))
            }
            None => (None, None),
        };
        Self {
            nal_length_bytes: usize::from(configuration.nal_length_bytes),
            sequence,
            picture,
            ..Self::default()
        }
    }

    pub(crate) fn prepare(&mut self, frame: &mut EncryptedVideoFrame) -> Option<FlushPictures> {
        let mut slices = Vec::new();
        for (index, descriptor) in frame.nal_units.iter().enumerate() {
            let unit = nal_unit(&frame.data, descriptor)?;
            match descriptor.unit_type {
                7 => self.sequence = sequence_parameters(unit),
                8 => self.picture = picture_parameters(unit),
                1 | 5 => slices.push(index),
                _ => {}
            }
        }
        if slices.is_empty() {
            return None;
        }
        let Some(placement) = self.place(frame, &slices) else {
            self.order_index = None;
            return None;
        };
        self.followers(&placement)
    }

    fn place(&mut self, frame: &mut EncryptedVideoFrame, slices: &[usize]) -> Option<Placement> {
        let sequence = self.sequence?;
        let nal_length_bytes = self.nal_length_bytes;
        if self.reordered || !matches!(nal_length_bytes, 1 | 2 | 4) {
            return None;
        }
        let mut starts = Vec::with_capacity(slices.len());
        let mut key_frame = false;
        for &index in slices {
            let descriptor = frame.nal_units.get(index)?;
            key_frame |= descriptor.unit_type == 5;
            let start = slice_start(nal_unit(&frame.data, descriptor)?, sequence)?;
            if start.slice_type % 5 == 1 {
                self.reordered = true;
                return None;
            }
            starts.push(start);
        }
        if starts.first()?.first_macroblock != 0 {
            return None;
        }
        let count = sequence.reference_frames + HELD_BEYOND_REFERENCE_FRAMES;
        let orders = match sequence.picture_order {
            PictureOrder::FrameNumber => vec![None; usize::try_from(count).ok()?],
            PictureOrder::Counter { bits } => {
                let step = u64::from(count) + 1;
                let modulus = 1_u64 << bits;
                if modulus < 4 * step {
                    return None;
                }
                let index = if key_frame { 0 } else { self.order_index? };
                let order = index.wrapping_mul(step) % modulus;
                renumber(
                    frame,
                    slices,
                    &starts,
                    bits,
                    u32::try_from(order).ok()?,
                    nal_length_bytes,
                )?;
                self.order_index = Some(index.wrapping_add(1));
                (1..=u64::from(count))
                    .map(|follower| u32::try_from((order + follower) % modulus).ok())
                    .collect()
            }
        };
        Some(Placement {
            sequence,
            starts,
            orders,
        })
    }

    fn followers(&mut self, placement: &Placement) -> Option<FlushPictures> {
        let sequence = placement.sequence;
        let picture = self.picture?;
        if picture.sequence_id != sequence.id
            || placement.starts.iter().any(|start| {
                start.picture_parameters_id != picture.id || !matches!(start.slice_type % 5, 0 | 2)
            })
        {
            return None;
        }
        let first = placement.starts.first()?;
        let frame_num = if first.reference {
            (first.frame_num + 1) % (1 << sequence.frame_num_bits)
        } else {
            first.frame_num
        };
        let nal_length_bytes = self.nal_length_bytes;
        let body = if picture.arithmetic_coding {
            Some(self.arithmetic_body(sequence.macroblocks))
        } else {
            None
        };
        let mut pictures = Vec::with_capacity(placement.orders.len());
        for &order in &placement.orders {
            let unit = skipped_picture(sequence, picture, frame_num, order, body);
            let length = u32::try_from(unit.len()).ok()?.to_be_bytes();
            let mut framed = Vec::with_capacity(nal_length_bytes + unit.len());
            framed.extend_from_slice(&length[length.len() - nal_length_bytes..]);
            framed.extend_from_slice(&unit);
            pictures.push(framed);
        }
        Some(FlushPictures { pictures })
    }

    fn arithmetic_body(&mut self, macroblocks: u32) -> &[u8] {
        if self
            .arithmetic_body
            .as_ref()
            .is_none_or(|(cached, _)| *cached != macroblocks)
        {
            self.arithmetic_body = Some((macroblocks, arithmetic_skip_run(macroblocks)));
        }
        self.arithmetic_body
            .as_ref()
            .map_or(&[], |(_, body)| body.as_slice())
    }
}

#[derive(Debug, Default)]
pub(crate) struct FlushPacing {
    recent_frames: [Option<Instant>; 2],
}

impl FlushPacing {
    pub(crate) fn immediate_pictures(&mut self, now: Instant) -> usize {
        let interval = match self.recent_frames {
            [Some(earlier), _] => Some(now.saturating_duration_since(earlier) / 2),
            [None, Some(previous)] => Some(now.saturating_duration_since(previous)),
            [None, None] => None,
        };
        self.recent_frames = [self.recent_frames[1], Some(now)];
        interval.map_or(usize::MAX, |interval| {
            usize::try_from(interval.as_micros() / DECODER_PICTURE_BUDGET.as_micros())
                .unwrap_or(usize::MAX)
                .saturating_sub(1)
        })
    }
}

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

fn nal_unit<'a>(data: &'a [u8], descriptor: &NalUnitDescriptor) -> Option<&'a [u8]> {
    let end = descriptor
        .payload_offset
        .checked_add(descriptor.payload_length)?;
    data.get(descriptor.payload_offset..end)
}

fn renumber(
    frame: &mut EncryptedVideoFrame,
    slices: &[usize],
    starts: &[SliceStart],
    bits: u32,
    order: u32,
    nal_length_bytes: usize,
) -> Option<()> {
    let mut rewritten = Vec::with_capacity(slices.len());
    let mut resized = false;
    for (&index, start) in slices.iter().zip(starts) {
        let unit = nal_unit(&frame.data, frame.nal_units.get(index)?)?;
        let mut payload = without_emulation_prevention(unit.get(1..)?);
        for offset in 0..bits {
            let position = start.order_position + offset as usize;
            let byte = payload.get_mut(position / 8)?;
            let mask = 1_u8 << (7 - position % 8);
            if (order >> (bits - 1 - offset)) & 1 == 1 {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
        let mut renumbered = Vec::with_capacity(unit.len() + 2);
        renumbered.push(*unit.first()?);
        renumbered.extend(with_emulation_prevention(&payload));
        resized |= renumbered.len() != unit.len();
        rewritten.push(renumbered);
    }
    if !resized {
        for (&index, unit) in slices.iter().zip(&rewritten) {
            let descriptor = frame.nal_units.get(index)?;
            let end = descriptor.payload_offset + descriptor.payload_length;
            frame
                .data
                .get_mut(descriptor.payload_offset..end)?
                .copy_from_slice(unit);
        }
        return Some(());
    }
    let mut cursor = 0_usize;
    for descriptor in &frame.nal_units {
        if descriptor.payload_offset != cursor.checked_add(nal_length_bytes)? {
            return None;
        }
        cursor = descriptor
            .payload_offset
            .checked_add(descriptor.payload_length)?;
    }
    if cursor != frame.data.len() {
        return None;
    }
    let mut data = Vec::with_capacity(frame.data.len() + 2 * slices.len());
    let mut descriptors = Vec::with_capacity(frame.nal_units.len());
    let mut replacements = slices.iter().zip(&rewritten).peekable();
    for (index, descriptor) in frame.nal_units.iter().enumerate() {
        let unit = match replacements.peek() {
            Some((slice, replacement)) if **slice == index => {
                let replacement = replacement.as_slice();
                replacements.next();
                replacement
            }
            _ => nal_unit(&frame.data, descriptor)?,
        };
        let length = u32::try_from(unit.len()).ok()?.to_be_bytes();
        if length[..length.len() - nal_length_bytes]
            .iter()
            .any(|byte| *byte != 0)
        {
            return None;
        }
        data.extend_from_slice(&length[length.len() - nal_length_bytes..]);
        descriptors.push(NalUnitDescriptor {
            payload_offset: data.len(),
            payload_length: unit.len(),
            unit_type: descriptor.unit_type,
        });
        data.extend_from_slice(unit);
    }
    frame.data = data;
    frame.nal_units = descriptors;
    Some(())
}

fn parameter_sets(codec_data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (sequence, rest) = only_unit(codec_data.get(5..)?, 0x1f)?;
    let (picture, _) = only_unit(rest, 0xff)?;
    Some((sequence, picture))
}

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
    if unit.first()? & 0x1f != 7 {
        return None;
    }
    let profile = *unit.get(1)?;
    let payload = without_emulation_prevention(unit.get(4..)?);
    let mut bits = BitReader::new(&payload);
    let id = bits.unsigned_golomb()?;
    let mut chroma = true;
    if HIGH_PROFILES.contains(&profile) {
        let chroma_format = bits.unsigned_golomb()?;
        if chroma_format > 3 || (chroma_format == 3 && bits.bit()?) {
            return None;
        }
        chroma = chroma_format != 0;
        bits.unsigned_golomb()?;
        bits.unsigned_golomb()?;
        bits.bit()?;
        if bits.bit()? {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.bit()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let frame_num_bits = bits.unsigned_golomb()?.checked_add(4)?;
    if frame_num_bits > 16 {
        return None;
    }
    let picture_order = match bits.unsigned_golomb()? {
        0 => {
            let order_bits = bits.unsigned_golomb()?.checked_add(4)?;
            if order_bits > MAXIMUM_ORDER_BITS {
                return None;
            }
            PictureOrder::Counter { bits: order_bits }
        }
        2 => PictureOrder::FrameNumber,
        _ => return None,
    };
    let reference_frames = bits.unsigned_golomb()?;
    bits.bit()?;
    let width = bits.unsigned_golomb()?.checked_add(1)?;
    let height = bits.unsigned_golomb()?.checked_add(1)?;
    let frame_macroblocks_only = bits.bit()?;
    let macroblocks = width.checked_mul(height)?;
    if reference_frames == 0
        || reference_frames > MAXIMUM_REFERENCE_FRAMES
        || !frame_macroblocks_only
        || macroblocks > MAXIMUM_MACROBLOCKS
    {
        return None;
    }
    Some(SequenceParameters {
        id,
        frame_num_bits,
        picture_order,
        reference_frames,
        macroblocks,
        chroma,
    })
}

fn skip_scaling_list(bits: &mut BitReader<'_>, size: u32) -> Option<()> {
    let mut last = 8_i32;
    let mut next = 8_i32;
    for _ in 0..size {
        if next != 0 {
            next = (last + bits.signed_golomb()?).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
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
    let bottom_field_order = bits.bit()?;
    let slice_groups = bits.unsigned_golomb()?.checked_add(1)?;
    let default_references = bits.unsigned_golomb()?.checked_add(1)?;
    bits.unsigned_golomb()?;
    let weighted_prediction = bits.bit()?;
    bits.bits(2)?;
    let initial_qp = bits.signed_golomb()?;
    bits.signed_golomb()?;
    bits.signed_golomb()?;
    let deblocking_control = bits.bit()?;
    bits.bit()?;
    let redundant_picture_count = bits.bit()?;
    if slice_groups != 1 || !(-26..=25).contains(&initial_qp) {
        return None;
    }
    Some(PictureParameters {
        id,
        sequence_id,
        arithmetic_coding,
        bottom_field_order,
        default_references,
        weighted_prediction,
        initial_qp,
        deblocking_control,
        redundant_picture_count,
    })
}

fn slice_start(unit: &[u8], sequence: SequenceParameters) -> Option<SliceStart> {
    let header = *unit.first()?;
    let prefix = unit.get(1..)?;
    let payload =
        without_emulation_prevention(&prefix[..prefix.len().min(SLICE_HEADER_PREFIX_BYTES)]);
    let mut bits = BitReader::new(&payload);
    let first_macroblock = bits.unsigned_golomb()?;
    let slice_type = bits.unsigned_golomb()?;
    let picture_parameters_id = bits.unsigned_golomb()?;
    let frame_num = bits.bits(sequence.frame_num_bits)?;
    if header & 0x1f == 5 {
        bits.unsigned_golomb()?;
    }
    let order_position = bits.position;
    if let PictureOrder::Counter { bits: order_bits } = sequence.picture_order {
        bits.bits(order_bits)?;
    }
    Some(SliceStart {
        first_macroblock,
        slice_type,
        picture_parameters_id,
        frame_num,
        order_position,
        reference: header & 0x60 != 0,
    })
}

fn skipped_picture(
    sequence: SequenceParameters,
    picture: PictureParameters,
    frame_num: u32,
    order: Option<u32>,
    arithmetic_body: Option<&[u8]>,
) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.unsigned_golomb(0);
    bits.unsigned_golomb(0);
    bits.unsigned_golomb(picture.id);
    bits.bits(frame_num, sequence.frame_num_bits);
    if let (PictureOrder::Counter { bits: order_bits }, Some(order)) =
        (sequence.picture_order, order)
    {
        bits.bits(order, order_bits);
    }
    if order.is_some() && picture.bottom_field_order {
        bits.signed_golomb(0);
    }
    if picture.redundant_picture_count {
        bits.unsigned_golomb(0);
    }
    if picture.default_references == 1 {
        bits.bit(false);
    } else {
        bits.bit(true);
        bits.unsigned_golomb(0);
    }
    bits.bit(false);
    if picture.weighted_prediction {
        bits.unsigned_golomb(0);
        if sequence.chroma {
            bits.unsigned_golomb(0);
        }
        bits.bit(false);
        if sequence.chroma {
            bits.bit(false);
        }
    }
    let payload = match arithmetic_body {
        Some(body) => {
            bits.unsigned_golomb(0);
            bits.signed_golomb(ARITHMETIC_SLICE_QP - 26 - picture.initial_qp);
            if picture.deblocking_control {
                bits.unsigned_golomb(1);
            }
            while bits.position % 8 != 0 {
                bits.bit(true);
            }
            let mut payload = bits.bytes;
            payload.extend_from_slice(body);
            payload
        }
        None => {
            bits.signed_golomb(0);
            if picture.deblocking_control {
                bits.unsigned_golomb(1);
            }
            bits.unsigned_golomb(sequence.macroblocks);
            bits.finish()
        }
    };
    let mut unit = vec![0x01];
    unit.extend(with_emulation_prevention(&payload));
    unit
}

fn arithmetic_skip_run(macroblocks: u32) -> Vec<u8> {
    let mut encoder = ArithmeticEncoder {
        low: 0,
        range: 510,
        outstanding: 0,
        first: true,
        bits: BitWriter::default(),
    };
    let mut state = ARITHMETIC_SKIP_STATE;
    for macroblock in 0..macroblocks {
        encoder.most_probable(state);
        state = (state + 1).min(ARITHMETIC_STEADY_STATE);
        encoder.terminate(macroblock + 1 == macroblocks);
    }
    encoder.bits.bytes
}

struct ArithmeticEncoder {
    low: u32,
    range: u32,
    outstanding: u32,
    first: bool,
    bits: BitWriter,
}

impl ArithmeticEncoder {
    fn put(&mut self, bit: bool) {
        if self.first {
            self.first = false;
        } else {
            self.bits.bit(bit);
        }
        while self.outstanding > 0 {
            self.bits.bit(!bit);
            self.outstanding -= 1;
        }
    }

    fn renormalize(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put(false);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put(true);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    fn most_probable(&mut self, state: usize) {
        let quarter = ((self.range >> 6) & 3) as usize;
        self.range -= u32::from(ARITHMETIC_RANGE[state][quarter]);
        self.renormalize();
    }

    fn terminate(&mut self, end: bool) {
        self.range -= 2;
        if end {
            self.low += self.range;
            self.range = 2;
            self.renormalize();
            self.put((self.low >> 9) & 1 == 1);
            self.bits.bits(((self.low >> 7) & 3) | 1, 2);
        } else {
            self.renormalize();
        }
    }
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
        zeros = if byte == 0 {
            zeros.saturating_add(1)
        } else {
            0
        };
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
        zeros = if byte == 0 {
            zeros.saturating_add(1)
        } else {
            0
        };
    }
    if escaped.last() == Some(&0) {
        escaped.push(3);
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

    fn signed_golomb(&mut self) -> Option<i32> {
        let code = self.unsigned_golomb()?;
        let magnitude = i32::try_from(code.div_ceil(2)).ok()?;
        Some(if code % 2 == 1 { magnitude } else { -magnitude })
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

    fn signed_golomb(&mut self, value: i32) {
        let magnitude = value.unsigned_abs();
        self.unsigned_golomb(if value > 0 {
            2 * magnitude - 1
        } else {
            2 * magnitude
        });
    }

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
    use crate::WireVideoCodec;

    const PHONE_SEQUENCE: &str = "6742001fda0320f69a80808083c50aa8";
    const PHONE_PICTURE: &str = "68ce0d88";
    const X264_SEQUENCE: &str = "6742c01fda0320f6c044000003000400000300f23c60ca80";
    const X264_PICTURE: &str = "68ce3c80";
    const APPLE_SEQUENCE: &str = "2764001fac5680c83d90";
    const APPLE_PICTURE: &str = "28ee3cb0";
    const X264_HIGH_SEQUENCE: &str = "6764001facb201907b6022000003000200000300f01e306490";
    const X264_HIGH_PICTURE: &str = "68ebc3cb22c0";

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

    fn frame_of(units: &[Vec<u8>]) -> EncryptedVideoFrame {
        let mut data = Vec::new();
        let mut nal_units = Vec::new();
        for unit in units {
            data.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            nal_units.push(NalUnitDescriptor {
                payload_offset: data.len(),
                payload_length: unit.len(),
                unit_type: unit[0] & 0x1f,
            });
            data.extend_from_slice(unit);
        }
        EncryptedVideoFrame {
            data,
            codec: WireVideoCodec::H264,
            nal_units,
            presentation_time_ns: None,
            nonce_counter: 1,
        }
    }

    fn frame(units: &[&str]) -> EncryptedVideoFrame {
        frame_of(
            &units
                .iter()
                .map(|unit| hex::decode(unit).expect("NAL unit"))
                .collect::<Vec<_>>(),
        )
    }

    fn pictures(flush: &mut DecoderFlush, units: &[&str]) -> Option<Vec<String>> {
        flush
            .prepare(&mut frame(units))
            .map(|followers| followers.pictures.iter().map(hex::encode).collect())
    }

    #[test]
    fn reference_frame_is_followed_by_three_pictures_with_the_next_frame_number() {
        let mut flush = DecoderFlush::new(&configuration(X264_SEQUENCE, X264_PICTURE));
        assert_eq!(
            pictures(&mut flush, &["6588840a7c4222a20c085f87"]).expect("followers"),
            ["0000000601e250017760"; 3]
        );
        assert_eq!(
            pictures(&mut flush, &["419a20107f3c2133606a4099"]).expect("followers"),
            ["0000000601e450017760"; 3]
        );
    }

    #[test]
    fn skipped_picture_matches_the_one_decoded_on_the_device() {
        let sequence = sequence_parameters(&hex::decode(X264_SEQUENCE).expect("hex"))
            .expect("sequence parameters");
        let picture = picture_parameters(&hex::decode(X264_PICTURE).expect("hex"))
            .expect("picture parameters");
        assert_eq!(
            hex::encode(skipped_picture(sequence, picture, 5, None, None)),
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
                picture_order: PictureOrder::FrameNumber,
                reference_frames: 1,
                macroblocks: 1_500,
                chroma: true,
            }
        );
        let picture = picture_parameters(&hex::decode(PHONE_PICTURE).expect("hex"))
            .expect("picture parameters");
        assert_eq!(
            picture,
            PictureParameters {
                id: 0,
                sequence_id: 0,
                arithmetic_coding: false,
                bottom_field_order: false,
                default_references: 1,
                weighted_prediction: false,
                initial_qp: 3,
                deblocking_control: false,
                redundant_picture_count: false,
            }
        );
        for (frame_num, expected) in [(1, "01e2400bbb"), (0, "01e0400bbb"), (15, "01fe400bbb")] {
            assert_eq!(
                hex::encode(skipped_picture(sequence, picture, frame_num, None, None)),
                expected
            );
        }
    }

    #[test]
    fn frame_number_wraps_and_stays_put_after_a_picture_that_is_not_a_reference() {
        let mut flush = DecoderFlush::new(&configuration(PHONE_SEQUENCE, PHONE_PICTURE));
        assert_eq!(
            pictures(&mut flush, &["419be0"]).expect("followers"),
            ["0000000501e0400bbb"; 3]
        );
        assert_eq!(
            pictures(&mut flush, &["019be0"]).expect("followers"),
            ["0000000501fe400bbb"; 3]
        );
    }

    #[test]
    fn parameter_sets_inside_a_frame_replace_the_configured_ones() {
        let mut flush = DecoderFlush::new(&configuration(X264_SEQUENCE, X264_PICTURE));
        assert_eq!(
            pictures(&mut flush, &[PHONE_SEQUENCE, PHONE_PICTURE, "6588840a7c42"])
                .expect("followers"),
            ["0000000501e2400bbb"; 3]
        );
    }

    #[test]
    fn streams_that_cannot_be_flushed_are_left_alone() {
        let key_frame = ["6588840a7c4222a20c085f87"];
        let mut five_references = DecoderFlush::new(&configuration(
            "6742001fd30320f69a80808083c50aa8",
            PHONE_PICTURE,
        ));
        assert_eq!(pictures(&mut five_references, &key_frame), None);
        let mut supported = DecoderFlush::new(&configuration(PHONE_SEQUENCE, PHONE_PICTURE));
        assert_eq!(pictures(&mut supported, &[PHONE_PICTURE]), None);
        assert_eq!(pictures(&mut supported, &["4130"]), None);
        let mut truncated = DecoderFlush::new(&VideoCodecConfiguration {
            codec: WireVideoCodec::H264,
            codec_data: vec![1, 66, 0, 31, 0xff, 0xe1, 0, 9, 0x67],
            nal_length_bytes: 4,
        });
        assert_eq!(pictures(&mut truncated, &["419be0"]), None);
    }

    #[test]
    fn decoded_skipped_pictures_are_recognised_and_frames_are_not() {
        let mut pending = VecDeque::from([1_001, 1_002, 1_003, 2_001]);
        assert!(!take_flush_output(&mut pending, 1_000));
        assert!(take_flush_output(&mut pending, 1_001));
        assert!(take_flush_output(&mut pending, 1_003));
        assert!(!take_flush_output(&mut pending, 1_002));
        assert!(!take_flush_output(&mut pending, 2_000));
        assert_eq!(pending, VecDeque::from([2_001]));
    }

    #[test]
    fn flush_pictures_stay_within_what_the_decoder_absorbs_between_frames() {
        let start = Instant::now();
        let at = |micros: u64| start + Duration::from_micros(micros);
        let mut steady = FlushPacing::default();
        assert_eq!(steady.immediate_pictures(at(0)), usize::MAX);
        assert_eq!(steady.immediate_pictures(at(33_333)), 5);
        assert_eq!(steady.immediate_pictures(at(66_666)), 5);
        let mut fast = FlushPacing::default();
        fast.immediate_pictures(at(0));
        fast.immediate_pictures(at(16_667));
        assert_eq!(fast.immediate_pictures(at(33_333)), 2);
        assert_eq!(fast.immediate_pictures(at(50_000)), 2);
        assert_eq!(fast.immediate_pictures(at(1_050_000)), 100);
        assert_eq!(fast.immediate_pictures(at(1_051_000)), 99);
        assert_eq!(fast.immediate_pictures(at(1_052_000)), 0);
        let mut android = FlushPacing::default();
        android.immediate_pictures(at(0));
        android.immediate_pictures(at(22_000));
        assert_eq!(android.immediate_pictures(at(44_000)), 3);
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let plain = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        let escaped = with_emulation_prevention(&plain);
        assert!(escaped.windows(3).any(|window| window == [0, 0, 3]));
        assert_eq!(without_emulation_prevention(&escaped), plain);
        assert_eq!(with_emulation_prevention(&[7, 0, 0]), [7, 0, 0, 3]);
        assert_eq!(without_emulation_prevention(&[7, 0, 0, 3]), [7, 0, 0]);
    }

    fn apple_slice(
        header: u8,
        slice_type: u32,
        picture: u32,
        frame_num: u32,
        order: u32,
    ) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.unsigned_golomb(0);
        bits.unsigned_golomb(slice_type);
        bits.unsigned_golomb(picture);
        bits.bits(frame_num, 5);
        if header & 0x1f == 5 {
            bits.unsigned_golomb(0);
        }
        bits.bits(order, 6);
        let mut unit = vec![header];
        unit.extend(with_emulation_prevention(&bits.bytes));
        unit.extend_from_slice(&[0, 1, 0x4b, 0xff]);
        unit
    }

    fn order_of(frame: &EncryptedVideoFrame, sequence: &str) -> u32 {
        let sequence =
            sequence_parameters(&hex::decode(sequence).expect("hex")).expect("sequence parameters");
        let descriptor = frame
            .nal_units
            .iter()
            .find(|descriptor| matches!(descriptor.unit_type, 1 | 5))
            .expect("slice");
        let unit = nal_unit(&frame.data, descriptor).expect("unit");
        let start = slice_start(unit, sequence).expect("slice start");
        let payload = without_emulation_prevention(&unit[1..]);
        let mut bits = BitReader::new(&payload);
        bits.position = start.order_position;
        bits.bits(6).expect("picture order")
    }

    #[test]
    fn apple_high_profile_frames_are_renumbered_and_followed_by_arithmetic_pictures() {
        let mut flush = DecoderFlush::new(&configuration(APPLE_SEQUENCE, APPLE_PICTURE));
        let mut key_frame = frame(&["25b8200fffd9674d"]);
        let followers = flush.prepare(&mut key_frame).expect("followers");
        assert_eq!(key_frame, frame(&["25b8200fffd9674d"]));
        assert_eq!(
            followers
                .pictures
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>(),
            [
                "0000001101e104d700000300000300000300006f40",
                "0000001101e108d700000300000300000300006f40",
                "0000001101e10cd700000300000300000300006f40",
            ]
        );
        let mut second = frame(&["21e1044bffb8c628"]);
        let followers = flush.prepare(&mut second).expect("followers");
        assert_eq!(second, frame(&["21e1104bffb8c628"]));
        assert_eq!(
            followers
                .pictures
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>(),
            [
                "0000001101e214d700000300000300000300006f40",
                "0000001101e218d700000300000300000300006f40",
                "0000001101e21cd700000300000300000300006f40",
            ]
        );
        let mut third = frame(&["21e2087fc61bea1f"]);
        flush.prepare(&mut third).expect("followers");
        assert_eq!(third, frame(&["21e2207fc61bea1f"]));
    }

    #[test]
    fn every_reference_frame_and_weighted_prediction_are_covered() {
        let mut flush = DecoderFlush::new(&configuration(X264_HIGH_SEQUENCE, X264_HIGH_PICTURE));
        assert_eq!(
            pictures(&mut flush, &["65888437d4d44641"]).expect("followers"),
            ["0000001101e3b26500000300000300000300006f40"; 5]
        );
        assert_eq!(
            pictures(&mut flush, &["419a3b137fc284cb"]).expect("followers"),
            ["0000001101e5b26500000300000300000300006f40"; 5]
        );
    }

    #[test]
    fn picture_order_waits_for_a_key_frame_and_wraps() {
        let mut flush = DecoderFlush::new(&configuration(APPLE_SEQUENCE, APPLE_PICTURE));
        let mut early = frame_of(&[apple_slice(0x21, 0, 0, 3, 3)]);
        let untouched = early.clone();
        assert_eq!(flush.prepare(&mut early), None);
        assert_eq!(early, untouched);
        let mut key_frame = frame_of(&[apple_slice(0x25, 2, 0, 0, 9)]);
        assert_eq!(
            flush
                .prepare(&mut key_frame)
                .expect("followers")
                .pictures
                .len(),
            3
        );
        assert_eq!(order_of(&key_frame, APPLE_SEQUENCE), 0);
        for index in 1..=17_u32 {
            let mut next = frame_of(&[
                hex::decode("0605ffff").expect("hex"),
                apple_slice(0x21, 0, 0, index % 32, 16),
            ]);
            assert_eq!(
                flush.prepare(&mut next).expect("followers").pictures.len(),
                3
            );
            assert_eq!(order_of(&next, APPLE_SEQUENCE), index * 4 % 64);
            let mut cursor = 0;
            for descriptor in &next.nal_units {
                let length =
                    u32::from_be_bytes(next.data[cursor..cursor + 4].try_into().expect("length"));
                assert_eq!(descriptor.payload_offset, cursor + 4);
                assert_eq!(descriptor.payload_length, length as usize);
                cursor += 4 + descriptor.payload_length;
            }
            assert_eq!(cursor, next.data.len());
            let slice = nal_unit(&next.data, &next.nal_units[1]).expect("slice");
            assert_eq!(slice[slice.len() - 3..], [1, 0x4b, 0xff]);
            assert_eq!(slice.len(), if index == 16 { 8 } else { 7 });
            assert_eq!(without_emulation_prevention(slice)[3..], [0, 1, 0x4b, 0xff]);
        }
    }

    #[test]
    fn frames_on_other_picture_parameters_keep_the_order_without_followers() {
        let mut flush = DecoderFlush::new(&configuration(APPLE_SEQUENCE, APPLE_PICTURE));
        let mut key_frame = frame_of(&[apple_slice(0x25, 2, 0, 0, 0)]);
        assert!(flush.prepare(&mut key_frame).is_some());
        let mut other = frame_of(&[apple_slice(0x21, 0, 1, 1, 1)]);
        assert_eq!(flush.prepare(&mut other), None);
        assert_eq!(order_of(&other, APPLE_SEQUENCE), 4);
        let mut next = frame_of(&[apple_slice(0x21, 0, 0, 2, 2)]);
        assert!(flush.prepare(&mut next).is_some());
        assert_eq!(order_of(&next, APPLE_SEQUENCE), 8);
    }

    #[test]
    fn reordered_streams_are_never_touched_again() {
        let mut flush = DecoderFlush::new(&configuration(APPLE_SEQUENCE, APPLE_PICTURE));
        let mut key_frame = frame_of(&[apple_slice(0x25, 2, 0, 0, 0)]);
        assert!(flush.prepare(&mut key_frame).is_some());
        let mut bidirectional = frame_of(&[apple_slice(0x01, 1, 0, 1, 2)]);
        assert_eq!(flush.prepare(&mut bidirectional), None);
        assert_eq!(order_of(&bidirectional, APPLE_SEQUENCE), 2);
        let mut later_key_frame = frame_of(&[apple_slice(0x25, 2, 0, 0, 5)]);
        assert_eq!(flush.prepare(&mut later_key_frame), None);
        assert_eq!(order_of(&later_key_frame, APPLE_SEQUENCE), 5);
    }
}
