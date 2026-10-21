use base64::Engine;
use base64::engine::general_purpose::STANDARD;

// A picture whose every pixel is an index into its palette, as the machine's
// own numbers stand: a colour code, or a pen.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub indices: Vec<u8>,
    // Each entry as 0xRRGGBB.
    pub palette: Vec<u32>,
}

impl Picture {
    // An indexed PNG, its pixels the indices and its palette the colours, so
    // that a reader of the file gets the machine's numbers back.
    pub fn png(&self) -> Vec<u8> {
        let palette: Vec<u8> = self
            .palette
            .iter()
            .flat_map(|colour| {
                let [_, red, green, blue] = colour.to_be_bytes();
                [red, green, blue]
            })
            .collect();
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, self.width, self.height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(palette);
        encoder.set_compression(png::Compression::Balanced);
        let mut writer = encoder
            .write_header()
            .expect("a picture's header is written to memory");
        writer
            .write_image_data(&self.indices)
            .expect("a picture holds an index for each of its pixels");
        writer.finish().expect("a picture ends in memory");
        png
    }
}

// What the protocol carries an image's bytes as: "The base64-encoded image
// data" (2026-07-28, schema, ImageContent).
pub fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    // RFC 4648's test vectors for base 64, section 10:
    // https://www.rfc-editor.org/rfc/rfc4648#section-10
    #[test]
    fn bytes_are_carried_as_rfc_4648_base64() {
        for (bytes, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(bytes.as_bytes()), encoded);
        }
        // The alphabet's last two characters, which its URL-safe variant
        // replaces (section 5).
        assert_eq!(base64(&[0xFB, 0xFF]), "+/8=");
    }

    // What the png crate makes of one picture, fixed by its hash, so that an
    // upgrade that writes another file fails here. The file was read back
    // chunk by chunk with Python's zlib where this was written: an IHDR of
    // 3 by 2 at eight bits, colour type 3, a PLTE of these three colours, and
    // an IDAT whose rows, unfiltered, are these indices.
    #[test]
    fn a_picture_is_written_as_the_same_png() {
        let picture = Picture {
            width: 3,
            height: 2,
            indices: vec![0, 1, 2, 2, 1, 0],
            palette: vec![0x000000, 0xFF8000, 0x0080FF],
        };
        let digest: String = Sha256::digest(picture.png())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            digest,
            "54882dfd17bee3a53456e9f920434344b8655c91ddb6f0405c85f61d26f63c78"
        );
    }
}
