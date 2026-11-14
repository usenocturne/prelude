//! On-screen keyboard for text entry (Wi-Fi passwords), driven by touch or
//! the knob: turn to move between keys, press to type.

use platform_superbird::{RotaryDirection, TouchInputPhase};
use prelude_wireless::station::passphrase_is_valid;

use super::{Canvas, DISPLAY_WIDTH, FOCUS, GREEN, GROUP, GROUP_X, HEADER_HEIGHT, SECONDARY, WHITE};
use crate::fonts::FontSet;

pub(super) const MAXIMUM_LENGTH: usize = 63;
const FIELD_TOP: i32 = HEADER_HEIGHT + 4;
const FIELD_HEIGHT: i32 = 60;
const KEY_TOP: i32 = FIELD_TOP + FIELD_HEIGHT + 8;
const KEY_PITCH: i32 = 76;
const KEY_HEIGHT: i32 = 70;
const KEY_GAP: i32 = 6;
const KEY_RADIUS: i32 = 12;
const ROWS_WIDTH: i32 = DISPLAY_WIDTH as i32 - GROUP_X * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Layer {
    Lower,
    Upper,
    Digits,
    Symbols,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    Char(char),
    Shift,
    Backspace,
    Layer(Layer),
    Space,
    Reveal,
    Join,
}

impl Key {
    const fn weight(self) -> i32 {
        match self {
            Self::Char(_) => 2,
            Self::Shift | Self::Backspace => 3,
            Self::Layer(_) => 3,
            Self::Space => 8,
            Self::Reveal => 4,
            Self::Join => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KeyboardAction {
    None,
    Submit(String),
}

const LOWER: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const UPPER: [&str; 3] = ["QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"];
const DIGITS: [&str; 3] = ["1234567890", "-/:;()$&@\"", ".,?!'"];
const SYMBOLS: [&str; 3] = ["[]{}#%^*+=", "_\\|~<>`", ".,?!'"];

#[derive(Debug)]
pub(super) struct Keyboard {
    pub(super) title: String,
    text: String,
    pub(super) layer: Layer,
    pub(super) focus: usize,
    revealed: bool,
    pressed: Option<usize>,
}

impl Keyboard {
    pub(super) fn new(title: String) -> Self {
        Self {
            title,
            text: String::new(),
            layer: Layer::Lower,
            focus: 0,
            revealed: false,
            pressed: None,
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn can_submit(&self) -> bool {
        passphrase_is_valid(&self.text)
    }

    pub(super) fn field_text(&self) -> String {
        if self.revealed {
            self.text.clone()
        } else {
            "•".repeat(self.text.chars().count())
        }
    }

    fn rows(&self) -> Vec<Vec<Key>> {
        let (characters, shift_slot, layer_key) = match self.layer {
            Layer::Lower => (LOWER, Key::Shift, Key::Layer(Layer::Digits)),
            Layer::Upper => (UPPER, Key::Shift, Key::Layer(Layer::Digits)),
            Layer::Digits => (DIGITS, Key::Layer(Layer::Symbols), Key::Layer(Layer::Lower)),
            Layer::Symbols => (SYMBOLS, Key::Layer(Layer::Digits), Key::Layer(Layer::Lower)),
        };
        let keys = |row: &str| row.chars().map(Key::Char).collect::<Vec<_>>();
        let mut third = vec![shift_slot];
        third.extend(keys(characters[2]));
        third.push(Key::Backspace);
        vec![
            keys(characters[0]),
            keys(characters[1]),
            third,
            vec![layer_key, Key::Space, Key::Reveal, Key::Join],
        ]
    }

    /// Every key of the current layer in reading order (the knob's order).
    pub(super) fn keys(&self) -> Vec<Key> {
        self.rows().into_iter().flatten().collect()
    }

    /// `(x, y, width, height)` of key `index`. Rows share the full width in
    /// proportion to their keys' weights.
    pub(super) fn key_rect(&self, index: usize) -> (i32, i32, i32, i32) {
        let mut remaining = index;
        for (row_index, row) in self.rows().iter().enumerate() {
            if remaining >= row.len() {
                remaining -= row.len();
                continue;
            }
            let total: i32 = row.iter().map(|key| key.weight()).sum();
            let before: i32 = row[..remaining].iter().map(|key| key.weight()).sum();
            let start = GROUP_X + ROWS_WIDTH * before / total;
            let end = GROUP_X + ROWS_WIDTH * (before + row[remaining].weight()) / total;
            let top = KEY_TOP + KEY_PITCH * row_index as i32;
            return (start + KEY_GAP / 2, top, end - start - KEY_GAP, KEY_HEIGHT);
        }
        (0, 0, 0, 0)
    }

    fn key_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..self.keys().len()).find(|index| {
            let (left, top, width, height) = self.key_rect(*index);
            (left..left + width).contains(&x) && (top..top + height).contains(&y)
        })
    }

    pub(super) fn rotary(&mut self, direction: RotaryDirection, steps: u8) {
        let count = self.keys().len();
        let steps = usize::from(steps) % count;
        self.focus = match direction {
            RotaryDirection::Clockwise => (self.focus + steps) % count,
            RotaryDirection::CounterClockwise => (self.focus + count - steps) % count,
        };
    }

    pub(super) fn select(&mut self) -> KeyboardAction {
        let keys = self.keys();
        let Some(key) = keys.get(self.focus).copied() else {
            return KeyboardAction::None;
        };
        self.press(key)
    }

    pub(super) fn touch(&mut self, phase: TouchInputPhase, x: i32, y: i32) -> KeyboardAction {
        match phase {
            TouchInputPhase::Down => {
                self.pressed = self.key_at(x, y);
                if let Some(index) = self.pressed {
                    self.focus = index;
                }
                KeyboardAction::None
            }
            TouchInputPhase::Move => KeyboardAction::None,
            TouchInputPhase::Up => {
                let pressed = self.pressed.take();
                if pressed.is_some() && pressed == self.key_at(x, y) {
                    self.select()
                } else {
                    KeyboardAction::None
                }
            }
        }
    }

    fn press(&mut self, key: Key) -> KeyboardAction {
        match key {
            Key::Char(character) => {
                if self.text.len() < MAXIMUM_LENGTH {
                    self.text.push(character);
                }
                if self.layer == Layer::Upper {
                    self.switch(Layer::Lower);
                }
            }
            Key::Space => {
                if self.text.len() < MAXIMUM_LENGTH {
                    self.text.push(' ');
                }
            }
            Key::Backspace => {
                self.text.pop();
            }
            Key::Shift => self.switch(if self.layer == Layer::Upper {
                Layer::Lower
            } else {
                Layer::Upper
            }),
            Key::Layer(layer) => self.switch(layer),
            Key::Reveal => self.revealed = !self.revealed,
            Key::Join if self.can_submit() => return KeyboardAction::Submit(self.text.clone()),
            Key::Join => {}
        }
        KeyboardAction::None
    }

    /// Keeps the focus on the same key slot where the new layer has one.
    fn switch(&mut self, layer: Layer) {
        self.layer = layer;
        self.focus = self.focus.min(self.keys().len() - 1);
    }

    pub(super) fn render(&self, canvas: &mut Canvas, fonts: &FontSet) {
        canvas.rounded_rect(
            GROUP_X,
            FIELD_TOP,
            ROWS_WIDTH,
            FIELD_HEIGHT,
            KEY_RADIUS,
            GROUP,
            255,
        );
        let field_y = FIELD_TOP + FIELD_HEIGHT / 2;
        if self.text.is_empty() {
            canvas.text(
                fonts,
                "Password",
                GROUP_X + 20,
                field_y,
                26.0,
                SECONDARY,
                false,
            );
        } else {
            canvas.text(
                fonts,
                &self.field_text(),
                GROUP_X + 20,
                field_y,
                26.0,
                WHITE,
                false,
            );
        }
        for (index, key) in self.keys().into_iter().enumerate() {
            let (x, y, width, height) = self.key_rect(index);
            let focused = index == self.focus;
            let (fill, label_color) = match key {
                Key::Join if self.can_submit() => (GREEN, [0, 0, 0]),
                Key::Join => (GROUP, SECONDARY),
                _ if focused => (FOCUS, WHITE),
                _ => (GROUP, WHITE),
            };
            canvas.rounded_rect(x, y, width, height, KEY_RADIUS, fill, 255);
            if focused {
                canvas.ring(x, y, width, height, KEY_RADIUS, 2.5, WHITE);
            }
            let label = match key {
                Key::Char(character) => character.to_string(),
                Key::Shift => "Shift".to_owned(),
                Key::Backspace => "Delete".to_owned(),
                Key::Layer(Layer::Digits) => "123".to_owned(),
                Key::Layer(Layer::Symbols) => "#+=".to_owned(),
                Key::Layer(Layer::Lower | Layer::Upper) => "ABC".to_owned(),
                Key::Space => "Space".to_owned(),
                Key::Reveal if self.revealed => "Hide".to_owned(),
                Key::Reveal => "Show".to_owned(),
                Key::Join => "Join".to_owned(),
            };
            let size = if matches!(key, Key::Char(_)) {
                30.0
            } else {
                24.0
            };
            canvas.text_center(
                fonts,
                &label,
                x + width / 2,
                y + height / 2,
                size,
                label_color,
                false,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_ui::DISPLAY_HEIGHT;

    fn press(keyboard: &mut Keyboard, key: Key) -> KeyboardAction {
        keyboard.focus = keyboard
            .keys()
            .iter()
            .position(|candidate| *candidate == key)
            .unwrap_or_else(|| panic!("{key:?} on layer {:?}", keyboard.layer));
        keyboard.select()
    }

    fn type_text(keyboard: &mut Keyboard, text: &str) {
        for character in text.chars() {
            for layer in [Layer::Lower, Layer::Upper, Layer::Digits, Layer::Symbols] {
                keyboard.layer = layer;
                if keyboard.keys().contains(&Key::Char(character)) {
                    press(keyboard, Key::Char(character));
                    break;
                }
            }
        }
    }

    #[test]
    fn every_printable_ascii_is_typeable() {
        for byte in 0x20_u8..=0x7e {
            let character = char::from(byte);
            let mut keyboard = Keyboard::new("Home".to_owned());
            if character == ' ' {
                press(&mut keyboard, Key::Space);
            } else {
                type_text(&mut keyboard, &character.to_string());
            }
            assert_eq!(keyboard.text(), character.to_string(), "{character:?}");
        }
    }

    #[test]
    fn layer_keys_reach_every_layer() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        press(&mut keyboard, Key::Shift);
        assert_eq!(keyboard.layer, Layer::Upper);
        press(&mut keyboard, Key::Char('A'));
        assert_eq!(
            keyboard.layer,
            Layer::Lower,
            "shift applies to one character"
        );
        press(&mut keyboard, Key::Layer(Layer::Digits));
        assert_eq!(keyboard.layer, Layer::Digits);
        press(&mut keyboard, Key::Layer(Layer::Symbols));
        assert_eq!(keyboard.layer, Layer::Symbols);
        press(&mut keyboard, Key::Layer(Layer::Digits));
        press(&mut keyboard, Key::Layer(Layer::Lower));
        assert_eq!(keyboard.layer, Layer::Lower);
        assert_eq!(keyboard.text(), "A");
    }

    #[test]
    fn knob_walks_all_keys_and_wraps() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        let count = keyboard.keys().len();
        assert_eq!(keyboard.focus, 0);
        keyboard.rotary(RotaryDirection::Clockwise, 1);
        assert_eq!(keyboard.focus, 1);
        keyboard.rotary(RotaryDirection::CounterClockwise, 2);
        assert_eq!(keyboard.focus, count - 1);
        keyboard.rotary(RotaryDirection::Clockwise, 1);
        assert_eq!(keyboard.focus, 0);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..count {
            seen.insert(keyboard.focus);
            keyboard.rotary(RotaryDirection::Clockwise, 1);
        }
        assert_eq!(seen.len(), count);
    }

    #[test]
    fn masked_until_show() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        type_text(&mut keyboard, "abc1");
        assert_eq!(keyboard.field_text(), "••••");
        keyboard.layer = Layer::Lower;
        press(&mut keyboard, Key::Reveal);
        assert_eq!(keyboard.field_text(), "abc1");
        press(&mut keyboard, Key::Reveal);
        assert_eq!(keyboard.field_text(), "••••");
    }

    #[test]
    fn backspace_and_max_length() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        type_text(&mut keyboard, "abc");
        keyboard.layer = Layer::Lower;
        press(&mut keyboard, Key::Backspace);
        assert_eq!(keyboard.text(), "ab");
        let mut keyboard = Keyboard::new("Home".to_owned());
        type_text(&mut keyboard, &"x".repeat(70));
        assert_eq!(keyboard.text().len(), MAXIMUM_LENGTH);
        let mut empty = Keyboard::new("Home".to_owned());
        press(&mut empty, Key::Backspace);
        assert_eq!(empty.text(), "");
    }

    #[test]
    fn join_disabled_until_passphrase_valid() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        type_text(&mut keyboard, "short");
        keyboard.layer = Layer::Lower;
        assert!(!keyboard.can_submit());
        assert_eq!(press(&mut keyboard, Key::Join), KeyboardAction::None);
        type_text(&mut keyboard, "pass1");
        keyboard.layer = Layer::Lower;
        assert!(keyboard.can_submit());
        assert_eq!(
            press(&mut keyboard, Key::Join),
            KeyboardAction::Submit("shortpass1".to_owned())
        );
    }

    #[test]
    fn tap_hits_the_key_under_the_finger() {
        let mut keyboard = Keyboard::new("Home".to_owned());
        let index = keyboard
            .keys()
            .iter()
            .position(|key| *key == Key::Char('q'))
            .expect("q key");
        let (x, y, width, height) = keyboard.key_rect(index);
        let (center_x, center_y) = (x + width / 2, y + height / 2);
        assert_eq!(
            keyboard.touch(TouchInputPhase::Down, center_x, center_y),
            KeyboardAction::None
        );
        assert_eq!(keyboard.text(), "", "keys type on release");
        keyboard.touch(TouchInputPhase::Up, center_x, center_y);
        assert_eq!(keyboard.text(), "q");
        keyboard.touch(TouchInputPhase::Down, 5, 5);
        keyboard.touch(TouchInputPhase::Up, 5, 5);
        assert_eq!(keyboard.text(), "q", "taps outside the keys do nothing");
    }

    #[test]
    fn keys_are_at_least_seventy_pixels_tall_and_fit_the_screen() {
        for layer in [Layer::Lower, Layer::Upper, Layer::Digits, Layer::Symbols] {
            let mut keyboard = Keyboard::new("Home".to_owned());
            keyboard.layer = layer;
            for index in 0..keyboard.keys().len() {
                let (x, y, width, height) = keyboard.key_rect(index);
                assert!(height >= 70, "{layer:?} key {index}");
                assert!(width >= 56, "{layer:?} key {index}");
                assert!(x >= 0 && x + width <= DISPLAY_WIDTH as i32);
                assert!(y >= HEADER_HEIGHT && y + height <= DISPLAY_HEIGHT as i32);
            }
        }
    }
}
