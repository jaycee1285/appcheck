//! Select one regular file; never unpack archive paths onto the filesystem.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;

fn is_elf(reader: &mut impl Read, arch: &str) -> Result<bool> {
    let mut header = [0; 64];
    let mut bytes = Vec::new();
    reader.take(64).read_to_end(&mut bytes)?;
    if bytes.len() != 64 {
        return Ok(false);
    }
    header.copy_from_slice(&bytes);
    let machine: u16 = if arch == "x86_64" { 62 } else { 183 };
    Ok(&header[..6] == b"\x7fELF\x02\x01"
        && u16::from_le_bytes([header[18], header[19]]) == machine
        && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3))
}

fn tar_binaries(reader: impl Read, arch: &str) -> Result<Vec<String>> {
    let mut archive = tar::Archive::new(reader.take(2 * 1024 * 1024 * 1024 + 1));
    let mut names = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file()
            || !(64..=MAX_BINARY_BYTES).contains(&entry.size())
        {
            continue;
        }
        let raw_name = String::from_utf8(entry.path_bytes().into_owned())?;
        let name = raw_name.strip_prefix("./").unwrap_or(&raw_name).to_string();
        if validate_member(&name).is_ok() && is_elf(&mut entry, arch)? {
            ensure!(
                !names.contains(&name),
                "Archive contains duplicate executable {name}"
            );
            names.push(name);
        }
    }
    let mut reader = archive.into_inner();
    io::copy(&mut reader, &mut io::sink())?;
    ensure!(reader.limit() > 0, "Expanded tar exceeds 2 GiB scan limit");
    Ok(names)
}

pub(crate) fn binaries(artifact: &Path, installer: &str, arch: &str) -> Result<Vec<String>> {
    let file = fs::File::open(artifact)?;
    match installer {
        "tar.gz" => tar_binaries(flate2::read::GzDecoder::new(file), arch),
        "tar.xz" => tar_binaries(xz2::read::XzDecoder::new(file), arch),
        "zip" => {
            let mut archive = zip::ZipArchive::new(file)?;
            unique_zip_entries(artifact, archive.central_directory_start(), archive.len())?;
            let mut names = Vec::new();
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
                if entry.is_dir()
                    || (kind != 0 && kind != 0o100000)
                    || !(64..=MAX_BINARY_BYTES).contains(&entry.size())
                {
                    continue;
                }
                let name = entry.name().to_string();
                if validate_member(&name).is_ok() && is_elf(&mut entry, arch)? {
                    names.push(name);
                }
            }
            Ok(names)
        }
        _ => bail!("Unsupported archive installer"),
    }
}

pub fn validate_member(member: &str) -> Result<()> {
    ensure!(
        !member.is_empty()
            && member.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            }),
        "Archive member must be an exact relative path without '.', '..', globs or backslashes"
    );
    Ok(())
}

fn copy_member(reader: impl Read, size: u64, target: &Path) -> Result<()> {
    ensure!(
        size >= 64 && size <= MAX_BINARY_BYTES,
        "Archive binary must be between 64 bytes and 512 MiB"
    );
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)?;
    let count = io::copy(&mut reader.take(MAX_BINARY_BYTES + 1), &mut file)?;
    ensure!(
        count == size,
        "Extracted size does not match archive header"
    );
    Ok(())
}

fn extract_tar(reader: impl Read, member: &str, target: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader.take(2 * 1024 * 1024 * 1024 + 1));
    let mut found = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let raw = entry.path_bytes();
        let canonical = raw.strip_prefix(b"./").unwrap_or(&raw);
        if canonical != member.as_bytes() {
            continue;
        }
        ensure!(!found, "Archive contains duplicate member {member}");
        ensure!(
            entry.header().entry_type().is_file(),
            "Archive member is not a regular file: {member}"
        );
        let size = entry.size();
        copy_member(entry, size, target)?;
        found = true;
    }
    // Read through the compression trailer, including its checksum.
    let mut reader = archive.into_inner();
    io::copy(&mut reader, &mut io::sink())?;
    ensure!(reader.limit() > 0, "Expanded tar exceeds 2 GiB scan limit");
    ensure!(found, "Archive has no exact member {member}");
    Ok(())
}

fn unique_zip_entries(artifact: &Path, start: u64, unique_count: usize) -> Result<()> {
    // ZipArchive indexes by name, silently collapsing duplicate central-directory
    // entries. Compare its count with the raw directory before selecting a file.
    let mut file = fs::File::open(artifact)?;
    file.seek(SeekFrom::Start(start))?;
    for count in 0..=unique_count {
        let mut signature = [0; 4];
        file.read_exact(&mut signature)?;
        if signature != *b"PK\x01\x02" {
            ensure!(count == unique_count, "ZIP directory count mismatch");
            return Ok(());
        }
        ensure!(count < unique_count, "ZIP contains duplicate filenames");
        let mut header = [0; 42];
        file.read_exact(&mut header)?;
        let trailing: u64 = [24, 26, 28]
            .iter()
            .map(|&i| u16::from_le_bytes([header[i], header[i + 1]]) as u64)
            .sum();
        file.seek(SeekFrom::Current(trailing as i64))?;
    }
    unreachable!()
}

pub fn extract(artifact: &Path, installer: &str, member: &str, target: &Path) -> Result<()> {
    validate_member(member)?;
    let file = fs::File::open(artifact)?;
    match installer {
        "tar.gz" => extract_tar(flate2::read::GzDecoder::new(file), member, target),
        "tar.xz" => extract_tar(xz2::read::XzDecoder::new(file), member, target),
        "zip" => {
            let mut archive = zip::ZipArchive::new(file).context("Cannot read ZIP archive")?;
            unique_zip_entries(artifact, archive.central_directory_start(), archive.len())?;
            let mut found = false;
            for index in 0..archive.len() {
                let entry = archive.by_index(index)?;
                if entry.name_raw() != member.as_bytes() {
                    continue;
                }
                ensure!(!found, "Archive contains duplicate member {member}");
                let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
                ensure!(
                    !entry.is_dir() && (kind == 0 || kind == 0o100000),
                    "Archive member is not a regular file: {member}"
                );
                let size = entry.size();
                copy_member(entry, size, target)?;
                found = true;
            }
            ensure!(found, "Archive has no exact member {member}");
            Ok(())
        }
        _ => bail!("Unsupported archive installer: {installer}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn tar_bytes(entries: &[(&str, tar::EntryType, &[u8])]) -> Result<Vec<u8>> {
        let mut archive = tar::Builder::new(Vec::new());
        for (name, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(*kind);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("outside")?;
            }
            header.set_cksum();
            archive.append_data(&mut header, name, *data)?;
        }
        Ok(archive.into_inner()?)
    }

    #[test]
    fn exact_regular_member_in_each_format() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut binary = vec![42; 128];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[16..18].copy_from_slice(&2u16.to_le_bytes());
        binary[18..20].copy_from_slice(&62u16.to_le_bytes());
        let tar = tar_bytes(&[
            ("README", tar::EntryType::Regular, b"ignore"),
            ("bin/tool", tar::EntryType::Regular, &binary),
        ])?;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar)?;
        let mut xz = xz2::write::XzEncoder::new(Vec::new(), 1);
        xz.write_all(&tar)?;
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file("bin/tool", zip::write::SimpleFileOptions::default())?;
        zip.write_all(&binary)?;
        for (kind, bytes) in [
            ("tar.gz", gz.finish()?),
            ("tar.xz", xz.finish()?),
            ("zip", zip.finish()?.into_inner()),
        ] {
            let artifact = dir.path().join(kind);
            fs::write(&artifact, bytes)?;
            assert_eq!(binaries(&artifact, kind, "x86_64")?, ["bin/tool"]);
            assert!(binaries(&artifact, kind, "aarch64")?.is_empty());
            let target = dir.path().join(format!("output-{kind}"));
            extract(&artifact, kind, "bin/tool", &target)?;
            assert_eq!(fs::read(target)?, binary);
        }
        assert!(!dir.path().join("bin").exists());
        assert!(!dir.path().join("README").exists());
        Ok(())
    }

    #[test]
    fn leading_dot_tar_member_is_canonical_but_duplicate_aliases_fail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut binary = vec![42; 128];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[16..18].copy_from_slice(&2u16.to_le_bytes());
        binary[18..20].copy_from_slice(&62u16.to_le_bytes());
        let write_archive = |name: &str, entries: &[(&str, tar::EntryType, &[u8])]| -> Result<_> {
            let tar = tar_bytes(entries)?;
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&tar)?;
            let path = dir.path().join(name);
            fs::write(&path, gz.finish()?)?;
            Ok(path)
        };
        let one = write_archive("one.tar.gz", &[("./tool", tar::EntryType::Regular, &binary)])?;
        assert_eq!(binaries(&one, "tar.gz", "x86_64")?, ["tool"]);
        let target = dir.path().join("tool");
        extract(&one, "tar.gz", "tool", &target)?;
        assert_eq!(fs::read(target)?, binary);
        let duplicate = write_archive("duplicate.tar.gz", &[("./tool", tar::EntryType::Regular, &binary), ("tool", tar::EntryType::Regular, &binary)])?;
        assert!(binaries(&duplicate, "tar.gz", "x86_64").is_err());
        assert!(extract(&duplicate, "tar.gz", "tool", &dir.path().join("duplicate-tool")).is_err());
        Ok(())
    }

    #[test]
    fn rejects_links_duplicates_missing_and_unsafe_members() -> Result<()> {
        for name in [
            "/tool",
            "../tool",
            "bin/../tool",
            "./tool",
            "bin//tool",
            "bin\\tool",
            "*",
        ] {
            assert!(validate_member(name).is_err());
        }
        let binary = vec![0; 64];
        for entries in [
            vec![("tool", tar::EntryType::Symlink, b"".as_slice())],
            vec![("tool", tar::EntryType::Link, b"".as_slice())],
            vec![("tool", tar::EntryType::Directory, b"".as_slice())],
            vec![
                ("tool", tar::EntryType::Regular, binary.as_slice()),
                ("tool", tar::EntryType::Regular, binary.as_slice()),
            ],
            vec![("different", tar::EntryType::Regular, binary.as_slice())],
        ] {
            let dir = tempfile::tempdir()?;
            assert!(
                extract_tar(
                    io::Cursor::new(tar_bytes(&entries)?),
                    "tool",
                    &dir.path().join("out")
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn zip_duplicate_names_and_symlinks_are_rejected() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for name in ["tool", "xxxx"] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())?;
            zip.write_all(&[1; 64])?;
        }
        let mut bytes = zip.finish()?.into_inner();
        for i in 0..bytes.len() - 3 {
            if &bytes[i..i + 4] == b"xxxx" {
                bytes[i..i + 4].copy_from_slice(b"tool");
            }
        }
        let artifact = dir.path().join("duplicate.zip");
        fs::write(&artifact, bytes)?;
        assert!(
            extract(&artifact, "zip", "tool", &dir.path().join("out"))
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.add_symlink("tool", "outside", zip::write::SimpleFileOptions::default())?;
        fs::write(&artifact, zip.finish()?.into_inner())?;
        assert!(
            extract(&artifact, "zip", "tool", &dir.path().join("out"))
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
        assert!(!dir.path().join("out").exists());
        Ok(())
    }
}
