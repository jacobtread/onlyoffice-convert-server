const ENCRYPTED_SIGNATURES: &[&[u8]] = &[
    b"EncryptedPackage",
    b"Microsoft_Container_",
    b"DRMContent",
    b"EncryptionInfo",
    b"EncryptedData",
    b"EncryptedDocument",
    b"ECMA-376 Encryption",
    b"msoffice",
    b"encrypt",
];

#[derive(Debug)]
pub enum FileCondition {
    Normal,
    LikelyCorrupted,
    LikelyEncrypted,
}

/// Helper to check the condition of a file for better corruption and encryption error
/// checking
pub fn get_file_condition(data: &[u8]) -> FileCondition {
    let size = data.len();

    // File is empty, probably corrupted
    if size == 0 {
        return FileCondition::LikelyCorrupted;
    }

    // Read file header (Not really header, just first 32KB of the file)
    let header_len = std::cmp::min(1024 * 32, size);
    let header = &data[..header_len];

    if header.len() < 4 {
        return FileCondition::LikelyCorrupted;
    }

    // Check for password protection signatures (File is probably encrypted)
    for signature in ENCRYPTED_SIGNATURES {
        if find_needle(header, signature) {
            return FileCondition::LikelyEncrypted;
        }

        // Check UTF-16 LE version
        let utf16_le = to_utf16_le(signature);
        if find_needle(header, &utf16_le) {
            return FileCondition::LikelyEncrypted;
        }

        // Check UTF-16 BE version
        let utf16_be = to_utf16_be(signature);
        if find_needle(header, &utf16_be) {
            return FileCondition::LikelyEncrypted;
        }
    }

    // Check for common corruption signs (ZIP-based file)
    if header.first() == Some(&b'P') && header.get(1) == Some(&b'K') {
        // Too small for valid ZIP (File is probably corrupted)
        if size < 22 {
            return FileCondition::LikelyCorrupted;
        }

        // Check for ZIP end record (File is probably corrupted)
        let end_record_start = size - 22;
        if size < end_record_start + 4 {
            return FileCondition::LikelyCorrupted;
        }

        // Invalid ZIP end record (File is probably corrupted)
        let end_record = &data[end_record_start..end_record_start + 4];
        if end_record != [0x50, 0x4b, 0x05, 0x06] {
            return FileCondition::LikelyCorrupted;
        }
    }

    FileCondition::Normal
}

fn find_needle(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Convert ASCII bytes to UTF-16 Little Endian
fn to_utf16_le(ascii: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(ascii.len() * 2);
    for &byte in ascii {
        result.push(byte);
        result.push(0);
    }
    result
}

/// Convert ASCII bytes to UTF-16 Big Endian
fn to_utf16_be(ascii: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(ascii.len() * 2);
    for &byte in ascii {
        result.push(0);
        result.push(byte);
    }
    result
}
