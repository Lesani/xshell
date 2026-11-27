#![cfg(feature = "ring")]
use xshell_protocol::ring::device_code::DeviceCode;

#[test]
fn device_code_96_bit_all_indices_roundtrip_and_normalization() {
    for index in 0..4096u16 {
        let bits = ((index as u128) << 84)
            | ((index as u128) << 72)
            | ((index as u128) << 60)
            | ((index as u128) << 48)
            | ((index as u128) << 36)
            | ((index as u128) << 24)
            | ((index as u128) << 12)
            | index as u128;
        let bytes = bits.to_be_bytes();
        let code = DeviceCode::from_bytes(bytes[4..].try_into().unwrap());
        let text = code.format();
        assert_eq!(text.split('-').count(), 8);
        let parsed = DeviceCode::parse(&text.to_uppercase().replace('-', "  ")).unwrap();
        assert_eq!(parsed.secret().as_bytes(), &bytes[4..]);
        assert!(!format!("{code:?}").contains(text.as_str()));
    }
    assert!(DeviceCode::parse("abacus").is_err());
}

#[test]
fn eff_source_subset_hash_license_and_ascii_tokens() {
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;
    use xshell_protocol::ring::device_code::{DEVICE_CODE_NOTICE, DEVICE_CODE_WORDS};
    let original = include_bytes!("../src/ring/device_code/eff-large-original.txt");
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex(&Sha256::digest(original)),
        "addd35536511597a02fa0a9ff1e5284677b8883b83e986e43f15a3db996b903e"
    );
    assert_eq!(
        hex(&Sha256::digest(DEVICE_CODE_WORDS)),
        "e74126bde834e51fa9a56ae79eb2776571e7f7ae73ea36a468cd09b08fff285e"
    );
    let source: Vec<_> = std::str::from_utf8(original)
        .unwrap()
        .lines()
        .map(|line| line.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(source.len(), 7776);
    let eligible: Vec<_> = source.into_iter().filter(|w| !w.contains('-')).collect();
    assert_eq!(eligible.len(), 7772);
    let words: Vec<_> = DEVICE_CODE_WORDS.lines().collect();
    assert_eq!(words.len(), 4096);
    assert_eq!(words.iter().collect::<HashSet<_>>().len(), 4096);
    for (i, word) in words.iter().enumerate() {
        assert_eq!(*word, eligible[i * 7772 / 4096]);
        assert!((3..=9).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_lowercase()));
    }
    assert!(
        DEVICE_CODE_NOTICE.contains("CC-BY-4.0") && DEVICE_CODE_NOTICE.contains("Joseph Bonneau")
    );
    assert_eq!(
        DeviceCode::from_bytes([0; 12]).format().as_str(),
        "abacus-abacus-abacus-abacus-abacus-abacus-abacus-abacus"
    );
    assert_eq!(
        DeviceCode::from_bytes([255; 12]).format().as_str(),
        "zoology-zoology-zoology-zoology-zoology-zoology-zoology-zoology"
    );
}
