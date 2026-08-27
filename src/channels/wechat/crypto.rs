use super::*;
// ── Crypto helpers ─────────────────────────────────────────────────────────────

pub(crate) fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut padded = data.to_vec();
    padded.extend(vec![padding as u8; padding]);
    padded
}

pub(crate) fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("Empty data".into());
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > data.len() {
        return Err("Invalid padding".into());
    }
    if data[data.len() - pad_len..]
        .iter()
        .any(|&b| b != pad_len as u8)
    {
        return Err("Invalid PKCS7 padding".into());
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

pub(crate) fn encrypt_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let padded = pkcs7_pad(plaintext, 16);
    let mut enc = Encryptor::<Aes128>::new(key.into());
    padded
        .chunks(16)
        .flat_map(|chunk| {
            let arr: [u8; 16] = chunk.try_into().unwrap();
            let mut block = aes::Block::from(arr);
            enc.encrypt_block_mut(&mut block);
            block.to_vec()
        })
        .collect()
}

#[allow(dead_code)]
pub(crate) fn decrypt_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if !ciphertext.len().is_multiple_of(16) {
        return Err(format!(
            "Ciphertext length {} is not a multiple of 16",
            ciphertext.len()
        ));
    }
    let mut dec = Decryptor::<Aes128>::new(key.into());
    let decrypted: Vec<u8> = ciphertext
        .chunks(16)
        .flat_map(|chunk| {
            let arr: [u8; 16] = chunk.try_into().unwrap();
            let mut block = aes::Block::from(arr);
            dec.decrypt_block_mut(&mut block);
            block.to_vec()
        })
        .collect();
    pkcs7_unpad(&decrypted)
}

