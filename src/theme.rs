use gpui::{Rgba, rgb};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl ThemeMode {
    fn rgb(value: u32) -> Rgba {
        rgb(value)
    }
    fn choose(self, dark: u32, light: u32) -> Rgba {
        Self::rgb(if self == Self::Dark { dark } else { light })
    }
    pub fn background(self) -> Rgba {
        self.choose(0x181A20, 0xFFFFFF)
    }
    pub fn foreground(self) -> Rgba {
        self.choose(0xABB2BF, 0x24292F)
    }
    pub fn surface(self) -> Rgba {
        self.choose(0x21252B, 0xF3F4F6)
    }
    pub fn border(self) -> Rgba {
        self.choose(0x363C46, 0xD0D7DE)
    }
    pub fn accent(self) -> Rgba {
        self.choose(0x61AFEF, 0x0969DA)
    }
    pub fn selection(self) -> Rgba {
        self.choose(0x415A7D, 0xCBE3FF)
    }
    pub fn selection_foreground(self) -> Rgba {
        self.choose(0xFFFFFF, 0x24292F)
    }
    pub fn cursor(self) -> Rgba {
        self.choose(0xE6E6E6, 0x24292F)
    }
    pub fn palette(self, index: usize) -> Rgba {
        let colors = match self {
            Self::Dark => [
                0x181A20, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xABB2BF,
                0x5C6370, 0xFF808B, 0xB2DB93, 0xFFDA95, 0x7BC9FF, 0xE092F7, 0x70D0DC, 0xE6E6E6,
            ],
            Self::Light => [
                0x24292F, 0xA62632, 0x397300, 0x8A6100, 0x0969DA, 0x8236A0, 0x007C83, 0x57606A,
                0x6E7781, 0xCF222E, 0x1A7F37, 0x9A6700, 0x0550AE, 0x8250DF, 0x006D77, 0xFFFFFF,
            ],
        };
        Self::rgb(colors[index])
    }
}
