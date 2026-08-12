use base64::Engine as _;

pub(super) fn is_gpt_signature(signature: &str) -> bool {
    const FERNET_MIN_BYTES: usize = 73;
    const FERNET_MAX_BYTES: usize = 32 * 1024 * 1024;
    if !signature.starts_with("gAAAA") || signature.len() > FERNET_MAX_BYTES * 2 {
        return false;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(signature)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signature));
    let Ok(decoded) = decoded else {
        return false;
    };
    if decoded.len() < FERNET_MIN_BYTES
        || decoded.len() > FERNET_MAX_BYTES
        || decoded.first() != Some(&0x80)
    {
        return false;
    }
    let ciphertext_bytes = decoded.len().saturating_sub(57);
    ciphertext_bytes > 0 && ciphertext_bytes.is_multiple_of(16)
}
