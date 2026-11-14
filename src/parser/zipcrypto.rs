use chrono::{Datelike, Local, Timelike};
use once_cell::sync::Lazy;
use rand::RngCore;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

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
    update_crc32(&mut crc, bytes);
    crc ^ 0xffff_ffff
}

fn update_crc32(crc: &mut u32, bytes: &[u8]) {
    for byte in bytes {
        *crc = CRC_TABLE[((*crc ^ u32::from(*byte)) & 0xff) as usize] ^ (*crc >> 8);
    }
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
        for byte in password.bytes() {
            state.update(byte);
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

pub struct ZipFileEntry {
    pub name: String,
    pub path: PathBuf,
}

/// Writes stored, encrypted ZIP32 entries using one 64 KiB payload buffer.
/// Callers running inside Tokio should execute this CPU/IO work in spawn_blocking.
pub fn write_encrypted_zip_file(
    entries: &[ZipFileEntry],
    password: &str,
    output_path: &Path,
    max_bytes: u64,
) -> io::Result<u64> {
    let mut expected_size = 22u64;
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
    if entries.is_empty() || entries.len() > u16::MAX as usize {
        return Err(invalid("ZIP32 文件数量必须为1至65535"));
    }
    let mut sizes = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.name.is_empty()
            || entry.name.len() > u16::MAX as usize
            || entry.name.starts_with(['/', '\\'])
            || entry.name.contains([':', '\0'])
            || entry.name.split(['/', '\\']).any(|part| part == "..")
        {
            return Err(invalid("ZIP 文件名无效"));
        }
        let metadata = std::fs::symlink_metadata(&entry.path)?;
        if !metadata.is_file() || metadata.len() > (u32::MAX - 12) as u64 {
            return Err(invalid("ZIP 输入必须为ZIP32支持的普通文件"));
        }
        let size = metadata.len();
        expected_size = expected_size
            .checked_add(size + 88 + 2 * entry.name.len() as u64)
            .ok_or_else(|| invalid("ZIP 文件长度溢出"))?;
        sizes.push(size as u32);
    }
    if expected_size > max_bytes || expected_size > u32::MAX as u64 {
        return Err(invalid("加密压缩包超过大小上限"));
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)?;
    let mut writer = BufWriter::with_capacity(64 * 1024, file);
    let result =
        write_zip_files(&mut writer, entries, &sizes, password).and_then(|()| writer.flush());
    drop(writer);
    if let Err(error) = result {
        let _ = std::fs::remove_file(output_path);
        return Err(error);
    }
    Ok(expected_size)
}

fn write_zip_files(
    writer: &mut impl Write,
    entries: &[ZipFileEntry],
    sizes: &[u32],
    password: &str,
) -> io::Result<()> {
    let (time, date) = dos_date_time();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut central = Vec::with_capacity(entries.len());
    let mut offset = 0u32;
    for (entry, &size) in entries.iter().zip(sizes) {
        let mut input = File::open(&entry.path)?;
        let mut crc = 0xffff_ffff;
        let mut total = 0u64;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > size as u64 {
                return Err(io::Error::other("ZIP 输入在打包时发生变化"));
            }
            update_crc32(&mut crc, &buffer[..count]);
        }
        if total != size as u64 {
            return Err(io::Error::other("ZIP 输入在打包时发生变化"));
        }
        crc ^= 0xffff_ffff;
        input.rewind()?;
        let mut local = Vec::with_capacity(30);
        put_u32(&mut local, 0x0403_4b50);
        put_u16(&mut local, 20);
        put_u16(&mut local, 0x0801);
        put_u16(&mut local, 0);
        put_u16(&mut local, time);
        put_u16(&mut local, date);
        put_u32(&mut local, crc);
        put_u32(&mut local, size + 12);
        put_u32(&mut local, size);
        put_u16(&mut local, entry.name.len() as u16);
        put_u16(&mut local, 0);
        writer.write_all(&local)?;
        writer.write_all(entry.name.as_bytes())?;
        // Password encryption is initialized separately for every entry.
        // The first eleven random bytes and CRC check byte match ZipCrypto.
        central.push((offset, crc, size));
        offset += 30 + entry.name.len() as u32 + 12 + size;
        write_zip_payload(writer, &mut input, &mut buffer, size, crc, password)?;
    }
    let central_start = offset;
    for (entry, &(entry_offset, crc, size)) in entries.iter().zip(&central) {
        let mut header = Vec::with_capacity(46);
        put_u32(&mut header, 0x0201_4b50);
        put_u16(&mut header, 20);
        put_u16(&mut header, 20);
        put_u16(&mut header, 0x0801);
        put_u16(&mut header, 0);
        put_u16(&mut header, time);
        put_u16(&mut header, date);
        put_u32(&mut header, crc);
        put_u32(&mut header, size + 12);
        put_u32(&mut header, size);
        put_u16(&mut header, entry.name.len() as u16);
        put_u16(&mut header, 0);
        put_u16(&mut header, 0);
        put_u16(&mut header, 0);
        put_u16(&mut header, 0);
        put_u32(&mut header, 0);
        put_u32(&mut header, entry_offset);
        writer.write_all(&header)?;
        writer.write_all(entry.name.as_bytes())?;
        offset += 46 + entry.name.len() as u32;
    }
    let mut end = Vec::with_capacity(22);
    put_u32(&mut end, 0x0605_4b50);
    put_u16(&mut end, 0);
    put_u16(&mut end, 0);
    put_u16(&mut end, entries.len() as u16);
    put_u16(&mut end, entries.len() as u16);
    put_u32(&mut end, offset - central_start);
    put_u32(&mut end, central_start);
    put_u16(&mut end, 0);
    writer.write_all(&end)
}

fn write_zip_payload(
    writer: &mut impl Write,
    input: &mut File,
    buffer: &mut [u8],
    size: u32,
    crc: u32,
    password: &str,
) -> io::Result<()> {
    let mut crypto = ZipCrypto::new(password);
    let mut header = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut header[..11]);
    header[11] = (crc >> 24) as u8;
    for byte in &mut header {
        *byte = crypto.encrypt_byte(*byte);
    }
    writer.write_all(&header)?;
    let mut remaining = size as usize;
    let mut actual_crc = 0xffff_ffff;
    while remaining > 0 {
        let count = remaining.min(buffer.len());
        input.read_exact(&mut buffer[..count])?;
        update_crc32(&mut actual_crc, &buffer[..count]);
        for byte in &mut buffer[..count] {
            *byte = crypto.encrypt_byte(*byte);
        }
        writer.write_all(&buffer[..count])?;
        remaining -= count;
    }
    if actual_crc ^ 0xffff_ffff != crc || input.read(&mut [0u8; 1])? != 0 {
        return Err(io::Error::other("ZIP 输入在打包时发生变化"));
    }
    Ok(())
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
    fn streams_multiple_entries_across_buffer_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.bin");
        let second = directory.path().join("second.bin");
        let contents: Vec<u8> = (0..(64 * 1024 + 37))
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(&first, &contents).unwrap();
        std::fs::write(&second, []).unwrap();
        let path = directory.path().join("stream.zip");
        let entries = [
            ZipFileEntry {
                name: "001.jpg".into(),
                path: first,
            },
            ZipFileEntry {
                name: "002.jpg".into(),
                path: second,
            },
        ];
        let size = write_encrypted_zip_file(&entries, "Abcd2345", &path, 1024 * 1024).unwrap();
        let zip = std::fs::read(&path).unwrap();
        assert_eq!(size, zip.len() as u64);
        let mut offset = 0usize;
        for expected in [&contents[..], &[][..]] {
            assert_eq!(&zip[offset..offset + 4], b"PK\x03\x04");
            let compressed =
                u32::from_le_bytes(zip[offset + 18..offset + 22].try_into().unwrap()) as usize;
            let name_size =
                u16::from_le_bytes(zip[offset + 26..offset + 28].try_into().unwrap()) as usize;
            let start = offset + 30 + name_size;
            let mut crypto = ZipCrypto::new("Abcd2345");
            let plain: Vec<u8> = zip[start..start + compressed]
                .iter()
                .map(|cipher| {
                    let byte = *cipher ^ crypto.magic();
                    crypto.update(byte);
                    byte
                })
                .collect();
            assert_eq!(&plain[12..], expected);
            offset = start + compressed;
        }
        assert_eq!(&zip[offset..offset + 4], b"PK\x01\x02");
    }

    #[test]
    fn rejects_oversized_streaming_zip_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.bin");
        std::fs::write(&input, [1, 2, 3]).unwrap();
        let path = directory.path().join("output.zip");
        let mut entries = [ZipFileEntry {
            name: "001.jpg".into(),
            path: input,
        }];
        assert!(write_encrypted_zip_file(&entries, "pass", &path, 10).is_err());
        assert!(!path.exists());
        entries[0].name = "../001.jpg".into();
        assert!(write_encrypted_zip_file(&entries, "pass", &path, 1024).is_err());
        assert!(!path.exists());
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

        let input = directory.path().join("image.jpg");
        std::fs::write(&input, vec![123u8; 64 * 1024 + 37]).unwrap();
        let stream_path = directory.path().join("stream.zip");
        let unicode_password = "中文Abcd2345";
        write_encrypted_zip_file(
            &[ZipFileEntry {
                name: "图片.jpg".into(),
                path: input,
            }],
            unicode_password,
            &stream_path,
            1024 * 1024,
        )
        .unwrap();
        let output = Command::new("python").arg("-c").arg(
            "import sys,zipfile;z=zipfile.ZipFile(sys.argv[1]);assert z.read('图片.jpg',pwd=sys.argv[2].encode('utf-8'))==bytes([123])*(65536+37)"
        ).arg(stream_path).arg(unicode_password).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
