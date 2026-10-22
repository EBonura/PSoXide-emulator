//! SHA-256 of a disc image, using the CPU's SHA-256 instructions where it has them.
//!
//! The disc fingerprint labels every report and hashes the whole image (hundreds of MB for the
//! big games). The portable `sha2` code is a few hundred MB/s; the ARMv8 SHA-256 instructions
//! are several times faster. The `sha2` crate's own aarch64 backend sits behind its `asm`
//! feature, which also pulls in a C-and-assembly crate, so the gate carries the short hardware
//! compression function itself and falls back to `sha2` anywhere the instructions are missing.

use std::io::Read;

use sha2::{Digest, Sha256};

/// SHA-256 of everything `source` yields, or the read error.
pub fn sha256_of(source: &mut impl Read) -> std::io::Result<[u8; 32]> {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        return hardware::sha256_of(source);
    }
    software_sha256_of(source)
}

fn software_sha256_of(source: &mut impl Read) -> std::io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut block = vec![0u8; READ_BYTES];
    loop {
        match source.read(&mut block)? {
            0 => return Ok(hash.finalize().into()),
            n => hash.update(&block[..n]),
        }
    }
}

/// Read size. A multiple of the 64-byte hash block.
const READ_BYTES: usize = 1024 * 1024;

#[cfg(target_arch = "aarch64")]
mod hardware {
    use std::io::Read;

    use core::arch::aarch64::{
        vaddq_u32, vld1q_u32, vld1q_u8, vreinterpretq_u32_u8, vrev32q_u8, vsha256h2q_u32,
        vsha256hq_u32, vsha256su0q_u32, vsha256su1q_u32, vst1q_u32,
    };

    use super::READ_BYTES;

    const BLOCK: usize = 64;

    /// Initial hash value (FIPS 180-4, section 5.3.3).
    const INITIAL_STATE: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    /// Round constants (FIPS 180-4, section 4.2.2).
    const ROUND_CONSTANTS: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    /// SHA-256 of everything `source` yields. The caller has checked for the `sha2` CPU feature.
    pub fn sha256_of(source: &mut impl Read) -> std::io::Result<[u8; 32]> {
        let mut state = INITIAL_STATE;
        let mut buf = vec![0u8; READ_BYTES];
        // Bytes of `buf` waiting for the rest of their 64-byte block.
        let mut held = 0;
        let mut total: u64 = 0;
        loop {
            let n = source.read(&mut buf[held..])?;
            if n == 0 {
                break;
            }
            total += n as u64;
            let filled = held + n;
            let whole = filled - filled % BLOCK;
            compress_blocks(&mut state, &buf[..whole]);
            buf.copy_within(whole..filled, 0);
            held = filled - whole;
        }
        // Padding: a 1 bit, zeros, then the message length in bits as a big-endian u64.
        let mut tail = [0u8; 2 * BLOCK];
        tail[..held].copy_from_slice(&buf[..held]);
        tail[held] = 0x80;
        let tail_len = if held + 1 + 8 <= BLOCK {
            BLOCK
        } else {
            2 * BLOCK
        };
        tail[tail_len - 8..tail_len].copy_from_slice(&(total * 8).to_be_bytes());
        compress_blocks(&mut state, &tail[..tail_len]);
        let mut digest = [0u8; 32];
        for (out, word) in digest.chunks_exact_mut(4).zip(state) {
            out.copy_from_slice(&word.to_be_bytes());
        }
        Ok(digest)
    }

    /// Run the compression function over `data`, a whole number of 64-byte blocks.
    fn compress_blocks(state: &mut [u32; 8], data: &[u8]) {
        debug_assert_eq!(data.len() % BLOCK, 0);
        // SAFETY: `sha256_of` is only reached after `is_aarch64_feature_detected!("sha2")`.
        unsafe { compress(state, data) }
    }

    /// The SHA-256 compression function over whole blocks with the ARMv8 SHA-256 instructions:
    /// each `sha256h`/`sha256h2` pair does four rounds and `sha256su0`/`sha256su1` extend the
    /// message schedule four words at a time.
    ///
    /// # Safety
    ///
    /// The CPU must support the `sha2` feature. `data.len()` must be a multiple of 64.
    #[target_feature(enable = "sha2")]
    unsafe fn compress(state: &mut [u32; 8], data: &[u8]) {
        // SAFETY: `state` holds eight words, so both four-word loads and stores are in bounds.
        let (mut abcd, mut efgh) =
            unsafe { (vld1q_u32(state.as_ptr()), vld1q_u32(state.as_ptr().add(4))) };
        for block in data.chunks_exact(BLOCK) {
            let (saved_abcd, saved_efgh) = (abcd, efgh);
            // SAFETY: `block` is 64 bytes, so each of the four 16-byte loads is in bounds.
            let mut w = unsafe {
                [
                    vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr()))),
                    vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(16)))),
                    vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(32)))),
                    vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(48)))),
                ]
            };
            for group in 0..16 {
                let slot = group % 4;
                if group >= 4 {
                    // Words 4*group.. of the schedule from the four groups before.
                    w[slot] = vsha256su1q_u32(
                        vsha256su0q_u32(w[slot], w[(slot + 1) % 4]),
                        w[(slot + 2) % 4],
                        w[(slot + 3) % 4],
                    );
                }
                // SAFETY: `group` is below 16, so the load stays inside the 64 constants.
                let k = unsafe { vld1q_u32(ROUND_CONSTANTS.as_ptr().add(4 * group)) };
                let sum = vaddq_u32(w[slot], k);
                let before = abcd;
                abcd = vsha256hq_u32(abcd, efgh, sum);
                efgh = vsha256h2q_u32(efgh, before, sum);
            }
            abcd = vaddq_u32(abcd, saved_abcd);
            efgh = vaddq_u32(efgh, saved_efgh);
        }
        // SAFETY: as for the loads above.
        unsafe {
            vst1q_u32(state.as_mut_ptr(), abcd);
            vst1q_u32(state.as_mut_ptr().add(4), efgh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    /// A reader that returns its data in odd-sized pieces, as files and pipes may.
    struct Dribble<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(self.data.len()).min(out.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn matches_the_reference_for_every_padding_shape() {
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let lengths = (0..200).chain([
            1000,
            4095,
            4096,
            65_535,
            READ_BYTES - 1,
            READ_BYTES,
            READ_BYTES + 1,
            data.len(),
        ]);
        for len in lengths.filter(|n| *n <= data.len()) {
            let slice = &data[..len];
            let expected = reference(slice);
            for step in [1usize, 7, 64, 100, 4096, usize::MAX] {
                let got = sha256_of(&mut Dribble { data: slice, step }).unwrap();
                assert_eq!(got, expected, "length {len}, read size {step}");
            }
            assert_eq!(software_sha256_of(&mut &slice[..]).unwrap(), expected);
        }
    }

    #[test]
    fn spans_several_read_buffers() {
        let data = vec![0xa5u8; 2 * READ_BYTES + 12_345];
        assert_eq!(sha256_of(&mut &data[..]).unwrap(), reference(&data));
    }
}
