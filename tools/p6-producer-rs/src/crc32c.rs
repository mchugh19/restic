//! CRC-32C (Castagnoli) in the Linux kernel's crc32c() convention:
//! the running value starts at the caller's seed and is returned
//! without a final complement. The btrfs send stream checksum is
//! crc32c(0, header-with-zeroed-crc || payload); verified against
//! real streams from btrfs-progs 6.6.3 / kernel 6.17 (every command
//! in two captured streams checks out, including the constant
//! end-of-stream command whose crc is 0x9dc9_6c50).

const POLY: u32 = 0x82F6_3B78; // reflected CRC-32C polynomial

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    pub fn new() -> Self {
        Crc32c { state: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.state;
        for &b in data {
            c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.state = c;
    }

    pub fn finish(&self) -> u32 {
        self.state
    }
}

#[allow(dead_code)] // one-shot convenience used by tests and recon
pub fn crc32c(data: &[u8]) -> u32 {
    let mut h = Crc32c::new();
    h.update(data);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_command_crc_is_the_known_constant() {
        // The send stream terminator is a command with len=0, cmd=21
        // whose stored crc is identical in every observed stream. It
        // is crc32c(0, hdr) with the crc field zeroed.
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&0u32.to_le_bytes()); // len
        hdr.extend_from_slice(&21u16.to_le_bytes()); // cmd = END
        hdr.extend_from_slice(&0u32.to_le_bytes()); // crc, zeroed
        assert_eq!(crc32c(&hdr), 0x9dc9_6c50);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut h = Crc32c::new();
        for chunk in data.chunks(13) {
            h.update(chunk);
        }
        assert_eq!(h.finish(), crc32c(&data));
    }
}
