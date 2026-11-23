//! Подпись выпусков (вызывает scripts/release.sh). Ключ — hex-сид ed25519 в файле, не выводится.
//!   release_sign pubkey <key-file>                            — публичный ключ (hex)
//!   release_sign sign <key-file> <version> <file> <sha256> <size> — подпись (hex)
use ed25519_dalek::{Signer, SigningKey};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let seed_hex = std::fs::read_to_string(&a[1]).expect("ключ").trim().to_string();
    assert_eq!(seed_hex.len(), 64, "ключ — 32 байта в hex");
    let mut seed = [0u8; 32];
    for i in 0..32 {
        seed[i] = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).expect("hex");
    }
    let key = SigningKey::from_bytes(&seed);
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    match a[0].as_str() {
        "pubkey" => println!("{}", hex(key.verifying_key().as_bytes())),
        "sign" => {
            let info = proto::ReleaseInfo {
                version: a[2].clone(),
                notes: String::new(),
                file: a[3].clone(),
                sha256: a[4].clone(),
                size: a[5].parse().expect("size"),
                installer: None,
                min_server: None,
                sig: None,
            };
            assert_eq!(key.verifying_key().as_bytes(), &proto::RELEASE_PUBKEY, "ключ не совпадает с proto::RELEASE_PUBKEY");
            let sig = hex(&key.sign(&info.signing_message()).to_bytes());
            let signed = proto::ReleaseInfo { sig: Some(sig.clone()), ..info };
            assert!(signed.verify(), "подпись не проходит проверку клиента");
            println!("{sig}");
        }
        _ => panic!("pubkey|sign"),
    }
}
