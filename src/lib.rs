use crossbeam_channel::Sender;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct VanityResult {
    pub public_key_base58: String,
    pub secret_key_bytes: [u8; 32],
    pub full_keypair_bytes: Vec<u8>,
    pub attempts: u64,
}

#[derive(Debug, Clone)]
pub struct VanityConfig {
    pub suffix: String,
    pub case_insensitive: bool,
    pub num_threads: usize,
}

impl Default for VanityConfig {
    fn default() -> Self {
        Self {
            suffix: String::new(),
            case_insensitive: false,
            num_threads: num_cpus::get(),
        }
    }
}

#[inline(always)]
fn matches_suffix(pubkey_bytes: &[u8; 32], suffix: &str, case_insensitive: bool) -> bool {
    let encoded = bs58::encode(pubkey_bytes).into_string();
    if case_insensitive {
        encoded
            .to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase())
    } else {
        encoded.ends_with(suffix)
    }
}

pub fn validate_suffix(suffix: &str) -> Result<(), String> {
    const BASE58_ALPHABET: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    if suffix.is_empty() {
        return Err("Suffix must not be empty".to_string());
    }

    if suffix.len() > 8 {
        return Err(
            "Suffix too long (max 8 characters). Longer suffixes may take unreasonably long."
                .to_string(),
        );
    }

    for (i, ch) in suffix.chars().enumerate() {
        if !BASE58_ALPHABET.contains(ch) {
            return Err(format!(
                "Character '{}' at position {} is not valid base58. Note: base58 excludes 0, O, I, l",
                ch, i
            ));
        }
    }

    Ok(())
}

pub fn estimate_attempts(suffix_len: usize, case_insensitive: bool) -> f64 {
    let charset_size: f64 = if case_insensitive { 34.0 } else { 58.0 };
    charset_size.powi(suffix_len as i32)
}

pub fn generate_vanity_keypair(
    config: &VanityConfig,
    found: Arc<AtomicBool>,
    attempts_counter: Arc<AtomicU64>,
    progress_tx: Option<Sender<u64>>,
) -> Option<VanityResult> {
    let config = config.clone();
    let num_threads = config.num_threads;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let suffix = config.suffix.clone();
            let case_insensitive = config.case_insensitive;
            let found = Arc::clone(&found);
            let attempts_counter = Arc::clone(&attempts_counter);
            let progress_tx = progress_tx.clone();

            std::thread::spawn(move || -> Option<VanityResult> {
                let mut rng = OsRng;
                let mut local_attempts: u64 = 0;
                const BATCH_SIZE: u64 = 1024;

                loop {
                    for _ in 0..BATCH_SIZE {
                        if found.load(Ordering::Relaxed) {
                            return None;
                        }

                        let signing_key = SigningKey::generate(&mut rng);
                        let verifying_key = signing_key.verifying_key();
                        let pubkey_bytes: &[u8; 32] = verifying_key.as_bytes();

                        local_attempts += 1;

                        if matches_suffix(pubkey_bytes, &suffix, case_insensitive) {
                            found.store(true, Ordering::SeqCst);

                            let total = attempts_counter
                                .fetch_add(local_attempts, Ordering::Relaxed)
                                + local_attempts;

                            let mut full_keypair = Vec::with_capacity(64);
                            full_keypair.extend_from_slice(&signing_key.to_bytes());
                            full_keypair.extend_from_slice(verifying_key.as_bytes());

                            return Some(VanityResult {
                                public_key_base58: bs58::encode(pubkey_bytes).into_string(),
                                secret_key_bytes: signing_key.to_bytes(),
                                full_keypair_bytes: full_keypair,
                                attempts: total,
                            });
                        }
                    }

                    attempts_counter.fetch_add(BATCH_SIZE, Ordering::Relaxed);
                    local_attempts = 0;

                    if let Some(ref tx) = progress_tx {
                        let _ = tx.try_send(BATCH_SIZE);
                    }
                }
            })
        })
        .collect();

    let mut result: Option<VanityResult> = None;
    for handle in handles {
        if let Ok(Some(r)) = handle.join() {
            result = Some(r);
        }
    }

    result
}