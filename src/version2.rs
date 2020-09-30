use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;

use crate::errors::Errors;
use crate::pae;

use base64::{decode_config, encode_config, URL_SAFE_NO_PAD};
use rand_core::{CryptoRng, RngCore};

pub struct PublicToken;

impl PublicToken {
    pub const HEADER: &'static str = "v2.public.";

    pub fn sign(
        secret_key: impl AsRef<[u8]>,
        public_key: impl AsRef<[u8]>,
        message: impl AsRef<[u8]>,
        footer: Option<impl AsRef<[u8]>>,
    ) -> String {
        use ed25519_dalek::Keypair;
        use ed25519_dalek::PublicKey;
        use ed25519_dalek::SecretKey;
        use ed25519_dalek::Signer;

        let kp: Keypair = Keypair {
            secret: SecretKey::from_bytes(secret_key.as_ref()).unwrap(),
            public: PublicKey::from_bytes(public_key.as_ref()).unwrap(),
        };

        let f = match footer {
            Some(ref val) => val.as_ref(),
            None => &[0u8; 0],
        };

        let m2 = pae::pae(&[Self::HEADER.as_bytes(), message.as_ref(), f]);
        let sig = kp.sign(m2.as_ref());

        // sig.len() == 64, and additional 64 as buffer.
        let mut m_sig: Vec<u8> = Vec::with_capacity(64 + 64);
        m_sig.extend_from_slice(message.as_ref());
        m_sig.extend_from_slice(sig.to_bytes().as_ref());

        let token_no_footer = format!("{}{}", Self::HEADER, encode_config(m_sig, URL_SAFE_NO_PAD));

        if f.is_empty() {
            token_no_footer
        } else {
            format!("{}.{}", token_no_footer, encode_config(f, URL_SAFE_NO_PAD))
        }
    }

    pub fn verify(
        public_key: impl AsRef<[u8]>,
        token: &str,
        footer: Option<&str>,
    ) -> Result<bool, Errors> {
        use ed25519_dalek::PublicKey;
        use ed25519_dalek::Signature;
        use ed25519_dalek::Verifier;
        use orion::util::secure_cmp;

        if !token.starts_with(Self::HEADER) {
            return Err(Errors::TokenFormatError);
        }

        let f = match footer {
            Some(ref val) => val.as_bytes(),
            None => &[0u8; 0],
        };

        let parts_split = token.split('.').collect::<Vec<&str>>();

        if parts_split.len() == 4 {
            // footer is present in token
            if footer.is_none() {
                return Err(Errors::TokenFormatError);
            }

            let token_footer = decode_config(parts_split[3], URL_SAFE_NO_PAD).unwrap();
            if secure_cmp(f, token_footer.as_ref()).is_err() {
                return Err(Errors::TokenValidationError);
            }
        }

        let sm = decode_config(parts_split[2], URL_SAFE_NO_PAD).unwrap();
        let s = sm[(sm.len() - 64)..sm.len()].as_ref();
        let m = sm[..(sm.len() - 64)].as_ref();

        let m2 = pae::pae(&[Self::HEADER.as_bytes(), m, f]);
        let pk: PublicKey = PublicKey::from_bytes(public_key.as_ref()).unwrap();
        let sig = Signature::try_from(s).unwrap();

        if pk.verify(m2.as_ref(), &sig).is_ok() {
            return Ok(true);
        } else {
            return Err(Errors::TokenValidationError);
        }
    }
}

pub struct LocalToken;

impl LocalToken {
    pub const HEADER: &'static str = "v2.local.";

    fn encrypt_with_nonce(
        secret_key: impl AsRef<[u8]>,
        nonce: impl AsRef<[u8]>,
        message: impl AsRef<[u8]>,
        footer: Option<impl AsRef<[u8]>>,
    ) -> String {
        use orion::hazardous::aead::xchacha20poly1305::*;
        use orion::hazardous::hash::blake2b;

        let nonce_key = blake2b::SecretKey::from_slice(nonce.as_ref()).unwrap();
        let mut blake2b = blake2b::Blake2b::new(Some(&nonce_key), 24).unwrap();
        blake2b.update(message.as_ref()).unwrap();
        let nonce = Nonce::from_slice(blake2b.finalize().unwrap().as_ref()).unwrap();

        let f = match footer {
            Some(ref val) => val.as_ref(),
            None => &[0u8; 0],
        };
        let pre_auth = pae::pae(&[Self::HEADER.as_bytes(), nonce.as_ref(), f]);

        let mut out = vec![0u8; message.as_ref().len() + 16 + nonce.len()];
        seal(
            &SecretKey::from_slice(secret_key.as_ref()).unwrap(),
            &nonce,
            message.as_ref(),
            Some(&pre_auth),
            &mut out[nonce.len()..],
        )
        .unwrap();
        out[..nonce.len()].copy_from_slice(nonce.as_ref());

        let token_no_footer = format!("{}{}", Self::HEADER, encode_config(out, URL_SAFE_NO_PAD));

        if f.is_empty() {
            token_no_footer
        } else {
            format!("{}.{}", token_no_footer, encode_config(f, URL_SAFE_NO_PAD))
        }
    }

    pub fn encrypt<C>(
        csprng: &mut C,
        secret_key: impl AsRef<[u8]>,
        message: impl AsRef<[u8]>,
        footer: Option<impl AsRef<[u8]>>,
    ) -> String
    where
        C: CryptoRng + RngCore,
    {
        let mut rng_bytes = [0u8; 24];
        csprng.try_fill_bytes(&mut rng_bytes).unwrap();

        Self::encrypt_with_nonce(secret_key, &rng_bytes, message, footer)
    }

    pub fn decrypt(
        secret_key: impl AsRef<[u8]>,
        token: &str,
        footer: Option<&str>,
    ) -> Result<String, Errors> {
        use orion::hazardous::aead::xchacha20poly1305::*;
        use orion::util::secure_cmp;

        if !token.starts_with(Self::HEADER) {
            return Err(Errors::TokenFormatError);
        }

        let f = match footer {
            Some(ref val) => val.as_bytes(),
            None => &[0u8; 0],
        };

        let parts_split = token.split('.').collect::<Vec<&str>>();

        if parts_split.len() == 4 {
            // footer is present in token
            if footer.is_none() {
                return Err(Errors::TokenFormatError);
            }

            let token_footer = decode_config(parts_split[3], URL_SAFE_NO_PAD).unwrap();
            if secure_cmp(f, token_footer.as_ref()).is_err() {
                return Err(Errors::TokenValidationError);
            }
        }

        let nc = decode_config(parts_split[2], URL_SAFE_NO_PAD).unwrap();
        let n = nc[..24].as_ref();
        let c = nc[n.len()..].as_ref();

        let pre_auth = pae::pae(&[Self::HEADER.as_bytes(), n, f]);
        let mut out = vec![0u8; c.len() - 16];

        open(
            &SecretKey::from_slice(secret_key.as_ref()).unwrap(),
            &Nonce::from_slice(n).unwrap(),
            c,
            Some(pre_auth.as_ref()),
            &mut out,
        )
        .unwrap();

        Ok(String::from_utf8_lossy(out.as_ref()).into())
    }
}

#[cfg(test)]
mod test_public {

    use super::PublicToken;
    use hex;

    // Test vectors from: https://github.com/paragonie/paseto/blob/master/tests/Version2VectorTest.php
    const TEST_SK: [u8; 32] = [
        180, 203, 251, 67, 223, 76, 226, 16, 114, 125, 149, 62, 74, 113, 51, 7, 250, 25, 187, 125,
        159, 133, 4, 20, 56, 217, 225, 27, 148, 42, 55, 116,
    ];
    const TEST_PK: [u8; 32] = [
        30, 185, 219, 187, 188, 4, 124, 3, 253, 112, 96, 78, 0, 113, 240, 152, 126, 22, 178, 139,
        117, 114, 37, 193, 31, 0, 65, 93, 14, 32, 177, 162,
    ];

    #[test]
    fn check_test_keys() {
        assert_eq!(
            TEST_SK.as_ref(),
            hex::decode("b4cbfb43df4ce210727d953e4a713307fa19bb7d9f85041438d9e11b942a3774")
                .unwrap()
        );
        assert_eq!(
            TEST_PK.as_ref(),
            hex::decode("1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2")
                .unwrap()
        );
    }

    #[test]
    fn test_sign_verify_official_1() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAxOS0wMS0wMVQwMDowMDowMCswMDowMCJ9HQr8URrGntTu7Dz9J2IF23d1M7-9lH9xiqdGyJNvzp4angPW5Esc7C5huy_M8I8_DjJK2ZXC2SUYuOFM-Q_5Cw";
        let footer = "";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_official_2() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAxOS0wMS0wMVQwMDowMDowMCswMDowMCJ9flsZsx_gYCR0N_Ec2QxJFFpvQAs7h9HtKwbVK2n1MJ3Rz-hwe8KUqjnd8FAnIJZ601tp7lGkguU63oGbomhoBw.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        let footer = "{\"kid\":\"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN\"}";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_1() {
        // Empty string, 32-character NUL byte key.
        let message = "";
        let expected = "v2.public.xnHHprS7sEyjP5vWpOvHjAP2f0HER7SWfPuehZ8QIctJRPTrlZLtRCk9_iNdugsrqJoGaO4k9cDBq3TOXu24AA";
        let footer = "";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_2() {
        // Empty string, 32-character NUL byte key, non-empty footer.
        let message = "";
        let expected = "v2.public.Qf-w0RdU2SDGW_awMwbfC0Alf_nd3ibUdY3HigzU7tn_4MPMYIKAJk_J_yKYltxrGlxEdrWIqyfjW81njtRyDw.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_3() {
        // Non-empty string, 32-character 0xFF byte key.
        let message = "Frank Denis rocks";
        let expected = "v2.public.RnJhbmsgRGVuaXMgcm9ja3NBeHgns4TLYAoyD1OPHww0qfxHdTdzkKcyaE4_fBF2WuY1JNRW_yI8qRhZmNTaO19zRhki6YWRaKKlCZNCNrQM";
        let footer = "";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_4() {
        // Non-empty string, 32-character 0xFF byte key. (One character difference)
        let message = "Frank Denis rockz";
        let expected = "v2.public.RnJhbmsgRGVuaXMgcm9ja3qIOKf8zCok6-B5cmV3NmGJCD6y3J8fmbFY9KHau6-e9qUICrGlWX8zLo-EqzBFIT36WovQvbQZq4j6DcVfKCML";
        let footer = "";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_5() {
        // Non-empty string, 32-character 0xFF byte key, non-empty footer.
        let message = "Frank Denis rocks";
        let expected = "v2.public.RnJhbmsgRGVuaXMgcm9ja3O7MPuu90WKNyvBUUhAGFmi4PiPOr2bN2ytUSU-QWlj8eNefki2MubssfN1b8figynnY0WusRPwIQ-o0HSZOS0F.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_6() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAxOS0wMS0wMVQwMDowMDowMCswMDowMCJ9HQr8URrGntTu7Dz9J2IF23d1M7-9lH9xiqdGyJNvzp4angPW5Esc7C5huy_M8I8_DjJK2ZXC2SUYuOFM-Q_5Cw";
        let footer = "";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_7() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAxOS0wMS0wMVQwMDowMDowMCswMDowMCJ9fgvV_frkjyH7h0CWrGfonEctefgzQaCkICOAxDdbixbPvH_SMm0T6343YfgEAlOi8--euLS5gLlykHhREL38BA.UGFyYWdvbiBJbml0aWF0aXZlIEVudGVycHJpc2Vz";
        let footer = "Paragon Initiative Enterprises";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }

    #[test]
    fn test_sign_verify_8() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAxOS0wMS0wMVQwMDowMDowMCswMDowMCJ9flsZsx_gYCR0N_Ec2QxJFFpvQAs7h9HtKwbVK2n1MJ3Rz-hwe8KUqjnd8FAnIJZ601tp7lGkguU63oGbomhoBw.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        let footer = "{\"kid\":\"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN\"}";
        let actual = PublicToken::sign(TEST_SK, TEST_PK, message, Some(footer));

        assert_eq!(expected, actual);
        assert!(PublicToken::verify(TEST_PK, expected, Some(footer)).is_ok());
        assert!(PublicToken::verify(TEST_PK, &actual, Some(footer)).is_ok());
    }
}

#[cfg(test)]
mod test_local {

    use super::LocalToken;
    use hex;

    // Test vectors from: https://github.com/paragonie/paseto/blob/master/tests/Version2VectorTest.php
    const TEST_SK: [u8; 32] = [
        112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129,
        130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143,
    ];

    const TEST_NULL_KEY: [u8; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ];

    const TEST_FULL_KEY: [u8; 32] = [
        255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
    ];

    const TEST_NONCE: [u8; 24] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    const TEST_NONCE_2: [u8; 24] = [
        69, 116, 44, 151, 109, 104, 79, 248, 78, 189, 192, 222, 89, 128, 154, 151, 205, 162, 246,
        76, 132, 253, 161, 155,
    ];

    #[test]
    fn check_test_key_and_nonces() {
        assert_eq!(
            TEST_SK.as_ref(),
            hex::decode("707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f")
                .unwrap()
        );
        assert_eq!(
            TEST_NONCE_2.as_ref(),
            hex::decode("45742c976d684ff84ebdc0de59809a97cda2f64c84fda19b").unwrap()
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_1() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.97TTOvgwIxNGvV80XKiGZg_kD3tsXM_-qB4dZGHOeN1cTkgQ4PnW8888l802W8d9AvEGnoNBY3BnqHORy8a5cC8aKpbA0En8XELw2yDk2f1sVODyfnDbi6rEGMY3pSfCbLWMM2oHJxvlEl2XbQ";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_2() {
        let message =
            "{\"data\":\"this is a secret message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.CH50H-HM5tzdK4kOmQ8KbIvrzJfjYUGuu5Vy9ARSFHy9owVDMYg3-8rwtJZQjN9ABHb2njzFkvpr5cOYuRyt7CRXnHt42L5yZ7siD-4l-FoNsC7J2OlvLlIwlG06mzQVunrFNb7Z3_CHM0PK5w";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_3() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.5K4SCXNhItIhyNuVIZcwrdtaDKiyF81-eWHScuE0idiVqCo72bbjo07W05mqQkhLZdVbxEa5I_u5sgVk1QLkcWEcOSlLHwNpCkvmGGlbCdNExn6Qclw3qTKIIl5-O5xRBN076fSDPo5xUCPpBA";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_4() {
        let message =
            "{\"data\":\"this is a secret message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.pvFdDeNtXxknVPsbBCZF6MGedVhPm40SneExdClOxa9HNR8wFv7cu1cB0B4WxDdT6oUc2toyLR6jA6sc-EUM5ll1EkeY47yYk6q8m1RCpqTIzUrIu3B6h232h62DPbIxtjGvNRAwsLK7LcV8oQ";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_5() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.5K4SCXNhItIhyNuVIZcwrdtaDKiyF81-eWHScuE0idiVqCo72bbjo07W05mqQkhLZdVbxEa5I_u5sgVk1QLkcWEcOSlLHwNpCkvmGGlbCdNExn6Qclw3qTKIIl5-zSLIrxZqOLwcFLYbVK1SrQ.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        let footer = "{\"kid\":\"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN\"}";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_official_6() {
        let message =
            "{\"data\":\"this is a secret message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.pvFdDeNtXxknVPsbBCZF6MGedVhPm40SneExdClOxa9HNR8wFv7cu1cB0B4WxDdT6oUc2toyLR6jA6sc-EUM5ll1EkeY47yYk6q8m1RCpqTIzUrIu3B6h232h62DnMXKdHn_Smp6L_NfaEnZ-A.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        let footer = "{\"kid\":\"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN\"}";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_1() {
        // Empty message, empty footer, empty nonce
        let message = "";
        let expected = "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNUtKpdy5KXjKfpSKrOlqQvQ";
        let footer = "";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_NULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_NULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_2() {
        // Empty message, empty footer, empty nonce
        let message = "";
        let expected = "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNSOvpveyCsjPYfe9mtiJDVg";
        let footer = "";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_FULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_FULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_3() {
        // Empty message, empty footer, empty nonce
        let message = "";
        let expected = "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNkIWACdHuLiJiW16f2GuGYA";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_4() {
        // Empty message, non-empty footer, empty nonce
        let message = "";
        let expected =
            "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNfzz6yGkE4ZxojJAJwKLfvg.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_NULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_NULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_5() {
        // Empty message, non-empty footer, empty nonce
        let message = "";
        let expected =
            "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNJbTJxAGtEg4ZMXY9g2LSoQ.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_FULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_FULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_6() {
        // Empty message, non-empty footer, empty nonce
        let message = "";
        let expected =
            "v2.local.driRNhM20GQPvlWfJCepzh6HdijAq-yNreCcZAS0iGVlzdHjTf2ilg.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_7() {
        // Non-empty message, empty footer, empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.BEsKs5AolRYDb_O-bO-lwHWUextpShFSvu6cB-KuR4wR9uDMjd45cPiOF0zxb7rrtOB5tRcS7dWsFwY4ONEuL5sWeunqHC9jxU0";
        let footer = "";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_NULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_NULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_8() {
        // Non-empty message, empty footer, empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.BEsKs5AolRYDb_O-bO-lwHWUextpShFSjvSia2-chHyMi4LtHA8yFr1V7iZmKBWqzg5geEyNAAaD6xSEfxoET1xXqahe1jqmmPw";
        let footer = "";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_FULL_KEY, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_FULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_9() {
        // Non-empty message, empty footer, empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.BEsKs5AolRYDb_O-bO-lwHWUextpShFSXlvv8MsrNZs3vTSnGQG4qRM9ezDl880jFwknSA6JARj2qKhDHnlSHx1GSCizfcF019U";
        let footer = "";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_10() {
        // Non-empty message, non-empty footer, non-empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.FGVEQLywggpvH0AzKtLXz0QRmGYuC6yvbcqXgWxM3vJGrJ9kWqquP61Xl7bz4ZEqN5XwH7xyzV0QqPIo0k52q5sWxUQ4LMBFFso.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_NULL_KEY, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_NULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_11() {
        // Non-empty message, non-empty footer, non-empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.FGVEQLywggpvH0AzKtLXz0QRmGYuC6yvZMW3MgUMFplQXsxcNlg2RX8LzFxAqj4qa2FwgrUdH4vYAXtCFrlGiLnk-cHHOWSUSaw.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual =
            LocalToken::encrypt_with_nonce(TEST_FULL_KEY, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_FULL_KEY, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_12() {
        // Non-empty message, non-empty footer, non-empty nonce
        let message = "Love is stronger than hate or fear";
        let expected = "v2.local.FGVEQLywggpvH0AzKtLXz0QRmGYuC6yvl05z9GIX0cnol6UK94cfV77AXnShlUcNgpDR12FrQiurS8jxBRmvoIKmeMWC5wY9Y6w.Q3VvbiBBbHBpbnVz";
        let footer = "Cuon Alpinus";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_13() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.5K4SCXNhItIhyNuVIZcwrdtaDKiyF81-eWHScuE0idiVqCo72bbjo07W05mqQkhLZdVbxEa5I_u5sgVk1QLkcWEcOSlLHwNpCkvmGGlbCdNExn6Qclw3qTKIIl5-zKeei_8CY0oUMtEai3HYcQ.UGFyYWdvbiBJbml0aWF0aXZlIEVudGVycHJpc2Vz";
        let footer = "Paragon Initiative Enterprises";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }

    #[test]
    fn test_encrypt_decrypt_14() {
        let message =
            "{\"data\":\"this is a signed message\",\"exp\":\"2019-01-01T00:00:00+00:00\"}";
        let expected = "v2.local.5K4SCXNhItIhyNuVIZcwrdtaDKiyF81-eWHScuE0idiVqCo72bbjo07W05mqQkhLZdVbxEa5I_u5sgVk1QLkcWEcOSlLHwNpCkvmGGlbCdNExn6Qclw3qTKIIl5-zSLIrxZqOLwcFLYbVK1SrQ.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        let footer = "{\"kid\":\"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN\"}";
        let actual = LocalToken::encrypt_with_nonce(TEST_SK, TEST_NONCE_2, message, Some(footer));

        assert_eq!(expected, actual);
        assert_eq!(
            LocalToken::decrypt(TEST_SK, expected, Some(footer)).unwrap(),
            message
        );
    }
}
