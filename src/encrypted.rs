use bytes::Bytes;

const ENCRYPTED_SIGNATURES: &[&[u8]] = &[
    b"EncryptedPackage",
    b"Microsoft_Container_",
    b"DRMContent",
    b"EncryptionInfo",
    b"EncryptedData",
    b"EncryptedDocument",
    b"ECMA-376 Encryption",
    b"msoffice",
];

/// Helper to check if a file is more likely encrypted rather than corrupted
///
/// (Used to handle the case where the LibreOffice error message is the same)
pub fn is_likely_encrypted(data: &Bytes) -> bool {
    let size = data.len();

    // File is empty, probably corrupted
    if size == 0 {
        return true;
    }

    // Read file header
    let header_len = std::cmp::min(512, size);
    let header = &data[..header_len];

    if header.len() < 4 {
        return false;
    }

    // Check for password protection signatures (File is probably encrypted)
    for signature in ENCRYPTED_SIGNATURES {
        if find_needle(header, signature) {
            return true;
        }
    }

    // Check for common corruption signs (ZIP-based file)
    if header.first() == Some(&b'P') && header.get(1) == Some(&b'K') {
        // Too small for valid ZIP (File is probably corrupted)
        if size < 22 {
            return false;
        }

        // Check for ZIP end record (File is probably corrupted)
        let end_record_start = size - 22;
        if size < end_record_start + 4 {
            return true;
        }

        // Invalid ZIP end record (File is probably corrupted)
        let end_record = &data[end_record_start..end_record_start + 4];
        if end_record != [0x50, 0x4b, 0x05, 0x06] {
            return true;
        }
    }

    false
}

fn find_needle(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
