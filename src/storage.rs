use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

pub fn clean_text(text: &str) -> String {
    let repaired = if text.contains(['Ã', 'Â', 'â']) {
        let (bytes, _, failed) = encoding_rs::WINDOWS_1252.encode(text);
        if !failed {
            std::str::from_utf8(&bytes)
                .ok()
                .filter(|s| !s.chars().any(char::is_control))
                .map(str::to_owned)
        } else {
            None
        }
    } else {
        None
    };
    repaired
        .as_deref()
        .unwrap_or(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn component(name: &str, limit: usize) -> String {
    let name = clean_text(name).replace(|c: char| c.is_control() || "<>:\"/\\|?*".contains(c), "_");
    let name = name.trim_matches([' ', '.']);
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.chars().count() == 1 && "123456789¹²³".contains(n))
    });
    let mut output = if reserved {
        String::from("_")
    } else {
        String::new()
    };
    let mut units = output.len();
    for c in name.chars() {
        units += c.len_utf16();
        if units > limit {
            break;
        }
        output.push(c);
    }
    output.truncate(output.trim_end_matches([' ', '.']).len());
    if output.is_empty() {
        "archivo".into()
    } else {
        output
    }
}

fn file_parts(name: &str) -> (String, String) {
    match name.trim_matches([' ', '.']).rsplit_once('.') {
        Some((stem, ext))
            if !ext.is_empty()
                && ext.len() <= 14
                && ext.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            (component(stem, 76), format!(".{ext}"))
        }
        _ => (component(name, 76), String::new()),
    }
}

pub fn short_hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))[..12].to_owned()
}

pub struct Staged {
    path: PathBuf,
    file: File,
    hash: Sha256,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct Saved {
    pub name: String,
    pub bytes: u64,
    pub duplicate: bool,
}

impl Staged {
    pub fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context("No se pudo crear la carpeta de destino")?;
        let path = dir.join(format!(".moodle-{}.part", Uuid::new_v4()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .context("No se pudo crear el archivo temporal")?;
        Ok(Self {
            path,
            file,
            hash: Sha256::new(),
            bytes: 0,
        })
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.file
            .write_all(chunk)
            .context("No se pudo escribir en el destino")?;
        self.hash.update(chunk);
        self.bytes += chunk.len() as u64;
        Ok(())
    }

    pub fn finish(self, name: &str) -> Result<Saved> {
        self.file
            .sync_all()
            .context("No se pudo completar la escritura")?;
        let digest = self.hash.clone().finalize();
        let hex = format!("{digest:x}");
        let (stem, ext) = file_parts(name);
        let dir = self.path.parent().expect("El temporal tiene carpeta");
        for ordinal in 0..=999 {
            let name = match ordinal {
                0 => format!("{stem}{ext}"),
                1 => format!("{stem} ~{}{ext}", &hex[..12]),
                _ => format!("{stem} ~{}-{ordinal}{ext}", &hex[..12]),
            };
            let path = dir.join(&name);
            match move_without_replace(&self.path, &path) {
                Ok(()) => {
                    return Ok(Saved {
                        name,
                        bytes: self.bytes,
                        duplicate: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::symlink_metadata(&path).is_ok_and(|meta| {
                        meta.is_file() && !meta.is_symlink() && meta.len() == self.bytes
                    }) && hash_file(&path).is_ok_and(|existing| existing == digest)
                    {
                        return Ok(Saved {
                            name,
                            bytes: self.bytes,
                            duplicate: true,
                        });
                    }
                }
                Err(error) => return Err(error).context("No se pudo guardar el archivo final"),
            }
        }
        anyhow::bail!("Hay demasiados archivos con el mismo nombre");
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn hash_file(path: &Path) -> io::Result<sha2::digest::Output<Sha256>> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            return Ok(hash.finalize());
        }
        hash.update(&buffer[..size]);
    }
}

fn move_without_replace(source: &Path, target: &Path) -> io::Result<()> {
    let directory = fs::canonicalize(source.parent().expect("El temporal tiene carpeta"))?;
    let source = wide(directory.join(source.file_name().unwrap()).as_os_str());
    let target = wide(directory.join(target.file_name().unwrap()).as_os_str());
    // Both paths are NUL terminated and point into the same directory. Zero flags
    // preserve an existing destination even if another instance writes first.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

pub fn save(dir: &Path, name: &str, data: &[u8]) -> Result<Saved> {
    let mut staged = Staged::new(dir)?;
    staged.write(data)?;
    staged.finish(name)
}
