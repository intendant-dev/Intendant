//! The deterministic vector RNG — spec §13.1.
//!
//! ChaCha20 (RFC 8439) with the vector's 32-byte key and 12-byte
//! nonce, **initial counter 0**. The keystream is ONE byte stream;
//! draws are taken sequentially in the order given by the vector's
//! `draw_order` array of `{name, nbytes}` entries (names explicit and
//! unique within a vector — draw sizes never depend on schema
//! inference, and JSON property order is not portable).

use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20;

pub struct VectorRng {
    cipher: ChaCha20,
}

impl VectorRng {
    pub fn new(key: &[u8; 32], nonce: &[u8; 12]) -> Self {
        Self {
            cipher: ChaCha20::new(key.into(), nonce.into()),
        }
    }

    /// The next `nbytes` of the single keystream (keystream applied to
    /// zeros) — one `draw_order` entry.
    pub fn draw(&mut self, nbytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; nbytes];
        self.cipher.apply_keystream(&mut buf);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc8439_a1_vector1_counter0() {
        // RFC 8439 appendix A.1 test vector #1: zero key, zero nonce,
        // counter 0 — pins the counter-0 start of the stream.
        let mut rng = VectorRng::new(&[0u8; 32], &[0u8; 12]);
        assert_eq!(
            hex(&rng.draw(64)),
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
        );
    }

    #[test]
    fn rfc8439_232_block_function_counter1() {
        // RFC 8439 §2.3.2: key 00..1f, nonce 000000090000004a00000000,
        // counter 1. Our stream starts at counter 0, so bytes 64..128
        // are that block — reached by drawing (not seeking), which also
        // exercises draw-order sequencing across a block boundary.
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 12] = [0, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let mut rng = VectorRng::new(&key, &nonce);
        let _counter0 = rng.draw(64);
        assert_eq!(
            hex(&rng.draw(64)),
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    #[test]
    fn draws_are_one_stream() {
        // Split draws concatenate to the same bytes as one draw: the
        // keystream is a single sequence, not per-draw restarts.
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let mut split = VectorRng::new(&key, &nonce);
        let mut joined: Vec<u8> = Vec::new();
        for n in [1, 2, 3, 5, 8, 13, 32] {
            joined.extend(split.draw(n));
        }
        let mut whole = VectorRng::new(&key, &nonce);
        assert_eq!(joined, whole.draw(64));
    }
}
