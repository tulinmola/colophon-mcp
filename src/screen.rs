use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::emulator::cpc_t;
use crate::machine::gate_array_rgb;
use crate::picture::Picture;

// Where the bytes of a picture are, by the names colophon-player's
// <colophon-cpc-screen> gives its readings.
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Reading {
    /// As the CPC's video hardware reads memory: the base is a display start, a character row's lines lie 2K apart, and a row's bytes wrap inside their 2K, unless the 6845's start, read from R12 and R13 when no base is given, carries them on into the next 16K.
    Video,
    /// A flat buffer: width bytes, then the next width, wrapping at the end of the machine's RAM.
    Linear,
    /// Each column of the picture down before the next, wrapping as linear does.
    Columns,
}

// What a screen is asked for; whatever is not given is read from the machine.
pub struct Asked {
    pub reading: Reading,
    pub base: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub rasters: Option<u32>,
    pub mode: Option<u8>,
    pub palette: Option<[u8; 16]>,
}

// What a reading is drawn from, every part of it settled.
pub struct Screen {
    pub reading: Reading,
    pub base: u32,
    // The video reading's first character as the 6845 counts it, MA13 to
    // MA0, which carries MA11 and MA10 where the base, an address, cannot.
    pub start: u32,
    pub width: u32,
    pub height: u32,
    pub rasters: u32,
    pub mode: u8,
    pub palette: [u8; 16],
}

// The video hardware reads the base 64K and nothing else, whatever the
// processor is looking at: cpc_tick in the emulator's cpc.c.
pub const VIDEO_SPACE: u32 = 0x10000;

// A sample is half a mode 1 pixel, and a byte is eight samples:
// GATE_ARRAY_SAMPLES_PER_CHARACTER in the emulator's gate_array.h, sixteen to
// a character of two bytes. The picture keeps every sample across and draws
// each line twice, as the emulator's command line draws a CPC's screenshots.
pub const PIXELS_PER_BYTE: u32 = 8;
pub const PIXELS_PER_LINE: u32 = 2;

// The bits of a video base that carry the raster line, A13 to A11.
const RASTER_BITS: u32 = 0x3800;

// 1024 pixels a side at most: an image costs a visual token for each 28 by 28
// pixels, and is scaled down past a long edge of 1568 pixels or 1568 visual
// tokens on the models with the smaller limits, so a square of 1024, at 1369,
// reaches every model whole: "Resolution and token cost",
// https://platform.claude.com/docs/en/build-with-claude/vision
pub const MOST_WIDTH: u32 = 128;
pub const MOST_HEIGHT: u32 = 512;
// R9, the Maximum Raster Address, keeps five bits (writable_bits in the
// emulator's crtc.c), so a character row has at most 32 lines.
pub const MOST_RASTERS: u32 = 32;

// A count read from the machine, held to what a screen reads.
fn bounded(name: &str, value: u32, most: u32, because: &str) -> Result<u32, String> {
    if (1..=most).contains(&value) {
        return Ok(value);
    }
    Err(format!(
        "{because}, and a screen reads from 1 to {most}; give {name}"
    ))
}

// The first character of a video base, as the 6845 would count it: the base
// holds MA13 and MA12 and MA9 to MA0, and nothing of MA11 and MA10.
fn character_of(base: u32) -> u32 {
    ((base & 0xC000) >> 2) | ((base & 0x7FE) >> 1)
}

// What was asked, with the rest read from the machine as it stands: the
// geometry from the 6845, as colophon-archive's Cpc.video() reads it, and
// the mode in force and the inks from the Gate Array. What is given is held
// to its bounds by whoever gives it, save a base, which only the machine's
// RAM bounds.
pub fn settle(asked: &Asked, cpc: &cpc_t) -> Result<Screen, String> {
    let registers = &cpc.crtc.registers;
    let video = asked.reading == Reading::Video;
    if !video && asked.rasters.is_some() {
        return Err(
            "rasters is read by the video reading alone: a flat buffer or one in columns has \
             rows of one line; give it with the video reading, or leave it out"
                .to_owned(),
        );
    }
    let rasters = asked.rasters.unwrap_or(u32::from(registers[9]) + 1);
    let width = match asked.width {
        Some(width) => width,
        None => {
            let bytes = 2 * u32::from(registers[1]);
            let because = format!("R1 is {}, which gives a width of {bytes}", registers[1]);
            bounded("width", bytes, MOST_WIDTH, &because)?
        }
    };
    let height = match asked.height {
        Some(height) => height,
        None => {
            let lines = u32::from(registers[6]) * rasters;
            let because = format!(
                "R6 is {}, which with rasters of {rasters} gives a height of {lines}",
                registers[6]
            );
            bounded("height", lines, MOST_HEIGHT, &because)?
        }
    };
    let (base, start) = match asked.base {
        Some(base) if video && (base >= VIDEO_SPACE || base & RASTER_BITS != 0) => {
            return Err(format!(
                "&{base:04X} is no display start: the video reading reads the base 64K, and \
                 there A13 to A11 carry the raster line, so a base is &0000 to &07FF, &4000 to \
                 &47FF, &8000 to &87FF or &C000 to &C7FF; linear and columns read the rest"
            ));
        }
        Some(base) if base >= cpc.ram_size => {
            return Err(format!(
                "&{base:04X} is past the RAM, which ends at &{:04X}",
                cpc.ram_size - 1
            ));
        }
        Some(base) => (base, character_of(base)),
        // R12 and R13, the 6845's start address, its count of the first
        // character in fourteen bits; the base is the address the board
        // wires that to, as colophon-archive's archive.c reads it.
        None => {
            let start = (u32::from(registers[12]) << 8 | u32::from(registers[13])) & 0x3FFF;
            (video_address(start, 0), start)
        }
    };
    // The mode and the inks as the Gate Array draws them: decode_pens reads
    // every mode past 2 as mode 3, and gate_array_rgb a colour code by its
    // lowest five bits.
    let mut palette = [0; 16];
    for (code, ink) in palette.iter_mut().zip(cpc.gate_array.inks) {
        *code = ink & 0x1F;
    }
    Ok(Screen {
        reading: asked.reading,
        base,
        start,
        width,
        height,
        rasters,
        mode: asked.mode.unwrap_or(cpc.gate_array.mode.min(3)),
        palette: asked.palette.unwrap_or(palette),
    })
}

// The board's wiring of the 6845's address lines, cpc_video_address in the
// emulator's cpc.c, after Kevin Thacker's "Screen memory addressess"
// (https://cpctech.cpcwiki.de/docs/scraddr.html): MA9..MA0 land on A10..A1,
// RA2..RA0 on A13..A11 and MA13..MA12 on A15..A14, and MA11 and MA10 reach
// nothing. A0 is the byte of the pair a character is fetched as.
fn video_address(ma: u32, raster: u32) -> u32 {
    ((ma & 0x3000) << 2) | ((raster & 7) << 11) | ((ma & 0x3FF) << 1)
}

// Each byte's address, line by line, as colophon-player's src/js/emulator/
// cpc_video.js and colophon-archive's src/js/machines/cpc_video.js both read
// them, save that the video reading counts from the 6845's own start, MA11
// and MA10 and all, where theirs count from an address.
fn addresses(screen: &Screen, ram_size: u32) -> Vec<u32> {
    let (width, height) = (screen.width, screen.height);
    let mut addresses = Vec::with_capacity((width * height) as usize);
    for line in 0..height {
        for column in 0..width {
            addresses.push(match screen.reading {
                Reading::Video => {
                    let offset = (screen.base & 1) + line / screen.rasters * width + column;
                    let ma = (screen.start + (offset >> 1)) & 0x3FFF;
                    video_address(ma, line % screen.rasters) | (offset & 1)
                }
                Reading::Linear => (screen.base + line * width + column) % ram_size,
                Reading::Columns => (screen.base + column * height + line) % ram_size,
            });
        }
    }
    addresses
}

// A byte's pens from left to right, as decode_pens in the emulator's
// gate_array.c reads them, after "The Gate Array" (Grim), video mode: mode 0
// two of sixteen, mode 1 four of four, mode 2 eight of two, and mode 3, which
// the firmware does not offer, two of four from mode 0's places.
fn pens(mode: u8, byte: u8) -> Vec<u8> {
    let bit = |place: u8| (byte >> place) & 1;
    match mode {
        0 => vec![
            bit(7) | bit(3) << 1 | bit(5) << 2 | bit(1) << 3,
            bit(6) | bit(2) << 1 | bit(4) << 2 | bit(0) << 3,
        ],
        1 => (0..4)
            .map(|pixel| bit(7 - pixel) | bit(3 - pixel) << 1)
            .collect(),
        2 => (0..8).map(|pixel| bit(7 - pixel)).collect(),
        _ => vec![bit(7) | bit(3) << 1, bit(6) | bit(2) << 1],
    }
}

// Memory read as a screen: each pixel's index is its pen, and the palette the
// colours of the colour codes the pens are given.
pub fn draw(screen: &Screen, ram: &[u8]) -> Picture {
    let ram_size = u32::try_from(ram.len()).expect("a CPC's RAM is at most 128K");
    let width = screen.width * PIXELS_PER_BYTE;
    let mut indices = Vec::with_capacity((width * screen.height * PIXELS_PER_LINE) as usize);
    for line in addresses(screen, ram_size).chunks(screen.width as usize) {
        let drawn: Vec<u8> = line
            .iter()
            .flat_map(|&address| {
                let pens = pens(screen.mode, ram[address as usize]);
                let wide = PIXELS_PER_BYTE as usize / pens.len();
                pens.into_iter()
                    .flat_map(move |pen| std::iter::repeat_n(pen, wide))
            })
            .collect();
        for _ in 0..PIXELS_PER_LINE {
            indices.extend(&drawn);
        }
    }
    Picture {
        width,
        height: screen.height * PIXELS_PER_LINE,
        indices,
        palette: screen
            .palette
            .iter()
            .map(|&code| gate_array_rgb(code))
            .collect(),
    }
}
