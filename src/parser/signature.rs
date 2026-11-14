use sha2::{Digest, Sha256};

/// 起零数据 (Istero / ISAS DATA) SHA256 动态签名。
pub fn generate_istero_signature(
    token: &str,
    app_secret: &str,
    timestamp: impl ToString,
    nonce: &str,
    params: &[(String, String)],
) -> String {
    const IGNORED: &[&str] = &["sign", "token", "timestamp", "nonce"];
    let mut entries: Vec<&(String, String)> = params
        .iter()
        .filter(|(key, value)| !IGNORED.contains(&key.as_str()) && !value.is_empty())
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let query = entries
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let raw = format!("{token}{app_secret}{}{nonce}{query}", timestamp.to_string());
    hex::encode(Sha256::digest(raw.as_bytes())).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_implementation() {
        let params = vec![
            ("b".into(), "2".into()),
            ("c".into(), "test".into()),
            ("a".into(), "1".into()),
        ];
        let got = generate_istero_signature(
            "Tokenabc123",
            "AppSecretxyz789",
            1_751_811_509u64,
            "Nonce5e2a8d7f1c3b90",
            &params,
        );
        let expected = "c28025e3cd1fd02bdbc3cebc08dc23f21e1b62585c933ae9e50e9fe44ea863fa";
        assert_eq!(got.len(), 64);
        assert_eq!(got, expected);
    }
}
