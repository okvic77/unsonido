//! AirPlay 1 (RAOP) crypto: the Apple-Challenge response, the RSA-wrapped AES session key and
//! per-packet AES-128-CBC decryption.
//!
//! The RSA key is the AirPort Express key extracted in 2011 and used by every open-source RAOP
//! receiver; senders only accept receivers that can prove possession of it.

use std::net::IpAddr;
use std::sync::LazyLock;

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, general_purpose};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::{Oaep, Pkcs1v15Sign, RsaPrivateKey};

static AIRPORT_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
    RsaPrivateKey::from_pkcs1_pem(include_str!("airport_key.pem"))
        .expect("embedded AirPort key is valid")
});

/// Senders omit base64 padding; accept it either way.
const B64_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    B64_LENIENT.decode(text.trim()).context("invalid base64")
}

/// Sign `challenge || local ip || mac`, zero-padded to 32 bytes, with the AirPort key.
pub fn apple_response(challenge_b64: &str, local_ip: IpAddr, mac: [u8; 6]) -> Result<String> {
    let challenge = b64_decode(challenge_b64)?;
    if challenge.len() > 16 {
        return Err(anyhow!("oversized Apple-Challenge"));
    }
    let mut buf = challenge;
    match local_ip {
        IpAddr::V4(ip) => buf.extend_from_slice(&ip.octets()),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) => buf.extend_from_slice(&v4.octets()),
            None => buf.extend_from_slice(&ip.octets()),
        },
    }
    buf.extend_from_slice(&mac);
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    let signature = AIRPORT_KEY.sign(Pkcs1v15Sign::new_unprefixed(), &buf)?;
    Ok(general_purpose::STANDARD_NO_PAD.encode(signature))
}

/// Unwrap the session key from the ANNOUNCE `rsaaeskey` attribute.
pub fn decrypt_session_key(rsaaeskey_b64: &str) -> Result<[u8; 16]> {
    let wrapped = b64_decode(rsaaeskey_b64)?;
    let key = AIRPORT_KEY
        .decrypt(Oaep::new::<sha1::Sha1>(), &wrapped)
        .context("unwrapping AES key")?;
    key.try_into()
        .map_err(|_| anyhow!("AES key has the wrong length"))
}

/// AES-128-CBC with the IV reset for every packet. Only whole 16-byte blocks are encrypted;
/// trailing bytes are sent in the clear.
pub struct PacketCipher {
    cipher: Aes128,
    iv: [u8; 16],
}

impl PacketCipher {
    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self {
            cipher: Aes128::new(&key.into()),
            iv,
        }
    }

    pub fn decrypt(&self, data: &mut [u8]) {
        let mut prev = self.iv;
        for block in data.as_chunks_mut::<16>().0 {
            let ciphertext = *block;
            let mut b = ciphertext.into();
            self.cipher.decrypt_block(&mut b);
            for (out, (plain, iv)) in block.iter_mut().zip(b.iter().zip(prev)) {
                *out = plain ^ iv;
            }
            prev = ciphertext;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aes::cipher::BlockCipherEncrypt;
    use rsa::traits::PublicKeyParts;
    use rsa::{BigUint, RsaPublicKey};

    pub fn public_key() -> RsaPublicKey {
        RsaPublicKey::from(&*AIRPORT_KEY)
    }

    /// What a sender does: CBC-encrypt the whole blocks of a payload.
    pub fn encrypt_packet(key: [u8; 16], iv: [u8; 16], data: &mut [u8]) {
        let cipher = Aes128::new(&key.into());
        let mut prev = iv;
        for block in data.as_chunks_mut::<16>().0 {
            for (x, p) in block.iter_mut().zip(prev) {
                *x ^= p;
            }
            let mut arr = (*block).into();
            cipher.encrypt_block(&mut arr);
            block.copy_from_slice(&arr);
            prev = *block;
        }
    }

    #[test]
    fn session_key_round_trip() {
        let key = [7u8; 16];
        let wrapped = public_key()
            .encrypt(&mut rand_core(), Oaep::new::<sha1::Sha1>(), &key)
            .unwrap();
        let encoded = general_purpose::STANDARD_NO_PAD.encode(wrapped);
        assert_eq!(decrypt_session_key(&encoded).unwrap(), key);
    }

    #[test]
    fn packet_round_trip_leaves_tail_clear() {
        let (key, iv) = ([1u8; 16], [2u8; 16]);
        let original: Vec<u8> = (0..40u8).collect();
        let mut data = original.clone();
        encrypt_packet(key, iv, &mut data);
        assert_ne!(data[..32], original[..32]);
        assert_eq!(data[32..], original[32..]);
        PacketCipher::new(key, iv).decrypt(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn apple_response_verifies_with_public_key() {
        let challenge = general_purpose::STANDARD_NO_PAD.encode([9u8; 16]);
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        let mac = [0x02, 1, 2, 3, 4, 5];
        let response = apple_response(&challenge, ip, mac).unwrap();
        let signature = b64_decode(&response).unwrap();

        // Raw RSA verify: s^e mod n must be the PKCS#1 type-1 padded message.
        let public = public_key();
        let m = BigUint::from_bytes_be(&signature)
            .modpow(public.e(), public.n())
            .to_bytes_be();
        let mut expected = vec![9u8; 16];
        expected.extend([192, 168, 1, 20]);
        expected.extend(mac);
        expected.resize(32, 0);
        assert_eq!(m[0], 1, "type 1 padding");
        assert_eq!(&m[m.len() - 32..], &expected[..]);
    }

    fn rand_core() -> impl rsa::rand_core::CryptoRngCore {
        rsa::rand_core::OsRng
    }
}
