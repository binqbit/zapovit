use application::{Crypto, Envelope, Error, GuardianCode, RecoveryHasher, Result};
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use domain::{Id, Policy};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Semaphore;
use vsss_rs::Gf256;
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize, Deserialize)]
pub struct KeyringFile {
    pub active: String,
    pub keys: BTreeMap<String, String>,
}
impl Drop for KeyringFile {
    fn drop(&mut self) {
        for value in self.keys.values_mut() {
            value.zeroize();
        }
    }
}

pub struct Keyring {
    active: String,
    keys: BTreeMap<String, Zeroizing<Vec<u8>>>,
}
impl Keyring {
    pub fn from_file_data(file: KeyringFile) -> Result<Self> {
        if file.active.is_empty() || file.active.len() > 32 || !file.keys.contains_key(&file.active)
        {
            return Err(Error::Config);
        }
        let mut keys = BTreeMap::new();
        for (id, value) in &file.keys {
            let bytes = Zeroizing::new(B64.decode(value.as_bytes()).map_err(|_| Error::Config)?);
            if bytes.len() != 32 {
                return Err(Error::Config);
            }
            keys.insert(id.clone(), bytes);
        }
        Ok(Self {
            active: file.active.clone(),
            keys,
        })
    }
    fn get(&self, id: &str) -> Result<&[u8]> {
        self.keys.get(id).map(|v| v.as_slice()).ok_or(Error::Crypto)
    }
}

pub struct CryptoAdapter {
    kek: Keyring,
    verifier_keys: Keyring,
}
impl CryptoAdapter {
    pub fn new(kek: Keyring, verifier_keys: Keyring) -> Self {
        Self { kek, verifier_keys }
    }
    fn wrapping_aad(context: &str, id: Id, key_id: &str) -> Vec<u8> {
        let mut aad = b"zapovit/wrap/v1\0".to_vec();
        for field in [context.as_bytes(), id.as_bytes(), key_id.as_bytes()] {
            aad.extend_from_slice(&(field.len() as u32).to_be_bytes());
            aad.extend_from_slice(field);
        }
        aad
    }
    fn mac_input(secret_id: Id, actor: Id, policy: &Policy, raw: &[u8]) -> Vec<u8> {
        let mut out = b"zapovit/guardian/verifier/v1\0".to_vec();
        out.extend_from_slice(secret_id.as_bytes());
        out.extend_from_slice(actor.as_bytes());
        out.extend_from_slice(&Sha256::digest(policy.canonical_bytes()));
        out.extend_from_slice(raw);
        out
    }
}

impl Crypto for CryptoAdapter {
    fn random_key(&self) -> Result<Zeroizing<Vec<u8>>> {
        let mut bytes = Zeroizing::new(vec![0; 32]);
        getrandom::fill(&mut bytes).map_err(|_| Error::Crypto)?;
        Ok(bytes)
    }
    fn seal(&self, key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Envelope> {
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| Error::Crypto)?;
        let mut nonce = [0; 24];
        getrandom::fill(&mut nonce).map_err(|_| Error::Crypto)?;
        let ciphertext = cipher
            .encrypt(
                &XNonce::try_from(nonce.as_slice()).map_err(|_| Error::Crypto)?,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| Error::Crypto)?;
        Ok(Envelope {
            version: 1,
            key_id: "dek".into(),
            nonce: B64.encode(nonce),
            ciphertext: B64.encode(ciphertext),
        })
    }
    fn open(&self, key: &[u8], aad: &[u8], envelope: &Envelope) -> Result<Zeroizing<Vec<u8>>> {
        if envelope.version != 1 || envelope.ciphertext.len() > 15 * 1024 * 1024 {
            return Err(Error::Crypto);
        }
        let nonce = B64.decode(&envelope.nonce).map_err(|_| Error::Crypto)?;
        if nonce.len() != 24 {
            return Err(Error::Crypto);
        }
        let ciphertext = B64
            .decode(&envelope.ciphertext)
            .map_err(|_| Error::Crypto)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| Error::Crypto)?;
        cipher
            .decrypt(
                &XNonce::try_from(nonce.as_slice()).map_err(|_| Error::Crypto)?,
                Payload {
                    msg: &ciphertext,
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| Error::Crypto)
    }
    fn wrap(&self, context: &str, id: Id, plaintext: &[u8]) -> Result<Envelope> {
        let mut envelope = self.seal(
            self.kek.get(&self.kek.active)?,
            &Self::wrapping_aad(context, id, &self.kek.active),
            plaintext,
        )?;
        envelope.key_id = self.kek.active.clone();
        Ok(envelope)
    }
    fn unwrap(&self, context: &str, id: Id, envelope: &Envelope) -> Result<Zeroizing<Vec<u8>>> {
        self.open(
            self.kek.get(&envelope.key_id)?,
            &Self::wrapping_aad(context, id, &envelope.key_id),
            envelope,
        )
    }
    fn split(&self, secret_id: Id, key: &[u8], policy: &Policy) -> Result<Vec<GuardianCode>> {
        if key.len() != 32
            || policy.threshold == 0
            || usize::from(policy.threshold) > policy.guardians.len()
            || policy.guardians.len() > 10
        {
            return Err(Error::Crypto);
        }
        let shares = if policy.threshold == 1 {
            (1..=policy.guardians.len())
                .map(|i| {
                    let mut s = vec![i as u8];
                    s.extend_from_slice(key);
                    s
                })
                .collect()
        } else {
            // Fallible OS RNG is never replaced with deterministic randomness. A failure aborts this operation.
            std::panic::catch_unwind(|| {
                Gf256::split_bytes(
                    usize::from(policy.threshold),
                    policy.guardians.len(),
                    key,
                    rand_core::UnwrapErr(OsRng),
                )
            })
            .map_err(|_| Error::Crypto)?
            .map_err(|_| Error::Crypto)?
        };
        let shares = Zeroizing::new(shares);
        let mut result = Vec::new();
        for (actor, share) in policy.guardians.iter().zip(shares.iter()) {
            if share.len() != 33 {
                return Err(Error::Crypto);
            }
            let mut raw = Zeroizing::new(vec![if policy.threshold == 1 { 1 } else { 2 }]);
            raw.extend_from_slice(secret_id.as_bytes());
            raw.extend_from_slice(share);
            let checksum = Sha256::digest(raw.as_slice());
            let encoded = Zeroizing::new(B64.encode(raw.as_slice()));
            let code = Zeroizing::new(format!(
                "Z1.{}.{}",
                encoded.as_str(),
                hex::encode(&checksum[..4])
            ));
            let input = Zeroizing::new(Self::mac_input(secret_id, *actor, policy, &raw));
            let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(
                self.verifier_keys.get(&self.verifier_keys.active)?,
            )
            .map_err(|_| Error::Crypto)?;
            mac.update(&input);
            result.push(GuardianCode {
                account_id: *actor,
                index: share[0],
                code,
                verifier_key_id: self.verifier_keys.active.clone(),
                verifier: B64.encode(mac.finalize().into_bytes()),
            });
        }
        Ok(result)
    }
    fn verify_code(
        &self,
        secret_id: Id,
        actor: Id,
        policy: &Policy,
        key_id: &str,
        verifier: &str,
        code: &str,
    ) -> Result<Zeroizing<Vec<u8>>> {
        if code.len() > 256 || !policy.guardians.contains(&actor) {
            return Err(Error::InvalidCode);
        }
        let parts: Vec<_> = code.split('.').collect();
        if parts.len() != 3 || parts[0] != "Z1" {
            return Err(Error::InvalidCode);
        }
        let raw = Zeroizing::new(B64.decode(parts[1]).map_err(|_| Error::InvalidCode)?);
        if raw.len() != 50
            || raw[0] != if policy.threshold == 1 { 1 } else { 2 }
            || raw[1..17] != secret_id.as_bytes()[..]
            || raw[17] == 0
            || usize::from(raw[17]) > policy.guardians.len()
            || hex::encode(&Sha256::digest(raw.as_slice())[..4]) != parts[2]
        {
            return Err(Error::InvalidCode);
        }
        let input = Zeroizing::new(Self::mac_input(secret_id, actor, policy, &raw));
        let mut mac =
            <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(self.verifier_keys.get(key_id)?)
                .map_err(|_| Error::Crypto)?;
        mac.update(&input);
        mac.verify_slice(&B64.decode(verifier).map_err(|_| Error::InvalidCode)?)
            .map_err(|_| Error::InvalidCode)?;
        Ok(Zeroizing::new(raw[17..].to_vec()))
    }
    fn combine(&self, threshold: u8, shares: &[Zeroizing<Vec<u8>>]) -> Result<Zeroizing<Vec<u8>>> {
        if threshold == 0
            || shares.len() < usize::from(threshold)
            || shares.iter().any(|s| s.len() != 33 || s[0] == 0)
        {
            return Err(Error::InvalidCode);
        }
        let distinct: std::collections::BTreeSet<_> = shares.iter().map(|s| s[0]).collect();
        if distinct.len() != shares.len() {
            return Err(Error::InvalidCode);
        }
        if threshold == 1 {
            return Ok(Zeroizing::new(shares[0][1..].to_vec()));
        }
        let copies = Zeroizing::new(
            shares
                .iter()
                .take(usize::from(threshold))
                .map(|s| s.to_vec())
                .collect::<Vec<_>>(),
        );
        let result = Gf256::combine_bytes(copies.as_slice()).map_err(|_| Error::Crypto)?;
        if result.len() != 32 {
            return Err(Error::Crypto);
        }
        Ok(Zeroizing::new(result))
    }
    fn digest(&self, bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }
}

struct OsRng;
impl rand_core::TryRng for OsRng {
    type Error = getrandom::Error;
    fn try_next_u32(&mut self) -> std::result::Result<u32, Self::Error> {
        let mut b = [0; 4];
        getrandom::fill(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn try_next_u64(&mut self) -> std::result::Result<u64, Self::Error> {
        let mut b = [0; 8];
        getrandom::fill(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> std::result::Result<(), Self::Error> {
        getrandom::fill(dst)
    }
}
impl rand_core::TryCryptoRng for OsRng {}

pub struct ArgonHasher {
    slots: Arc<Semaphore>,
    params: Params,
}
impl ArgonHasher {
    pub fn new(memory: u32, iterations: u32, parallelism: u32, concurrency: usize) -> Result<Self> {
        if !(65536..=262144).contains(&memory)
            || !(3..=10).contains(&iterations)
            || !(1..=4).contains(&parallelism)
            || !(1..=2).contains(&concurrency)
        {
            return Err(Error::Config);
        }
        Ok(Self {
            slots: Arc::new(Semaphore::new(concurrency)),
            params: Params::new(memory, iterations, parallelism, Some(32))
                .map_err(|_| Error::Config)?,
        })
    }
    pub fn selector(token: &str) -> Result<Id> {
        if token.len() > 128 {
            return Err(Error::InvalidCode);
        }
        let parts: Vec<_> = token.split('.').collect();
        if parts.len() != 3 || parts[0] != "R1" || parts[2].len() != 43 {
            return Err(Error::InvalidCode);
        }
        Id::parse_str(parts[1]).map_err(|_| Error::InvalidCode)
    }
}
#[async_trait]
impl RecoveryHasher for ArgonHasher {
    async fn issue(&self) -> Result<(Id, Zeroizing<String>, String)> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::RateLimited)?;
        let params = self.params.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let selector = Id::new_v4();
            let mut bytes = Zeroizing::new([0u8; 32]);
            let mut salt = [0u8; 16];
            getrandom::fill(bytes.as_mut()).map_err(|_| Error::Crypto)?;
            getrandom::fill(&mut salt).map_err(|_| Error::Crypto)?;
            let encoded = Zeroizing::new(B64.encode(bytes.as_slice()));
            let token = Zeroizing::new(format!("R1.{}.{}", selector.simple(), encoded.as_str()));
            let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            let hash = argon
                .hash_password_with_salt(token.as_bytes(), &salt)
                .map_err(|_| Error::Crypto)?
                .to_string();
            Ok((selector, token, hash))
        })
        .await
        .map_err(|_| Error::Internal)?
    }
    async fn verify(&self, token: &str, selector: Id, verifier: &str) -> Result<bool> {
        if Self::selector(token)? != selector || verifier.len() > 256 {
            return Ok(false);
        }
        let expected = format!(
            "$argon2id$v=19$m={},t={},p={}$",
            self.params.m_cost(),
            self.params.t_cost(),
            self.params.p_cost()
        );
        if !verifier.starts_with(&expected) {
            return Ok(false);
        }
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::RateLimited)?;
        let password = Zeroizing::new(token.to_owned());
        let hash = verifier.to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let hash = PasswordHash::new(&hash).map_err(|_| Error::InvalidCode)?;
            Ok(Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok())
        })
        .await
        .map_err(|_| Error::Internal)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngSeed};

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            rng_seed: RngSeed::Fixed(0x5a_41_50_4f_56_49_54),
            ..ProptestConfig::default()
        })]

        #[test]
        fn threshold_recovers_random_keys_from_varying_subsets(
            key in any::<[u8; 32]>(),
            count in 1u8..=10,
            threshold_seed in any::<u8>(),
            ordering in any::<[u16; 10]>(),
        ) {
            let c = crypto();
            let id = Id::from_u128(500);
            let threshold = 1 + threshold_seed % count;
            let policy = Policy {
                guardians: (1..=count).map(|n| Id::from_u128(u128::from(n))).collect(),
                recipients: [Id::from_u128(501)].into(),
                threshold,
                timing: Default::default(),
            };
            let mut codes = c.split(id, &key, &policy).unwrap();
            codes.sort_by_key(|code| (ordering[usize::from(code.index) - 1], code.index));
            let shares = codes.iter().take(usize::from(threshold)).map(|code| {
                c.verify_code(id, code.account_id, &policy, &code.verifier_key_id, &code.verifier, &code.code).unwrap()
            }).collect::<Vec<_>>();
            let recovered = c.combine(threshold, &shares).unwrap();
            prop_assert_eq!(recovered.as_slice(), key.as_slice());
            prop_assert!(c.combine(threshold, &shares[..shares.len() - 1]).is_err());
        }

        #[test]
        fn malformed_code_and_envelope_inputs_do_not_panic(
            code_bytes in proptest::collection::vec(any::<u8>(), 0..512),
            nonce_bytes in proptest::collection::vec(any::<u8>(), 0..128),
            ciphertext in proptest::collection::vec(any::<u8>(), 0..2048),
            version in 0u8..=3,
        ) {
            let c = crypto();
            let actor = Id::from_u128(1);
            let id = Id::from_u128(500);
            let policy = Policy {
                guardians: [actor].into(),
                recipients: [actor].into(),
                threshold: 1,
                timing: Default::default(),
            };
            let code = String::from_utf8_lossy(&code_bytes);
            let _ = c.verify_code(id, actor, &policy, "test", &B64.encode([0; 32]), &code);
            let structured_code = format!("Z1.{}.00000000", B64.encode(&code_bytes));
            let _ = c.verify_code(id, actor, &policy, "test", &B64.encode([0; 32]), &structured_code);
            let envelope = Envelope {
                version,
                key_id: "dek".into(),
                nonce: B64.encode(nonce_bytes),
                ciphertext: B64.encode(ciphertext),
            };
            let _ = c.open(&[0; 32], b"synthetic parser test", &envelope);
        }
    }
    fn crypto() -> CryptoAdapter {
        let keys = |n| {
            Keyring::from_file_data(KeyringFile {
                active: "test".into(),
                keys: [("test".into(), B64.encode([n; 32]))].into(),
            })
            .unwrap()
        };
        CryptoAdapter::new(keys(1), keys(2))
    }
    #[test]
    fn opens_published_ietf_xchacha_vector() {
        // draft-irtf-cfrg-xchacha, Appendix A.1; independent of our sealing path.
        let c = crypto();
        let key = (0x80u8..=0x9f).collect::<Vec<_>>();
        let nonce = (0x40u8..=0x57).collect::<Vec<_>>();
        let aad = hex::decode("50515253c0c1c2c3c4c5c6c7").unwrap();
        let ciphertext = hex::decode(concat!(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb",
            "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452",
            "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9",
            "21f9664c97637da9768812f615c68b13b52ec0875924c1c7987947deafd8780acf49"
        ))
        .unwrap();
        let envelope = Envelope {
            version: 1,
            key_id: "dek".into(),
            nonce: B64.encode(nonce),
            ciphertext: B64.encode(ciphertext),
        };
        let expected=hex::decode("4c616469657320616e642047656e746c656d656e206f662074686520636c617373206f66202739393a204966204920636f756c64206f6666657220796f75206f6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73637265656e20776f756c642062652069742e").unwrap();
        assert_eq!(*c.open(&key, &aad, &envelope).unwrap(), expected);
    }
    #[test]
    fn combines_fixed_gf256_polynomial_vector() {
        // f(x)=secret + x + x² over GF(256), evaluated at x=1,2,3.
        // This checks the published field representation, not a split/combine round trip.
        let secret = (0u8..32).collect::<Vec<_>>();
        let shares = [(1, 0), (2, 6), (3, 6)]
            .into_iter()
            .map(|(index, mask)| {
                let mut bytes = vec![index];
                bytes.extend(secret.iter().map(|b| b ^ mask));
                Zeroizing::new(bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(*crypto().combine(3, &shares).unwrap(), secret);
    }
    #[test]
    fn authenticated_envelopes_reject_wrong_context_key_and_tampering() {
        let c = crypto();
        let id = Id::new_v4();
        let mut e = c.wrap("draft", id, b"password\0\xf0\x9f\x92\x99").unwrap();
        assert_eq!(
            &*c.unwrap("draft", id, &e).unwrap(),
            b"password\0\xf0\x9f\x92\x99"
        );
        assert!(c.unwrap("submission", id, &e).is_err());
        assert!(c.unwrap("draft", Id::new_v4(), &e).is_err());
        let mut bytes = B64.decode(&e.ciphertext).unwrap();
        bytes[0] ^= 1;
        e.ciphertext = B64.encode(bytes);
        assert!(c.unwrap("draft", id, &e).is_err());
    }
    #[test]
    fn rotation_retains_old_bindings_and_rejects_missing_or_relabelled_keys() {
        let old = crypto();
        let keyring = |previous, next, retain| {
            let mut keys = BTreeMap::from([("next".into(), B64.encode([next; 32]))]);
            if retain {
                keys.insert("test".into(), B64.encode([previous; 32]));
            }
            Keyring::from_file_data(KeyringFile {
                active: "next".into(),
                keys,
            })
            .unwrap()
        };
        let rotated = CryptoAdapter::new(keyring(1, 3, true), keyring(2, 4, true));
        let retired = CryptoAdapter::new(keyring(1, 3, false), keyring(2, 4, false));
        let id = Id::new_v4();
        let envelope = old
            .wrap("draft", id, b"synthetic rotation example")
            .unwrap();
        assert_eq!(
            &*rotated.unwrap("draft", id, &envelope).unwrap(),
            b"synthetic rotation example"
        );
        assert!(retired.unwrap("draft", id, &envelope).is_err());
        let mut relabelled = envelope.clone();
        relabelled.key_id = "next".into();
        assert!(rotated.unwrap("draft", id, &relabelled).is_err());
        let fresh = rotated.wrap("draft", id, b"new generation").unwrap();
        assert_eq!(fresh.key_id, "next");
        assert!(old.unwrap("draft", id, &fresh).is_err());

        let guardian = Id::new_v4();
        let policy = Policy {
            guardians: [guardian].into(),
            recipients: [guardian].into(),
            threshold: 1,
            timing: Default::default(),
        };
        let key = old.random_key().unwrap();
        let code = old.split(id, &key, &policy).unwrap().remove(0);
        let verified = rotated
            .verify_code(
                id,
                guardian,
                &policy,
                &code.verifier_key_id,
                &code.verifier,
                &code.code,
            )
            .unwrap();
        assert_eq!(*rotated.combine(1, &[verified]).unwrap(), *key);
        assert!(
            retired
                .verify_code(
                    id,
                    guardian,
                    &policy,
                    &code.verifier_key_id,
                    &code.verifier,
                    &code.code
                )
                .is_err()
        );
    }
    #[test]
    fn threshold_shares_are_bound_to_actor_and_policy() {
        let c = crypto();
        let id = Id::new_v4();
        let key = c.random_key().unwrap();
        let mut p = Policy {
            guardians: (1..=5).map(Id::from_u128).collect(),
            recipients: [Id::from_u128(6)].into(),
            threshold: 3,
            timing: Default::default(),
        };
        let codes = c.split(id, &key, &p).unwrap();
        let mut shares = Vec::new();
        for code in codes.iter().take(3) {
            shares.push(
                c.verify_code(
                    id,
                    code.account_id,
                    &p,
                    &code.verifier_key_id,
                    &code.verifier,
                    &code.code,
                )
                .unwrap(),
            );
        }
        assert!(c.combine(3, &shares[..2]).is_err());
        assert_eq!(*c.combine(3, &shares).unwrap(), *key);
        let code = &codes[0];
        assert!(
            c.verify_code(
                id,
                codes[1].account_id,
                &p,
                &code.verifier_key_id,
                &code.verifier,
                &code.code
            )
            .is_err()
        );
        p.threshold = 2;
        assert!(
            c.verify_code(
                id,
                code.account_id,
                &p,
                &code.verifier_key_id,
                &code.verifier,
                &code.code
            )
            .is_err()
        );
    }
    #[test]
    fn single_approval_is_explicit_and_duplicate_indices_fail() {
        let c = crypto();
        let key = c.random_key().unwrap();
        let id = Id::new_v4();
        let who = Id::new_v4();
        let p = Policy {
            guardians: [who].into(),
            recipients: [who].into(),
            threshold: 1,
            timing: Default::default(),
        };
        let codes = c.split(id, &key, &p).unwrap();
        let code = &codes[0];
        let s = c
            .verify_code(
                id,
                who,
                &p,
                &code.verifier_key_id,
                &code.verifier,
                &code.code,
            )
            .unwrap();
        assert_eq!(*c.combine(1, std::slice::from_ref(&s)).unwrap(), *key);
        assert!(c.combine(2, &[s.clone(), s]).is_err());
    }
    #[tokio::test]
    async fn recovery_hash_rejects_other_credentials_and_unbounded_parameters() {
        let h = ArgonHasher::new(65536, 3, 1, 2).unwrap();
        let (id, token, hash) = h.issue().await.unwrap();
        assert!(h.verify(&token, id, &hash).await.unwrap());
        assert!(!h.verify(&token, Id::new_v4(), &hash).await.unwrap());
        assert!(
            !h.verify(&token, id, &hash.replace("m=65536", "m=4294967295"))
                .await
                .unwrap()
        );
    }
}
