use chrono::{Datelike, Local, Timelike};
use once_cell::sync::Lazy;
use rand::RngCore;

static CRC_TABLE: Lazy<[u32; 256]> = Lazy::new(|| {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    table
});

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc = CRC_TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

struct ZipCrypto {
    k0: u32,
    k1: u32,
    k2: u32,
}

impl ZipCrypto {
    fn new(password: &str) -> Self {
        let mut state = Self {
            k0: 0x1234_5678,
            k1: 0x2345_6789,
            k2: 0x3456_7890,
        };
        for unit in password.encode_utf16() {
            state.update(unit as u8);
        }
        state
    }

    fn update(&mut self, byte: u8) {
        self.k0 = CRC_TABLE[((self.k0 ^ u32::from(byte)) & 0xff) as usize] ^ (self.k0 >> 8);
        self.k1 = self
            .k1
            .wrapping_add(self.k0 & 0xff)
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        self.k2 = CRC_TABLE[((self.k2 ^ (self.k1 >> 24)) & 0xff) as usize] ^ (self.k2 >> 8);
    }

    fn magic(&self) -> u8 {
        let temp = (self.k2 | 2) as u64;
        ((temp.wrapping_mul(temp ^ 1) & 0xffff_ffff) >> 8) as u8
    }

    fn encrypt_byte(&mut self, plain: u8) -> u8 {
        let cipher = plain ^ self.magic();
        self.update(plain);
        cipher
    }
}

pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

fn dos_date_time() -> (u16, u16) {
    let now = Local::now();
    let time =
        ((now.hour() & 0x1f) << 11) | ((now.minute() & 0x3f) << 5) | ((now.second() / 2) & 0x1f);
    let date = (((now.year() - 1980) & 0x7f) << 9)
        | (((now.month() & 0xf) as i32) << 5)
        | now.day() as i32;
    (time as u16, date as u16)
}

fn put_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

pub fn create_encrypted_zip(entries: &[ZipEntry], password: &str) -> Vec<u8> {
    let mut local_parts = Vec::new();
    let mut central_parts = Vec::new();
    let mut offset = 0u32;
    let (time, date) = dos_date_time();
    let mut rng = rand::thread_rng();

    for entry in entries {
        let name = entry.name.as_bytes();
        let crc = crc32(&entry.data);
        let mut crypto = ZipCrypto::new(password);
        let mut encrypted = Vec::with_capacity(entry.data.len() + 12);
        for _ in 0..11 {
            encrypted.push(crypto.encrypt_byte(rng.next_u32() as u8));
        }
        encrypted.push(crypto.encrypt_byte((crc >> 24) as u8));
        for byte in &entry.data {
            encrypted.push(crypto.encrypt_byte(*byte));
        }

        let mut local = Vec::with_capacity(30);
        put_u32(&mut local, 0x0403_4b50);
        put_u16(&mut local, 20);
        put_u16(&mut local, 0x0001);
        put_u16(&mut local, 0);
        put_u16(&mut local, time);
        put_u16(&mut local, date);
        put_u32(&mut local, crc);
        put_u32(&mut local, encrypted.len() as u32);
        put_u32(&mut local, entry.data.len() as u32);
        put_u16(&mut local, name.len() as u16);
        put_u16(&mut local, 0);
        local_parts.extend_from_slice(&local);
        local_parts.extend_from_slice(name);
        local_parts.extend_from_slice(&encrypted);

        let mut central = Vec::with_capacity(46);
        put_u32(&mut central, 0x0201_4b50);
        put_u16(&mut central, 20);
        put_u16(&mut central, 20);
        put_u16(&mut central, 0x0001);
        put_u16(&mut central, 0);
        put_u16(&mut central, time);
        put_u16(&mut central, date);
        put_u32(&mut central, crc);
        put_u32(&mut central, encrypted.len() as u32);
        put_u32(&mut central, entry.data.len() as u32);
        put_u16(&mut central, name.len() as u16);
        put_u32(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u32(&mut central, 0);
        put_u32(&mut central, offset);
        central_parts.extend_from_slice(&central);
        central_parts.extend_from_slice(name);

        offset =
            offset.wrapping_add(local.len() as u32 + name.len() as u32 + encrypted.len() as u32);
    }

    let central_size = central_parts.len() as u32;
    let local_size = local_parts.len() as u32;
    let mut end = Vec::with_capacity(22);
    put_u32(&mut end, 0x0605_4b50);
    put_u16(&mut end, 0);
    put_u16(&mut end, 0);
    put_u16(&mut end, entries.len() as u16);
    put_u16(&mut end, entries.len() as u16);
    put_u32(&mut end, central_size);
    put_u32(&mut end, local_size);
    put_u16(&mut end, 0);

    local_parts.extend_from_slice(&central_parts);
    local_parts.extend_from_slice(&end);
    local_parts
}

pub fn random_password(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| CHARS[usize::from(byte) % CHARS.len()] as char)
        .collect()
}

pub fn encrypt_entry_for_test(plain: &[u8], password: &str, header_plain: &[u8; 12]) -> Vec<u8> {
    let crc = crc32(plain);
    let mut crypto = ZipCrypto::new(password);
    let mut out = Vec::with_capacity(plain.len() + 12);
    for byte in &header_plain[..11] {
        out.push(crypto.encrypt_byte(*byte));
    }
    out.push(crypto.encrypt_byte((crc >> 24) as u8));
    for byte in plain {
        out.push(crypto.encrypt_byte(*byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn writes_valid_zip_signatures() {
        let zip = create_encrypted_zip(
            &[ZipEntry {
                name: "001.jpg".into(),
                data: b"fake-image-bytes-1".to_vec(),
            }],
            "Abcd2345",
        );
        assert_eq!(&zip[..4], b"PK\x03\x04");
        assert!(zip.windows(4).any(|w| w == b"PK\x01\x02"));
        assert!(zip.windows(4).any(|w| w == b"PK\x05\x06"));
    }

    #[test]
    fn random_password_is_alphanumeric() {
        let pwd = random_password(8);
        assert_eq!(pwd.len(), 8);
        assert!(pwd.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    #[ignore = "requires Python zipfile for interoperability verification"]
    fn python_zipfile_can_decrypt_generated_archive() {
        use std::process::Command;

        let password = "Abcd2345";
        let zip = create_encrypted_zip(
            &[ZipEntry {
                name: "001.jpg".into(),
                data: b"fake-image-bytes-1".to_vec(),
            }],
            password,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.zip");
        std::fs::write(&path, zip).unwrap();
        let script = format!(
            "import zipfile;z=zipfile.ZipFile(r'{}');d=z.read('001.jpg',pwd=b'{}');assert d==b'fake-image-bytes-1';print('PYZIP_OK')",
            path.to_string_lossy().replace('\\', "/"),
            password
        );
        let output = Command::new("python").arg("-c").arg(script).output();
        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("python failed to start: {error}"),
        };
        assert!(
            output.status.success(),
            "python zipfile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("PYZIP_OK"));
    }
}
