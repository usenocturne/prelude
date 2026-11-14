#![cfg_attr(not(test), allow(dead_code))]

pub fn force_low_latency_sps(nal: &[u8]) -> Result<Vec<u8>, String> {
    if nal.is_empty() || nal[0] & 0x1f != 7 {
        return Err("expected an H.264 SPS NAL unit".to_owned());
    }
    let header = nal[0];
    let rbsp = rbsp_from_ebsp(&nal[1..]);
    let rewritten = rewrite_sps_rbsp(&rbsp)?;
    let mut out = Vec::with_capacity(1 + rewritten.len() + rewritten.len() / 2);
    out.push(header);
    out.extend(ebsp_from_rbsp(&rewritten));
    Ok(out)
}

fn rbsp_from_ebsp(ebsp: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(ebsp.len());
    let mut index = 0;
    while index < ebsp.len() {
        if index + 2 < ebsp.len()
            && ebsp[index] == 0
            && ebsp[index + 1] == 0
            && ebsp[index + 2] == 3
            && index + 3 < ebsp.len()
            && ebsp[index + 3] <= 0x03
        {
            rbsp.push(0);
            rbsp.push(0);
            index += 3;
        } else {
            rbsp.push(ebsp[index]);
            index += 1;
        }
    }
    rbsp
}

fn ebsp_from_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut ebsp = Vec::with_capacity(rbsp.len() + rbsp.len() / 2);
    let mut zero_count = 0_u8;
    for &byte in rbsp {
        if zero_count >= 2 && byte <= 0x03 {
            ebsp.push(0x03);
            zero_count = 0;
        }
        ebsp.push(byte);
        zero_count = if byte == 0 {
            zero_count.saturating_add(1)
        } else {
            0
        };
    }
    ebsp
}

fn rewrite_sps_rbsp(rbsp: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(rbsp);
    let mut writer = BitWriter::default();

    let profile_idc = reader.read_bits(8)?;
    writer.write_bits(profile_idc, 8)?;
    writer.write_bits(reader.read_bits(8)?, 8)?;
    writer.write_bits(reader.read_bits(8)?, 8)?;
    copy_ue(&mut reader, &mut writer)?;

    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = copy_ue(&mut reader, &mut writer)?;
        if chroma_format_idc == 3 {
            writer.write_bit(reader.read_bit()?)?;
        }
        copy_ue(&mut reader, &mut writer)?;
        copy_ue(&mut reader, &mut writer)?;
        writer.write_bit(reader.read_bit()?)?;
        let seq_scaling_matrix_present = reader.read_bit()?;
        writer.write_bit(seq_scaling_matrix_present)?;
        if seq_scaling_matrix_present {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for index in 0..count {
                let present = reader.read_bit()?;
                writer.write_bit(present)?;
                if present {
                    let size = if index < 6 { 16 } else { 64 };
                    let mut last_scale = 8_i32;
                    let mut next_scale = 8_i32;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = read_se(&mut reader)?;
                            write_se(&mut writer, delta)?;
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        last_scale = if next_scale == 0 {
                            last_scale
                        } else {
                            next_scale
                        };
                    }
                }
            }
        }
    }

    copy_ue(&mut reader, &mut writer)?;
    let pic_order_cnt_type = copy_ue(&mut reader, &mut writer)?;
    if pic_order_cnt_type == 0 {
        copy_ue(&mut reader, &mut writer)?;
    } else if pic_order_cnt_type == 1 {
        writer.write_bit(reader.read_bit()?)?;
        copy_se(&mut reader, &mut writer)?;
        copy_se(&mut reader, &mut writer)?;
        let n = copy_ue(&mut reader, &mut writer)?;
        for _ in 0..n {
            copy_se(&mut reader, &mut writer)?;
        }
    }
    let max_num_ref_frames = copy_ue(&mut reader, &mut writer)?;
    writer.write_bit(reader.read_bit()?)?;
    copy_ue(&mut reader, &mut writer)?;
    copy_ue(&mut reader, &mut writer)?;
    let frame_mbs_only = reader.read_bit()?;
    writer.write_bit(frame_mbs_only)?;
    if !frame_mbs_only {
        writer.write_bit(reader.read_bit()?)?;
    }
    writer.write_bit(reader.read_bit()?)?;
    let frame_cropping = reader.read_bit()?;
    writer.write_bit(frame_cropping)?;
    if frame_cropping {
        for _ in 0..4 {
            copy_ue(&mut reader, &mut writer)?;
        }
    }

    let vui_present = reader.read_bit().unwrap_or(false);
    writer.write_bit(true)?;
    if vui_present {
        copy_vui_and_force_low_latency(&mut reader, &mut writer, max_num_ref_frames)?;
    } else {
        write_empty_vui_prefix(&mut writer)?;
        write_low_latency_bitstream_restriction(&mut writer, max_num_ref_frames)?;
    }
    writer.finish_rbsp()
}

fn copy_vui_and_force_low_latency(
    reader: &mut BitReader<'_>,
    writer: &mut BitWriter,
    max_num_ref_frames: u32,
) -> Result<(), String> {
    let aspect = reader.read_bit()?;
    writer.write_bit(aspect)?;
    if aspect {
        let idc = reader.read_bits(8)?;
        writer.write_bits(idc, 8)?;
        if idc == 255 {
            writer.write_bits(reader.read_bits(16)?, 16)?;
            writer.write_bits(reader.read_bits(16)?, 16)?;
        }
    }
    let overscan = reader.read_bit()?;
    writer.write_bit(overscan)?;
    if overscan {
        writer.write_bit(reader.read_bit()?)?;
    }
    let video_signal = reader.read_bit()?;
    writer.write_bit(video_signal)?;
    if video_signal {
        writer.write_bits(reader.read_bits(3)?, 3)?;
        let full_range = reader.read_bit()?;
        writer.write_bit(full_range)?;
        let colour = reader.read_bit()?;
        if colour {
            reader.read_bits(24)?;
        }
        writer.write_bit(true)?;
        writer.write_bits(1, 8)?;
        writer.write_bits(1, 8)?;
        writer.write_bits(1, 8)?;
    }
    let chroma_loc = reader.read_bit()?;
    writer.write_bit(chroma_loc)?;
    if chroma_loc {
        copy_ue(reader, writer)?;
        copy_ue(reader, writer)?;
    }
    let timing = reader.read_bit()?;
    writer.write_bit(timing)?;
    if timing {
        writer.write_bits(reader.read_bits(32)?, 32)?;
        writer.write_bits(reader.read_bits(32)?, 32)?;
        writer.write_bit(reader.read_bit()?)?;
    }
    let nal_hrd = reader.read_bit()?;
    writer.write_bit(nal_hrd)?;
    if nal_hrd {
        copy_hrd_parameters(reader, writer)?;
    }
    let vcl_hrd = reader.read_bit()?;
    writer.write_bit(vcl_hrd)?;
    if vcl_hrd {
        copy_hrd_parameters(reader, writer)?;
    }
    if nal_hrd || vcl_hrd {
        writer.write_bit(reader.read_bit()?)?;
    }
    writer.write_bit(reader.read_bit()?)?;
    let old_restriction = reader.read_bit().unwrap_or(false);
    if old_restriction {
        let mv_over = reader.read_bit()?;
        let bytes_per_pic = read_ue(reader)?;
        let bits_per_mb = read_ue(reader)?;
        let log2_max_mv_hor = read_ue(reader)?;
        let log2_max_mv_ver = read_ue(reader)?;
        let _old_reorder = read_ue(reader)?;
        let old_buf = read_ue(reader)?;

        writer.write_bit(true)?;
        writer.write_bit(mv_over)?;
        write_ue(writer, bytes_per_pic)?;
        write_ue(writer, bits_per_mb)?;
        write_ue(writer, log2_max_mv_hor)?;
        write_ue(writer, log2_max_mv_ver)?;
        write_ue(writer, 0)?;
        write_ue(writer, old_buf.min(max_num_ref_frames).max(1))?;
    } else {
        write_low_latency_bitstream_restriction(writer, max_num_ref_frames)?;
    }
    Ok(())
}

fn write_empty_vui_prefix(writer: &mut BitWriter) -> Result<(), String> {
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    Ok(())
}

fn write_low_latency_bitstream_restriction(
    writer: &mut BitWriter,
    max_num_ref_frames: u32,
) -> Result<(), String> {
    writer.write_bit(true)?;
    writer.write_bit(true)?;
    write_ue(writer, 0)?;
    write_ue(writer, 0)?;
    write_ue(writer, 16)?;
    write_ue(writer, 16)?;
    write_ue(writer, 0)?;
    write_ue(writer, max_num_ref_frames.max(1))?;
    Ok(())
}

fn copy_hrd_parameters(reader: &mut BitReader<'_>, writer: &mut BitWriter) -> Result<(), String> {
    let cpb_count = copy_ue(reader, writer)?;
    writer.write_bits(reader.read_bits(4)?, 4)?;
    writer.write_bits(reader.read_bits(4)?, 4)?;
    for _ in 0..=cpb_count {
        copy_ue(reader, writer)?;
        copy_ue(reader, writer)?;
        writer.write_bit(reader.read_bit()?)?;
    }
    writer.write_bits(reader.read_bits(5)?, 5)?;
    writer.write_bits(reader.read_bits(5)?, 5)?;
    writer.write_bits(reader.read_bits(5)?, 5)?;
    writer.write_bits(reader.read_bits(5)?, 5)?;
    Ok(())
}

fn copy_ue(reader: &mut BitReader<'_>, writer: &mut BitWriter) -> Result<u32, String> {
    let value = read_ue(reader)?;
    write_ue(writer, value)?;
    Ok(value)
}

fn copy_se(reader: &mut BitReader<'_>, writer: &mut BitWriter) -> Result<i32, String> {
    let value = read_se(reader)?;
    write_se(writer, value)?;
    Ok(value)
}

fn read_ue(reader: &mut BitReader<'_>) -> Result<u32, String> {
    let mut zeros = 0_u32;
    while !reader.read_bit()? {
        zeros = zeros.saturating_add(1);
        if zeros > 31 {
            return Err("exp-Golomb ue(v) overflow".to_owned());
        }
    }
    if zeros == 0 {
        return Ok(0);
    }
    let suffix = reader.read_bits(zeros)?;
    Ok(((1_u32 << zeros) - 1).saturating_add(suffix))
}

fn write_ue(writer: &mut BitWriter, value: u32) -> Result<(), String> {
    let value = u64::from(value) + 1;
    let bits = 64 - value.leading_zeros();
    let zeros = bits.saturating_sub(1);
    for _ in 0..zeros {
        writer.write_bit(false)?;
    }
    writer.write_bits(value as u32, bits)?;
    Ok(())
}

fn read_se(reader: &mut BitReader<'_>) -> Result<i32, String> {
    let code = read_ue(reader)?;
    let value = ((code + 1) >> 1) as i32;
    Ok(if code & 1 == 0 { -value } else { value })
}

fn write_se(writer: &mut BitWriter, value: i32) -> Result<(), String> {
    let code = if value > 0 {
        (value as u32) * 2 - 1
    } else {
        (-value as u32) * 2
    };
    write_ue(writer, code)
}

#[derive(Debug)]
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read_bit(&mut self) -> Result<bool, String> {
        let byte_index = self.bit / 8;
        if byte_index >= self.bytes.len() {
            return Err("unexpected end of SPS RBSP".to_owned());
        }
        let shift = 7 - (self.bit % 8);
        let bit = (self.bytes[byte_index] >> shift) & 1 == 1;
        self.bit += 1;
        Ok(bit)
    }

    fn read_bits(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0_u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Ok(value)
    }
}

#[derive(Debug, Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit: usize,
}

impl BitWriter {
    fn write_bit(&mut self, bit: bool) -> Result<(), String> {
        if self.bit % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            let byte_index = self.bit / 8;
            let shift = 7 - (self.bit % 8);
            self.bytes[byte_index] |= 1 << shift;
        }
        self.bit += 1;
        Ok(())
    }

    fn write_bits(&mut self, value: u32, count: u32) -> Result<(), String> {
        for index in (0..count).rev() {
            self.write_bit(((value >> index) & 1) == 1)?;
        }
        Ok(())
    }

    fn finish_rbsp(mut self) -> Result<Vec<u8>, String> {
        self.write_bit(true)?;
        while self.bit % 8 != 0 {
            self.write_bit(false)?;
        }
        Ok(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_sps_without_vui() -> Vec<u8> {
        let rbsp = {
            let mut w = BitWriter::default();
            w.write_bits(66, 8).expect("bits");
            w.write_bits(0b11000000, 8).expect("bits");
            w.write_bits(30, 8).expect("bits");
            write_ue(&mut w, 0).expect("bits");
            write_ue(&mut w, 0).expect("bits");
            write_ue(&mut w, 0).expect("bits");
            write_ue(&mut w, 0).expect("bits");
            write_ue(&mut w, 1).expect("bits");
            w.write_bit(false).expect("bits");
            write_ue(&mut w, 29).expect("bits");
            write_ue(&mut w, 49).expect("bits");
            w.write_bit(true).expect("bits");
            w.write_bit(true).expect("bits");
            w.write_bit(false).expect("bits");
            w.write_bit(false).expect("bits");
            w.finish_rbsp().expect("bits")
        };
        let mut nal = vec![0x67];
        nal.extend(ebsp_from_rbsp(&rbsp));
        nal
    }

    #[test]
    fn low_latency_rewrite_inserts_bitstream_restrictions() {
        let original = baseline_sps_without_vui();
        let rewritten = force_low_latency_sps(&original).expect("rewrite");
        assert_eq!(rewritten[0] & 0x1f, 7);
        assert_ne!(rewritten, original);
        assert!(
            rewritten.len() > original.len(),
            "VUI bitstream restrictions should grow the SPS"
        );

        let again = force_low_latency_sps(&rewritten).expect("second rewrite");
        assert_eq!(again[0] & 0x1f, 7);
    }

    #[test]
    fn parameter_sets_that_differ_only_in_colour_description_rewrite_identically() {
        let described = hexadecimal("6742001fda0320f69a80808083c50aa8");
        let unspecified = hexadecimal("6742001fda0320f69a81010103c50aa8");
        assert_ne!(described, unspecified);
        let rewritten = force_low_latency_sps(&described).expect("rewrite");
        assert_eq!(
            rewritten,
            force_low_latency_sps(&unspecified).expect("rewrite")
        );
        assert_eq!(
            force_low_latency_sps(&rewritten).expect("rewrite"),
            rewritten
        );
        assert_eq!(rewritten[..8], described[..8]);
        assert_eq!(rewritten, described);
    }

    fn hexadecimal(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hexadecimal"))
            .collect()
    }

    #[test]
    fn rejects_non_sps_nal() {
        assert!(force_low_latency_sps(&[0x68, 0x00]).is_err());
    }

    #[test]
    fn exp_golomb_round_trips_small_values() {
        for value in [0_u32, 1, 2, 3, 16, 49] {
            let mut writer = BitWriter::default();
            write_ue(&mut writer, value).expect("bits");
            let bytes = writer.finish_rbsp().expect("bits");
            let mut reader = BitReader::new(&bytes);
            assert_eq!(read_ue(&mut reader).expect("bits"), value);
        }
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let rbsp = vec![0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3];
        let ebsp = ebsp_from_rbsp(&rbsp);
        assert!(ebsp.windows(3).any(|window| window == [0, 0, 3]));
        assert_eq!(rbsp_from_ebsp(&ebsp), rbsp);
    }
}
