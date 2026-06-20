//! Known-answer tests anchoring the implementation to fixed external values.
//!
//! The mnemonic vectors are the canonical BIP39 test vectors. Stingle's
//! `MnemonicUtils` implements the same entropy→words algorithm over the same
//! 2048-word English wordlist, so matching these proves byte-compatibility of
//! the recovery-phrase encoding with the Android client.

use stingle_crypto::mnemonic;

const ALL_ZERO_16: [u8; 16] = [0u8; 16];
const ALL_FF_16: [u8; 16] = [0xffu8; 16];
const ALL_ZERO_32: [u8; 32] = [0u8; 32];
const ALL_FF_32: [u8; 32] = [0xffu8; 32];

#[test]
fn bip39_known_answers() {
    assert_eq!(
        mnemonic::entropy_to_mnemonic(&ALL_ZERO_16).unwrap(),
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
    );
    assert_eq!(
        mnemonic::entropy_to_mnemonic(&ALL_FF_16).unwrap(),
        "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong"
    );
    // 32-byte entropy → 24 words (the private-key case).
    assert_eq!(
        mnemonic::entropy_to_mnemonic(&ALL_ZERO_32).unwrap(),
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
         abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
         abandon art"
    );
    assert_eq!(
        mnemonic::entropy_to_mnemonic(&ALL_FF_32).unwrap(),
        "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo \
         zoo vote"
    );
}

#[test]
fn bip39_decode_matches_encode() {
    for entropy in [
        ALL_ZERO_16.to_vec(),
        ALL_FF_16.to_vec(),
        ALL_ZERO_32.to_vec(),
        ALL_FF_32.to_vec(),
    ] {
        let phrase = mnemonic::entropy_to_mnemonic(&entropy).unwrap();
        assert_eq!(mnemonic::mnemonic_to_entropy(&phrase).unwrap(), entropy);
    }
}

#[test]
fn bip39_rejects_bad_checksum() {
    // Valid 24-word phrase with the last word swapped to break the checksum.
    let phrase = mnemonic::entropy_to_mnemonic(&ALL_ZERO_32).unwrap();
    let mut words: Vec<&str> = phrase.split(' ').collect();
    *words.last_mut().unwrap() = "zoo"; // wrong checksum word
    let bad = words.join(" ");
    assert!(!mnemonic::is_valid(&bad));
}

#[test]
fn bip39_rejects_unknown_word() {
    assert!(mnemonic::mnemonic_to_entropy("notaword abandon about").is_err());
}
