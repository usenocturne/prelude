use std::{f64::consts::PI, fmt, sync::OnceLock};

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SAMPLES: usize = SUBBANDS * BLOCKS;
pub const PACKET_BYTES: usize = 60;

const SUBBANDS: usize = 8;
const BLOCKS: usize = 15;
const BITPOOL: i32 = 26;
const FRAME_BYTES: usize = 57;
const SYNCWORD: u8 = 0xad;
const H2_SYNC: u8 = 0x01;
const H2_SEQUENCE: [u8; 4] = [0x08, 0x38, 0xc8, 0xf8];
const H2_HEADER_BYTES: usize = 2;
const SCALE_FACTOR_BITS: u32 = 4;
const MAXIMUM_SCALE_FACTOR: u8 = 15;
const MAXIMUM_SUBBAND_BITS: i32 = 16;
const LOUDNESS_OFFSETS: [i32; SUBBANDS] = [-2, 0, 0, 0, 0, 0, 0, 1];
const ANALYSIS_TAPS: usize = 80;
const SYNTHESIS_TAPS: usize = 160;
const SYNTHESIS_GAIN: f64 = -8.0;
const PROTOTYPE: [f64; ANALYSIS_TAPS] = [
    0.0,
    1.56575398e-4,
    3.43256425e-4,
    5.54620202e-4,
    8.23919506e-4,
    1.13992507e-3,
    1.47640169e-3,
    1.78371725e-3,
    2.01182542e-3,
    2.10371989e-3,
    1.99454554e-3,
    1.61656283e-3,
    9.02154502e-4,
    -1.78805361e-4,
    -1.64973098e-3,
    -3.49717454e-3,
    5.65949473e-3,
    8.02941163e-3,
    1.04584443e-2,
    1.27472335e-2,
    1.46525263e-2,
    1.59045603e-2,
    1.62208471e-2,
    1.53184106e-2,
    1.29371806e-2,
    8.85757540e-3,
    2.92408442e-3,
    -4.91578024e-3,
    -1.46404076e-2,
    -2.61098752e-2,
    -3.90751381e-2,
    -5.31873032e-2,
    6.79989431e-2,
    8.29847578e-2,
    9.75753918e-2,
    1.11196689e-1,
    1.23264548e-1,
    1.33264415e-1,
    1.40753505e-1,
    1.45389847e-1,
    1.46955068e-1,
    1.45389847e-1,
    1.40753505e-1,
    1.33264415e-1,
    1.23264548e-1,
    1.11196689e-1,
    9.75753918e-2,
    8.29847578e-2,
    -6.79989431e-2,
    -5.31873032e-2,
    -3.90751381e-2,
    -2.61098752e-2,
    -1.46404076e-2,
    -4.91578024e-3,
    2.92408442e-3,
    8.85757540e-3,
    1.29371806e-2,
    1.53184106e-2,
    1.62208471e-2,
    1.59045603e-2,
    1.46525263e-2,
    1.27472335e-2,
    1.04584443e-2,
    8.02941163e-3,
    -5.65949473e-3,
    -3.49717454e-3,
    -1.64973098e-3,
    -1.78805361e-4,
    9.02154502e-4,
    1.61656283e-3,
    1.99454554e-3,
    2.10371989e-3,
    2.01182542e-3,
    1.78371725e-3,
    1.47640169e-3,
    1.13992507e-3,
    8.23919506e-4,
    5.54620202e-4,
    3.43256425e-4,
    1.56575398e-4,
];

type Subbands = [f64; SUBBANDS];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Sync,
    Checksum,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Sync => "the mSBC frame does not start with its syncword",
            Self::Checksum => "the mSBC frame failed its checksum",
        })
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, Clone)]
pub struct Encoder {
    history: [f64; ANALYSIS_TAPS],
    sequence: usize,
}

impl Default for Encoder {
    fn default() -> Self {
        Self {
            history: [0.0; ANALYSIS_TAPS],
            sequence: 0,
        }
    }
}

impl Encoder {
    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES]) -> [u8; PACKET_BYTES] {
        let mut blocks = [[0.0; SUBBANDS]; BLOCKS];
        for (subbands, samples) in blocks.iter_mut().zip(pcm.chunks_exact(SUBBANDS)) {
            self.analyze(samples, subbands);
        }
        let scale_factors = scale_factors(&blocks);
        let bits = allocate_bits(&scale_factors);
        let mut packet = [0; PACKET_BYTES];
        packet[0] = H2_SYNC;
        packet[1] = H2_SEQUENCE[self.sequence];
        self.sequence = (self.sequence + 1) % H2_SEQUENCE.len();
        let frame = &mut packet[H2_HEADER_BYTES..H2_HEADER_BYTES + FRAME_BYTES];
        frame[0] = SYNCWORD;
        let mut writer = BitWriter::new(&mut frame[4..]);
        for scale_factor in scale_factors {
            writer.write(u32::from(scale_factor), SCALE_FACTOR_BITS);
        }
        for subbands in &blocks {
            for ((sample, &scale_factor), &bits) in subbands.iter().zip(&scale_factors).zip(&bits) {
                if bits > 0 {
                    writer.write(quantize(*sample, scale_factor, bits), bits);
                }
            }
        }
        frame[3] = checksum(frame);
        packet
    }

    fn analyze(&mut self, samples: &[i16], subbands: &mut Subbands) {
        self.history
            .copy_within(..ANALYSIS_TAPS - SUBBANDS, SUBBANDS);
        for (slot, sample) in self.history[..SUBBANDS].iter_mut().rev().zip(samples) {
            *slot = f64::from(*sample);
        }
        let mut partial = [0.0; 2 * SUBBANDS];
        for (index, sum) in partial.iter_mut().enumerate() {
            *sum = (index..ANALYSIS_TAPS)
                .step_by(2 * SUBBANDS)
                .map(|tap| PROTOTYPE[tap] * self.history[tap])
                .sum();
        }
        for (subband, cosines) in subbands.iter_mut().zip(&cosines().analysis) {
            *subband = cosines.iter().zip(&partial).map(|(c, y)| c * y).sum();
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decoder {
    history: [f64; SYNTHESIS_TAPS],
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            history: [0.0; SYNTHESIS_TAPS],
        }
    }
}

impl Decoder {
    pub fn decode(&mut self, frame: &[u8]) -> Result<[i16; FRAME_SAMPLES], DecodeError> {
        let frame = frame.get(..FRAME_BYTES).ok_or(DecodeError::Sync)?;
        if frame[0] != SYNCWORD {
            return Err(DecodeError::Sync);
        }
        if checksum(frame) != frame[3] {
            return Err(DecodeError::Checksum);
        }
        let mut reader = BitReader::new(&frame[4..]);
        let mut scale_factors = [0; SUBBANDS];
        for scale_factor in &mut scale_factors {
            *scale_factor = reader.read(SCALE_FACTOR_BITS) as u8;
        }
        let bits = allocate_bits(&scale_factors);
        let mut pcm = [0; FRAME_SAMPLES];
        for output in pcm.chunks_exact_mut(SUBBANDS) {
            let mut subbands = [0.0; SUBBANDS];
            for ((sample, &scale_factor), &bits) in
                subbands.iter_mut().zip(&scale_factors).zip(&bits)
            {
                if bits > 0 {
                    *sample = dequantize(reader.read(bits), scale_factor, bits);
                }
            }
            self.synthesize(&subbands, output);
        }
        Ok(pcm)
    }

    pub fn conceal(&mut self) -> [i16; FRAME_SAMPLES] {
        let mut pcm = [0; FRAME_SAMPLES];
        for output in pcm.chunks_exact_mut(SUBBANDS) {
            self.synthesize(&[0.0; SUBBANDS], output);
        }
        pcm
    }

    fn synthesize(&mut self, subbands: &Subbands, output: &mut [i16]) {
        self.history
            .copy_within(..SYNTHESIS_TAPS - 2 * SUBBANDS, 2 * SUBBANDS);
        for (value, cosines) in self.history.iter_mut().zip(&cosines().synthesis) {
            *value = cosines.iter().zip(subbands).map(|(c, s)| c * s).sum();
        }
        for (index, sample) in output.iter_mut().enumerate() {
            let sum: f64 = (0..5)
                .map(|group| {
                    let window = 2 * SUBBANDS * group + index;
                    let vector = 4 * SUBBANDS * group + index;
                    self.history[vector] * PROTOTYPE[window]
                        + self.history[vector + 3 * SUBBANDS] * PROTOTYPE[window + SUBBANDS]
                })
                .sum();
            *sample = (sum * SYNTHESIS_GAIN)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        }
    }
}

#[derive(Debug, Default)]
pub struct Receiver {
    decoder: Decoder,
    pending: Vec<u8>,
    skipped: usize,
    concealed_in_gap: usize,
    sequence: Option<usize>,
    decoded: u64,
    concealed: u64,
}

impl Receiver {
    pub fn push(&mut self, bytes: &[u8], pcm: &mut Vec<i16>) {
        self.pending.extend_from_slice(bytes);
        let mut offset = 0;
        while self.pending.len() - offset >= PACKET_BYTES {
            let packet = &self.pending[offset..offset + PACKET_BYTES];
            let Some(sequence) = packet_sequence(packet) else {
                let step = self.pending[offset + 1..]
                    .windows(3)
                    .position(|window| packet_sequence(window).is_some())
                    .map_or(self.pending.len() - offset - 2, |start| start + 1);
                offset += step;
                self.skip(step, pcm);
                continue;
            };
            let frame = &packet[H2_HEADER_BYTES..H2_HEADER_BYTES + FRAME_BYTES];
            let decoded = self.decoder.decode(frame);
            let lost_in_gap = self.concealed_in_gap + usize::from(self.skipped >= PACKET_BYTES / 2);
            let lost_in_sequence = self.sequence.map_or(0, |previous| {
                (sequence + 2 * H2_SEQUENCE.len() - previous - 1) % H2_SEQUENCE.len()
            });
            for _ in self.concealed_in_gap..lost_in_gap.max(lost_in_sequence) {
                self.conceal(pcm);
            }
            match decoded {
                Ok(frame) => {
                    pcm.extend_from_slice(&frame);
                    self.decoded += 1;
                }
                Err(_) => self.conceal(pcm),
            }
            self.sequence = Some(sequence);
            self.skipped = 0;
            self.concealed_in_gap = 0;
            offset += PACKET_BYTES;
        }
        self.pending.drain(..offset);
    }

    pub const fn frames_decoded(&self) -> u64 {
        self.decoded
    }

    pub const fn frames_concealed(&self) -> u64 {
        self.concealed
    }

    fn skip(&mut self, bytes: usize, pcm: &mut Vec<i16>) {
        self.skipped += bytes;
        while self.skipped >= PACKET_BYTES {
            self.skipped -= PACKET_BYTES;
            self.concealed_in_gap += 1;
            self.conceal(pcm);
        }
    }

    fn conceal(&mut self, pcm: &mut Vec<i16>) {
        pcm.extend_from_slice(&self.decoder.conceal());
        self.concealed += 1;
    }
}

fn packet_sequence(bytes: &[u8]) -> Option<usize> {
    match bytes {
        [H2_SYNC, sequence, SYNCWORD, ..] => H2_SEQUENCE.iter().position(|value| value == sequence),
        _ => None,
    }
}

struct Cosines {
    analysis: [[f64; 2 * SUBBANDS]; SUBBANDS],
    synthesis: [[f64; SUBBANDS]; 2 * SUBBANDS],
}

fn cosines() -> &'static Cosines {
    static COSINES: OnceLock<Cosines> = OnceLock::new();
    COSINES.get_or_init(|| {
        let cosine = |subband: usize, offset: f64| {
            ((subband as f64 + 0.5) * offset * PI / SUBBANDS as f64).cos()
        };
        let mut cosines = Cosines {
            analysis: [[0.0; 2 * SUBBANDS]; SUBBANDS],
            synthesis: [[0.0; SUBBANDS]; 2 * SUBBANDS],
        };
        for (subband, row) in cosines.analysis.iter_mut().enumerate() {
            for (index, value) in row.iter_mut().enumerate() {
                *value = cosine(subband, index as f64 - 4.0);
            }
        }
        for (index, row) in cosines.synthesis.iter_mut().enumerate() {
            for (subband, value) in row.iter_mut().enumerate() {
                *value = cosine(subband, index as f64 + 4.0);
            }
        }
        cosines
    })
}

fn scale_factors(blocks: &[Subbands; BLOCKS]) -> [u8; SUBBANDS] {
    let mut scale_factors = [0; SUBBANDS];
    for (subband, scale_factor) in scale_factors.iter_mut().enumerate() {
        let peak = blocks
            .iter()
            .map(|subbands| subbands[subband].abs())
            .fold(0.0, f64::max);
        while *scale_factor < MAXIMUM_SCALE_FACTOR && peak > scale(*scale_factor) {
            *scale_factor += 1;
        }
    }
    scale_factors
}

fn scale(scale_factor: u8) -> f64 {
    f64::from(2_u32 << scale_factor)
}

fn allocate_bits(scale_factors: &[u8; SUBBANDS]) -> [u32; SUBBANDS] {
    let mut need = [0; SUBBANDS];
    for ((need, &scale_factor), offset) in need.iter_mut().zip(scale_factors).zip(LOUDNESS_OFFSETS)
    {
        *need = if scale_factor == 0 {
            -5
        } else {
            let loudness = i32::from(scale_factor) - offset;
            if loudness > 0 { loudness / 2 } else { loudness }
        };
    }
    let mut slice = need.iter().copied().max().unwrap_or_default() + 1;
    let mut count = 0;
    let mut slice_count = 0;
    loop {
        slice -= 1;
        count += slice_count;
        slice_count = need
            .iter()
            .map(|&need| {
                if need > slice + 1 && need < slice + MAXIMUM_SUBBAND_BITS {
                    1
                } else if need == slice + 1 {
                    2
                } else {
                    0
                }
            })
            .sum();
        if count + slice_count >= BITPOOL {
            break;
        }
    }
    if count + slice_count == BITPOOL {
        count += slice_count;
        slice -= 1;
    }
    let mut bits = need.map(|need| {
        if need < slice + 2 {
            0
        } else {
            (need - slice).min(MAXIMUM_SUBBAND_BITS)
        }
    });
    for (bits, &need) in bits.iter_mut().zip(&need) {
        if count >= BITPOOL {
            break;
        }
        if (2..MAXIMUM_SUBBAND_BITS).contains(bits) {
            *bits += 1;
            count += 1;
        } else if need == slice + 1 && BITPOOL > count + 1 {
            *bits = 2;
            count += 2;
        }
    }
    for bits in &mut bits {
        if count >= BITPOOL {
            break;
        }
        if *bits < MAXIMUM_SUBBAND_BITS {
            *bits += 1;
            count += 1;
        }
    }
    bits.map(|bits| bits.unsigned_abs())
}

fn quantize(sample: f64, scale_factor: u8, bits: u32) -> u32 {
    let levels = f64::from((1_u32 << bits) - 1);
    ((sample / scale(scale_factor) + 1.0) * levels / 2.0)
        .floor()
        .clamp(0.0, levels) as u32
}

fn dequantize(quantized: u32, scale_factor: u8, bits: u32) -> f64 {
    let levels = f64::from((1_u32 << bits) - 1);
    scale(scale_factor) * (f64::from(2 * quantized + 1) / levels - 1.0)
}

fn checksum(frame: &[u8]) -> u8 {
    frame[1..3]
        .iter()
        .chain(&frame[4..8])
        .fold(0x0f, |mut crc, &byte| {
            crc ^= byte;
            for _ in 0..8 {
                crc = if crc & 0x80 == 0 {
                    crc << 1
                } else {
                    (crc << 1) ^ 0x1d
                };
            }
            crc
        })
}

struct BitWriter<'a> {
    bytes: &'a mut [u8],
    position: usize,
}

impl<'a> BitWriter<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn write(&mut self, value: u32, bits: u32) {
        for shift in (0..bits).rev() {
            if (value >> shift) & 1 == 1
                && let Some(byte) = self.bytes.get_mut(self.position / 8)
            {
                *byte |= 0x80 >> (self.position % 8);
            }
            self.position += 1;
        }
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read(&mut self, bits: u32) -> u32 {
        let mut value = 0;
        for _ in 0..bits {
            let bit = self
                .bytes
                .get(self.position / 8)
                .map_or(0, |byte| (byte >> (7 - self.position % 8)) & 1);
            value = (value << 1) | u32::from(bit);
            self.position += 1;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEC_DELAY: usize = 73;

    fn speech_like(frames: usize) -> Vec<i16> {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames * FRAME_SAMPLES)
            .map(|index| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let time = index as f64 / f64::from(SAMPLE_RATE);
                let voiced: f64 = (1..=20)
                    .map(|harmonic| {
                        let harmonic = f64::from(harmonic);
                        (2.0 * PI * 155.0 * harmonic * time).sin() / harmonic
                    })
                    .sum();
                let breath = f64::from((seed >> 52) as i16 - 2_048) / 4.0;
                (voiced * 6_000.0 + breath) as i16
            })
            .collect()
    }

    fn encode_all(pcm: &[i16]) -> Vec<u8> {
        let mut encoder = Encoder::default();
        pcm.chunks_exact(FRAME_SAMPLES)
            .flat_map(|frame| encoder.encode(frame.try_into().expect("whole frame")))
            .collect()
    }

    fn signal_to_noise_db(reference: &[i16], decoded: &[i16]) -> f64 {
        let (signal, noise) = reference
            .iter()
            .zip(&decoded[CODEC_DELAY..])
            .skip(FRAME_SAMPLES)
            .fold((0.0, 0.0), |(signal, noise), (&reference, &decoded)| {
                let reference = f64::from(reference);
                (
                    signal + reference * reference,
                    noise + (reference - f64::from(decoded)).powi(2),
                )
            });
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn encoded_speech_decodes_close_to_the_original() {
        let pcm = speech_like(60);
        let packets = encode_all(&pcm);
        assert_eq!(packets.len(), 60 * PACKET_BYTES);
        let mut receiver = Receiver::default();
        let mut decoded = Vec::new();
        receiver.push(&packets, &mut decoded);
        assert_eq!(decoded.len(), pcm.len());
        assert_eq!(receiver.frames_decoded(), 60);
        assert_eq!(receiver.frames_concealed(), 0);
        let snr = signal_to_noise_db(&pcm, &decoded);
        assert!(snr > 20.0, "mSBC round trip SNR {snr:.1} dB");
    }

    #[test]
    fn filterbank_alone_reconstructs_the_input() {
        let pcm = speech_like(40);
        let mut encoder = Encoder::default();
        let mut decoder = Decoder::default();
        let mut decoded = vec![0; pcm.len()];
        for (input, output) in pcm
            .chunks_exact(SUBBANDS)
            .zip(decoded.chunks_exact_mut(SUBBANDS))
        {
            let mut subbands = [0.0; SUBBANDS];
            encoder.analyze(input, &mut subbands);
            decoder.synthesize(&subbands, output);
        }
        let snr = signal_to_noise_db(&pcm, &decoded);
        assert!(snr > 60.0, "filterbank SNR {snr:.1} dB");
    }

    #[test]
    fn packets_carry_the_h2_header_and_rotate_its_sequence() {
        let mut encoder = Encoder::default();
        let silence = [0; FRAME_SAMPLES];
        let headers: Vec<_> = (0..5)
            .map(|_| {
                let packet = encoder.encode(&silence);
                assert_eq!(packet[2], SYNCWORD);
                assert_eq!(&packet[3..5], &[0, 0]);
                assert_eq!(packet[PACKET_BYTES - 1], 0);
                [packet[0], packet[1]]
            })
            .collect();
        assert_eq!(
            headers,
            [
                [0x01, 0x08],
                [0x01, 0x38],
                [0x01, 0xc8],
                [0x01, 0xf8],
                [0x01, 0x08]
            ]
        );
    }

    #[test]
    fn bit_allocation_spends_exactly_the_bitpool() {
        for scale_factors in [
            [0; SUBBANDS],
            [9, 8, 7, 6, 5, 4, 3, 2],
            [15; SUBBANDS],
            [12, 0, 0, 0, 0, 0, 0, 1],
        ] {
            let bits = allocate_bits(&scale_factors);
            assert_eq!(bits.iter().sum::<u32>(), 26, "{scale_factors:?} → {bits:?}");
            assert!(bits.iter().all(|&bits| bits <= 16));
        }
    }

    #[test]
    fn corrupted_frames_are_rejected_by_their_checksum() {
        let packet = Encoder::default().encode(&[1_000; FRAME_SAMPLES]);
        let mut frame = packet[2..2 + FRAME_BYTES].to_vec();
        assert!(Decoder::default().decode(&frame).is_ok());
        frame[5] ^= 0x10;
        assert_eq!(
            Decoder::default().decode(&frame),
            Err(DecodeError::Checksum)
        );
        frame[0] = 0x9c;
        assert_eq!(Decoder::default().decode(&frame), Err(DecodeError::Sync));
    }

    #[test]
    fn the_receiver_reassembles_packets_split_across_reads() {
        let packets = encode_all(&speech_like(8));
        let mut whole = Vec::new();
        Receiver::default().push(&packets, &mut whole);
        let mut receiver = Receiver::default();
        let mut split = Vec::new();
        for chunk in packets.chunks(17) {
            receiver.push(chunk, &mut split);
        }
        assert_eq!(split, whole);
        assert_eq!(receiver.frames_decoded(), 8);
    }

    #[test]
    fn the_receiver_finds_packets_after_leading_garbage() {
        let packets = encode_all(&speech_like(4));
        let mut stream = vec![0x55; 7];
        stream.extend_from_slice(&packets);
        let mut receiver = Receiver::default();
        let mut pcm = Vec::new();
        receiver.push(&stream, &mut pcm);
        assert_eq!(receiver.frames_decoded(), 4);
        assert_eq!(receiver.frames_concealed(), 0);
        assert_eq!(pcm.len(), 4 * FRAME_SAMPLES);
    }

    #[test]
    fn lost_and_garbled_packets_keep_the_timeline() {
        let packets = encode_all(&speech_like(8));
        let packet = |index: usize| &packets[index * PACKET_BYTES..(index + 1) * PACKET_BYTES];
        let mut stream = Vec::new();
        stream.extend_from_slice(packet(0));
        stream.extend_from_slice(packet(2));
        stream.extend_from_slice(&[0; PACKET_BYTES]);
        stream.extend_from_slice(packet(4));
        let mut garbled = packet(5).to_vec();
        garbled[30] ^= 0xff;
        garbled[6] ^= 0x01;
        stream.extend_from_slice(&garbled);
        stream.extend_from_slice(packet(6));
        let mut receiver = Receiver::default();
        let mut pcm = Vec::new();
        receiver.push(&stream, &mut pcm);
        assert_eq!(receiver.frames_decoded(), 4);
        assert_eq!(receiver.frames_concealed(), 3);
        assert_eq!(pcm.len(), 7 * FRAME_SAMPLES);
    }

    #[test]
    fn steady_garbage_is_concealed_as_it_arrives() {
        let mut receiver = Receiver::default();
        let mut pcm = Vec::new();
        receiver.push(&[0x5a; 10 * PACKET_BYTES], &mut pcm);
        assert_eq!(receiver.frames_decoded(), 0);
        assert_eq!(receiver.frames_concealed(), 9);
        assert_eq!(pcm.len(), 9 * FRAME_SAMPLES);
        assert!(pcm.iter().all(|&sample| sample == 0));
    }
}
