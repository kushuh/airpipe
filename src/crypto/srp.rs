//! SRP-6a client as used by HomeKit / AirPlay 2 pair-setup.
//!
//! Group: RFC 5054 3072-bit, generator 5, hash SHA-512, username
//! `Pair-Setup`. The password is the PIN shown by the device, the device
//! password, or the fixed `3939` for transient pairing.
//!
//! HomeKit deviates from plain RFC 5054 in two places, both reproduced here:
//! `k = H(N | PAD(g))` and `u = H(PAD(A) | PAD(B))` use values padded to the
//! modulus length, while the proof `M1` hashes unpadded values.

use num_bigint::BigUint;
use sha2::{Digest, Sha512};
use thiserror::Error;

const USERNAME: &[u8] = b"Pair-Setup";

/// RFC 5054 appendix A, 3072-bit group.
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;
/// Byte length of the modulus.
const N_LEN: usize = 384;

#[derive(Debug, Error)]
pub enum SrpError {
    #[error("device sent an invalid SRP public key")]
    InvalidServerKey,
    #[error("device proof did not match: wrong PIN/password or device not genuine")]
    BadServerProof,
}

fn modulus() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("constant is valid hex")
}

fn pad(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; N_LEN.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

fn h(parts: &[&[u8]]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().into()
}

/// Client state after receiving the server's salt and public key.
pub struct Client {
    /// Our public value `A`, sent in pair-setup M3.
    pub public_key: Vec<u8>,
    /// Our proof `M1`, sent in pair-setup M3.
    pub proof: [u8; 64],
    session_key: [u8; 64],
    expected_server_proof: [u8; 64],
}

impl Client {
    /// Runs the client side of SRP for the given password, salt and server key.
    ///
    /// `secret` is 32 random bytes used as the private exponent `a`.
    pub fn new(password: &[u8], salt: &[u8], server_public: &[u8], secret: &[u8; 32]) -> Result<Self, SrpError> {
        let n = modulus();
        let g = BigUint::from(G);
        let b_pub = BigUint::from_bytes_be(server_public);
        if &b_pub % &n == BigUint::ZERO {
            return Err(SrpError::InvalidServerKey);
        }

        let a = BigUint::from_bytes_be(secret);
        let a_pub = g.modpow(&a, &n);

        let k = BigUint::from_bytes_be(&h(&[&n.to_bytes_be(), &pad(&g)]));
        let u = BigUint::from_bytes_be(&h(&[&pad(&a_pub), &pad(&b_pub)]));
        if u == BigUint::ZERO {
            return Err(SrpError::InvalidServerKey);
        }
        let inner = h(&[USERNAME, b":", password]);
        let x = BigUint::from_bytes_be(&h(&[salt, &inner]));

        // S = (B - k * g^x) ^ (a + u * x) mod N, kept non-negative.
        let kgx = (&k * g.modpow(&x, &n)) % &n;
        let base = ((&b_pub % &n) + &n - kgx) % &n;
        let s = base.modpow(&(&a + &u * &x), &n);
        let session_key = h(&[&s.to_bytes_be()]);

        let hn = h(&[&n.to_bytes_be()]);
        let hg = h(&[&g.to_bytes_be()]);
        let mut hxor = [0u8; 64];
        for i in 0..64 {
            hxor[i] = hn[i] ^ hg[i];
        }
        let a_bytes = a_pub.to_bytes_be();
        let proof = h(&[&hxor, &h(&[USERNAME]), salt, &a_bytes, &b_pub.to_bytes_be(), &session_key]);
        let expected_server_proof = h(&[&a_bytes, &proof, &session_key]);

        Ok(Self { public_key: a_bytes, proof, session_key, expected_server_proof })
    }

    /// Checks the server proof `M2` and returns the 64-byte session key `K`.
    pub fn verify(self, server_proof: &[u8]) -> Result<[u8; 64], SrpError> {
        // Not secret-dependent on our side: M2 is public once sent.
        if server_proof != self.expected_server_proof {
            return Err(SrpError::BadServerProof);
        }
        Ok(self.session_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal SRP server, so the client can be tested end to end.
    fn server(password: &[u8], salt: &[u8], b: &BigUint, a_pub: &[u8]) -> (Vec<u8>, [u8; 64], [u8; 64]) {
        let n = modulus();
        let g = BigUint::from(G);
        let x = BigUint::from_bytes_be(&h(&[salt, &h(&[USERNAME, b":", password])]));
        let v = g.modpow(&x, &n);
        let k = BigUint::from_bytes_be(&h(&[&n.to_bytes_be(), &pad(&g)]));
        let b_pub = (&k * &v + g.modpow(b, &n)) % &n;
        let a_pub = BigUint::from_bytes_be(a_pub);
        let u = BigUint::from_bytes_be(&h(&[&pad(&a_pub), &pad(&b_pub)]));
        let s = (&a_pub * v.modpow(&u, &n)).modpow(b, &n);
        let key = h(&[&s.to_bytes_be()]);
        (b_pub.to_bytes_be(), key, h(&[&a_pub.to_bytes_be()]))
    }

    #[test]
    fn client_and_server_agree() {
        let salt = [7u8; 16];
        let a = [3u8; 32];
        let b = BigUint::from_bytes_be(&[9u8; 32]);
        let n = modulus();
        let a_pub = BigUint::from(G).modpow(&BigUint::from_bytes_be(&a), &n).to_bytes_be();
        let (b_pub, server_key, _) = server(b"3939", &salt, &b, &a_pub);

        let client = Client::new(b"3939", &salt, &b_pub, &a).unwrap();
        assert_eq!(client.public_key, a_pub);
        let m2 = h(&[&client.public_key, &client.proof, &server_key]);
        let key = client.verify(&m2).unwrap();
        assert_eq!(key, server_key);
    }

    #[test]
    fn wrong_password_fails_verification() {
        let salt = [1u8; 16];
        let a = [2u8; 32];
        let b = BigUint::from_bytes_be(&[4u8; 32]);
        let n = modulus();
        let a_pub = BigUint::from(G).modpow(&BigUint::from_bytes_be(&a), &n).to_bytes_be();
        let (b_pub, server_key, _) = server(b"1234", &salt, &b, &a_pub);

        let client = Client::new(b"9999", &salt, &b_pub, &a).unwrap();
        let m2 = h(&[&client.public_key, &client.proof, &server_key]);
        assert!(matches!(client.verify(&m2), Err(SrpError::BadServerProof)));
    }

    #[test]
    fn zero_server_key_is_rejected() {
        assert!(matches!(Client::new(b"3939", &[0; 16], &[0], &[1; 32]), Err(SrpError::InvalidServerKey)));
    }
}
