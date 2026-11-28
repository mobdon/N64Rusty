//! The screens' colors, as `0xRRGGBB`.

/// The background: the palette is the console's four bright colors
/// ([`RED`], [`GOLD`], [`GREEN`], [`SKY`]) on this dark grey.
pub const BG: u32 = 0x1e1e22;
/// Panel and tab fill.
pub const PANEL: u32 = 0x3a3a40;
/// Borders, help text and unselected tabs.
pub const EDGE: u32 = 0x6a6a72;
/// Plain text.
pub const TEXT: u32 = 0xf0f0f0;
/// Highlight: the selection and setting values.
pub const GOLD: u32 = 0xffc20e;
/// One of the four bright colors; also "off" and errors.
pub const RED: u32 = 0xe4002b;
/// One of the four bright colors; also "on".
pub const GREEN: u32 = 0x00a651;
/// One of the four bright colors.
pub const SKY: u32 = 0x0a72d8;
/// The four bright colors in turn: title letters, tabs, cartridges, icon.
pub(super) const FOUR: [u32; 4] = [RED, GOLD, GREEN, SKY];
/// The fill of the selected tab or item.
pub(super) const PICKED: u32 = 0x24345c;
/// Drop shadows and the cartridge's slot.
pub(super) const SHADOW: u32 = 0x000000;
