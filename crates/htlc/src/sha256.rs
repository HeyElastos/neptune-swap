//! SHA-256 of a 32-byte message in Triton VM, checked against an expected digest.
//!
//! A 32-byte message is one 64-byte block whose second half is fixed padding (0x80, zeros, the
//! bit length 256), so this is a single compression with no length handling. Every value is a
//! u32 by construction, and soundness rests on three VM facts:
//! - `split` decomposes a field element into canonical u32 halves (both range-checked), so a
//!   divined word whose high half is 0 is a u32, and `split` + keep-low is addition mod 2^32;
//! - `and` and `xor` crash on operands that are not u32;
//! - every RAM cell is written before it is read, so the prover's initial RAM is never used.
//!
//! Sums before a reduction stay below 7 * 2^32, far under the field modulus.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const IV: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

/// The second half of the block: 0x80 after the message, zeros, and the length in bits (256).
const PADDING: [u32; 8] = [0x8000_0000, 0, 0, 0, 0, 0, 0, 256];

pub const ERROR_NOT_U32: i128 = 1_000;
/// Plus the index of the first digest word that differs.
pub const ERROR_DIGEST: i128 = 1_100;

/// tasm for the subroutine `label`: divines 8 big-endian u32 words of the message, computes
/// SHA-256 and crashes unless it equals `expected` (8 big-endian words). Uses RAM from `ram`.
pub fn check(label: &str, expected: [u32; 8], ram: u64) -> String {
    let mut g = Gen { code: vec![format!("{label}:")], next: ram };
    let w: Vec<u64> = (0..64).map(|_| g.alloc()).collect();

    for &cell in &w[..8] {
        g.emit(&format!("divine 1 dup 0 split pop 1 push 0 eq assert error_id {ERROR_NOT_U32}"));
        g.store(cell);
    }
    for (&cell, word) in w[8..16].iter().zip(PADDING) {
        g.emit(&format!("push {word}"));
        g.store(cell);
    }
    // Message schedule: W[t] = σ1(W[t-2]) + W[t-7] + σ0(W[t-15]) + W[t-16].
    for t in 16..64 {
        g.sigma(w[t - 2], [Rot(17), Rot(19), Shr(10)]);
        g.load(w[t - 7]);
        g.emit("add");
        g.sigma(w[t - 15], [Rot(7), Rot(18), Shr(3)]);
        g.emit("add");
        g.load(w[t - 16]);
        g.emit("add");
        g.mod32();
        g.store(w[t]);
    }

    let mut s: Vec<u64> = (0..8).map(|_| g.alloc()).collect();
    for (&cell, word) in s.iter().zip(IV) {
        g.emit(&format!("push {word}"));
        g.store(cell);
    }
    for (&k, &wt) in K.iter().zip(&w) {
        let [a, b, c, d, e, f, gg, h] = [s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]];
        // T1 = h + Σ1(e) + Ch(e, f, g) + K[t] + W[t]; Ch = (e & f) ^ (!e & g).
        g.load(h);
        g.sigma(e, [Rot(6), Rot(11), Rot(25)]);
        g.emit("add");
        g.load(e);
        g.load(f);
        g.emit("and");
        g.load(e);
        g.emit("push -1 mul push 4294967295 add"); // !e = 2^32 - 1 - e
        g.load(gg);
        g.emit("and xor add");
        g.emit(&format!("push {k} add"));
        g.load(wt);
        g.emit("add");
        let t1 = g.alloc();
        g.store(t1);
        // T2 = Σ0(a) + Maj(a, b, c); Maj = (a & b) ^ (a & c) ^ (b & c).
        g.sigma(a, [Rot(2), Rot(13), Rot(22)]);
        g.load(a);
        g.load(b);
        g.emit("and");
        g.load(a);
        g.load(c);
        g.emit("and xor");
        g.load(b);
        g.load(c);
        g.emit("and xor add");
        // a' = T1 + T2, e' = d + T1, both mod 2^32.
        g.load(t1);
        g.emit("add");
        g.mod32();
        let a2 = g.alloc();
        g.store(a2);
        g.load(d);
        g.load(t1);
        g.emit("add");
        g.mod32();
        let e2 = g.alloc();
        g.store(e2);
        s = vec![a2, a, b, c, e2, e, f, gg];
    }
    for (i, ((&cell, iv), want)) in s.iter().zip(IV).zip(expected).enumerate() {
        g.load(cell);
        g.emit(&format!("push {iv} add"));
        g.mod32();
        g.emit(&format!("push {want} eq assert error_id {}", ERROR_DIGEST + i as i128));
    }
    g.emit("return");
    g.code.join("\n")
}

#[derive(Clone, Copy)]
enum Op {
    Rot(u32),
    Shr(u32),
}
use Op::{Rot, Shr};

struct Gen {
    code: Vec<String>,
    next: u64,
}

impl Gen {
    fn emit(&mut self, line: &str) {
        self.code.push(line.to_string());
    }

    fn alloc(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    fn load(&mut self, cell: u64) {
        self.emit(&format!("push {cell} read_mem 1 pop 1"));
    }

    fn store(&mut self, cell: u64) {
        self.emit(&format!("push {cell} write_mem 1 pop 1"));
    }

    /// Keeps the low u32 of the top element.
    fn mod32(&mut self) {
        self.emit("split pick 1 pop 1");
    }

    /// x * 2^(32-n) = (x >> n) * 2^32 + (x << (32-n) mod 2^32): split gives both halves, which
    /// never overlap, so their sum is the rotation and the high half alone is the shift.
    fn op(&mut self, op: Op) {
        match op {
            Rot(n) => self.emit(&format!("push {} mul split add", 1u64 << (32 - n))),
            Shr(n) => self.emit(&format!("push {} mul split pop 1", 1u64 << (32 - n))),
        }
    }

    /// Pushes op0(x) ^ op1(x) ^ op2(x) for the word in `cell`.
    fn sigma(&mut self, cell: u64, ops: [Op; 3]) {
        for (i, op) in ops.into_iter().enumerate() {
            self.load(cell);
            self.op(op);
            if i > 0 {
                self.emit("xor");
            }
        }
    }
}
