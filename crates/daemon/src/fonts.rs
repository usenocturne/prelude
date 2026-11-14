use std::{fs, os::unix::fs::FileTypeExt, path::PathBuf, sync::OnceLock};

use fontdue::{Font, FontSettings};
use tracing::warn;

const MAXIMUM_FONT_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct FontSet {
    pub(crate) regular: Option<Font>,
    pub(crate) semibold: Option<Font>,
}

pub(crate) fn shared() -> &'static FontSet {
    static FONTS: OnceLock<FontSet> = OnceLock::new();
    FONTS.get_or_init(FontSet::load)
}

impl FontSet {
    pub(crate) fn face(&self, semibold: bool) -> Option<&Font> {
        if semibold {
            self.semibold.as_ref().or(self.regular.as_ref())
        } else {
            self.regular.as_ref()
        }
    }

    pub(crate) fn text_width(&self, text: &str, size: f32, semibold: bool) -> i32 {
        if let Some(font) = self.face(semibold) {
            text.chars()
                .map(|character| font.metrics(character, size).advance_width)
                .sum::<f32>()
                .round() as i32
        } else {
            let scale = bitmap_scale(size);
            (text.chars().count() as i32 * 6 - 1).max(0) * scale
        }
    }

    pub(crate) fn fit(&self, text: &str, size: f32, semibold: bool, maximum_width: i32) -> String {
        if self.text_width(text, size, semibold) <= maximum_width {
            return text.to_owned();
        }
        let mut fitted: String = text.to_owned();
        while !fitted.is_empty() {
            fitted.pop();
            let candidate = format!("{}…", fitted.trim_end());
            if self.text_width(&candidate, size, semibold) <= maximum_width {
                return candidate;
            }
        }
        "…".to_owned()
    }

    pub(crate) fn draw(
        &self,
        text: &str,
        (x, center_y): (i32, i32),
        size: f32,
        semibold: bool,
        mut plot: impl FnMut(i32, i32, u8),
    ) {
        let Some(font) = self.face(semibold) else {
            let scale = bitmap_scale(size);
            let top = center_y - 7 * scale / 2;
            for (index, character) in text.chars().enumerate() {
                let Some(glyph) = bitmap_glyph(character) else {
                    continue;
                };
                let left = x + index as i32 * 6 * scale;
                for (row, bits) in glyph.iter().enumerate() {
                    for column in (0..5).filter(|column| bits & (1 << (4 - column)) != 0) {
                        for py in 0..scale {
                            for px in 0..scale {
                                plot(
                                    left + column * scale + px,
                                    top + row as i32 * scale + py,
                                    255,
                                );
                            }
                        }
                    }
                }
            }
            return;
        };
        let baseline = font
            .horizontal_line_metrics(size)
            .map_or(size / 2.0, |metrics| {
                (metrics.ascent + metrics.descent) / 2.0
            })
            .round() as i32
            + center_y;
        let mut cursor_x = x as f32;
        for character in text.chars() {
            let (metrics, bitmap) = font.rasterize(character, size);
            let glyph_x = cursor_x.round() as i32 + metrics.xmin;
            let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                for column in 0..metrics.width {
                    let alpha = bitmap[row * metrics.width + column];
                    if alpha != 0 {
                        plot(glyph_x + column as i32, glyph_y + row as i32, alpha);
                    }
                }
            }
            cursor_x += metrics.advance_width;
        }
    }

    pub(crate) fn load() -> Self {
        let directories = [
            "/usr/share/fonts/prelude",
            "/usr/share/fonts/nocturne",
            "/usr/share/fonts/truetype/inter",
            "/usr/share/fonts/truetype/Roboto",
            "/usr/share/fonts/truetype/roboto",
            "/usr/share/fonts/Roboto",
            "/usr/share/fonts/ttf",
            "/usr/share/fonts",
        ];
        let regular = load_font(&directories, &["Inter-Regular.ttf", "Roboto-Regular.ttf"]);
        let semibold = load_font(
            &directories,
            &["Inter-SemiBold.ttf", "Inter-Bold.ttf", "Roboto-Bold.ttf"],
        );
        if regular.is_none() {
            warn!("no Inter or Roboto font was found; using the built-in bitmap glyphs");
        }
        Self { regular, semibold }
    }
}

fn bitmap_scale(size: f32) -> i32 {
    (size / 8.0).round().max(1.0) as i32
}

fn load_font(directories: &[&str], names: &[&str]) -> Option<Font> {
    directories
        .iter()
        .flat_map(|directory| names.iter().map(move |name| (*directory, *name)))
        .find_map(|(directory, name)| {
            let path = PathBuf::from(directory).join(name);
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.file_type().is_block_device()
                || metadata.len() == 0
                || metadata.len() > MAXIMUM_FONT_BYTES
            {
                return None;
            }
            let bytes = fs::read(path).ok()?;
            Font::from_bytes(bytes, FontSettings::default()).ok()
        })
}

pub(crate) fn bitmap_glyph(character: char) -> Option<[u8; 7]> {
    let character = character.to_ascii_uppercase();
    Some(match character {
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'B' => [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e],
        'C' => [0x0f, 0x10, 0x10, 0x10, 0x10, 0x10, 0x0f],
        'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'F' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
        'G' => [0x0f, 0x10, 0x10, 0x17, 0x11, 0x11, 0x0f],
        'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'I' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1f],
        'J' => [0x01, 0x01, 0x01, 0x01, 0x11, 0x11, 0x0e],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'P' => [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
        'Q' => [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1b, 0x11],
        'X' => [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f],
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1e, 0x01, 0x01, 0x0e, 0x01, 0x01, 0x1e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x10, 0x1e, 0x01, 0x01, 0x1e],
        '6' => [0x0e, 0x10, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x01, 0x0e],
        ' ' => [0; 7],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        ':' => [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00],
        '-' | '−' => [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '+' => [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04],
        '%' => [0x19, 0x1a, 0x02, 0x04, 0x08, 0x0b, 0x13],
        '·' => [0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00],
        '•' => [0x00, 0x04, 0x0e, 0x1f, 0x0e, 0x04, 0x00],
        '‹' => [0x00, 0x02, 0x04, 0x08, 0x04, 0x02, 0x00],
        '›' => [0x00, 0x08, 0x04, 0x02, 0x04, 0x08, 0x00],
        '…' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x15],
        '\'' | '’' => [0x04, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        _ => return None,
    })
}
