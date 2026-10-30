//! BF16 copies of models. On CPUs with AVX512-BF16 (Zen 4, Sapphire Rapids)
//! whisper.cpp multiplies BF16 matrices faster than Q8_0 ones: the encoder,
//! almost all of the transcription time, runs 1.25-1.4x faster on Zen 4.
//! The copy is made once, from the Q8_0 or F16 file, into $XDG_CACHE_HOME/voicetotextn.

use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

// ggml tensor types and whisper.cpp file types.
const F32: i32 = 0;
const F16: i32 = 1;
const Q8_0: i32 = 8;
const BF16: i32 = 30;
const FTYPE_F16: i32 = 1;
const FTYPE_Q8_0: i32 = 7;
const FTYPE_BF16: i32 = 24;
/// The header stores the quantization version as ftype / 1000.
const QNT_VERSION_FACTOR: i32 = 1000;
/// Magic, then 11 hyperparameters; the file type is the last one.
const HEADER_LEN: usize = 48;

/// True when whisper.cpp was built with the AVX512-BF16 kernels.
pub fn supported() -> bool {
    whisper_rs::print_system_info().contains("AVX512_BF16 = 1")
}

/// Returns the BF16 copy of `model`, converting it first if the copy is
/// missing or older than the model. `None`: the model has no Q8_0 or F16
/// weights to convert (already BF16, or another quantization).
pub fn cached(model: &Path) -> Result<Option<PathBuf>, String> {
    let name = model.file_stem().ok_or("model path has no file name")?.to_string_lossy();
    let copy = cache_dir().join(format!("{name}-bf16.bin"));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if modified(&copy).is_some_and(|c| modified(model).is_some_and(|m| c >= m)) {
        return Ok(Some(copy));
    }

    let read_err = |e: std::io::Error| format!("cannot read {}: {e}", model.display());
    let mut header = [0u8; HEADER_LEN];
    std::fs::File::open(model).and_then(|mut f| f.read_exact(&mut header)).map_err(read_err)?;
    if header[..4] != 0x67676d6c_u32.to_le_bytes() {
        return Err(format!("{} is not a whisper.cpp model", model.display()));
    }
    let ftype = i32::from_le_bytes(header[44..].try_into().unwrap()) % QNT_VERSION_FACTOR;
    let weights = match ftype {
        FTYPE_Q8_0 => Q8_0,
        FTYPE_F16 => F16,
        _ => return Ok(None),
    };

    let t = std::time::Instant::now();
    let src = std::fs::read(model).map_err(read_err)?;
    std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
    // Renamed into place when complete, so a crash leaves no half-written model.
    let tmp = copy.with_extension(format!("{}.tmp", std::process::id()));
    let write_err = |e: std::io::Error| format!("cannot write {}: {e}", tmp.display());
    let written = std::fs::File::create(&tmp).map_err(write_err).and_then(|file| {
        let mut out = BufWriter::new(file);
        convert(&src, weights, &mut out)?;
        out.into_inner().map_err(|e| write_err(e.into_error()))?.sync_all().map_err(write_err)?;
        std::fs::rename(&tmp, &copy).map_err(write_err)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    log!("{} converted to BF16 in {} ms", model.display(), t.elapsed().as_millis());
    Ok(Some(copy))
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("voicetotextn")
}

/// Copies a whisper.cpp model, turning the 2-D tensors of the `weights` type
/// (the ones whisper.cpp creates with the file's type) into BF16. Convolutions,
/// positional embeddings, biases and norms stay as they are.
fn convert(src: &[u8], weights: i32, out: &mut impl Write) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut r = Reader { src, pos: HEADER_LEN };
    out.write_all(&src[..HEADER_LEN - 4]).map_err(io)?;
    out.write_all(&FTYPE_BF16.to_le_bytes()).map_err(io)?;
    // Mel filters and vocabulary, copied as they are.
    let (n_mel, n_fft) = (r.i32()? as usize, r.i32()? as usize);
    r.bytes(4 * n_mel * n_fft)?;
    for _ in 0..r.i32()? {
        let len = r.i32()? as usize;
        r.bytes(len)?;
    }
    out.write_all(&src[HEADER_LEN..r.pos]).map_err(io)?;

    let f16_table: Vec<f32> = (0..=u16::MAX).map(f16_to_f32).collect();
    let f16 = |b: &[u8]| f16_table[u16::from_le_bytes([b[0], b[1]]) as usize];
    let mut buf = Vec::new();
    while r.pos < src.len() {
        // Tensor header: dimensions, name length, type, shape, name.
        let start = r.pos;
        let (n_dims, name_len, ttype) = (r.i32()?, r.i32()? as usize, r.i32()?);
        let mut n = 1;
        for _ in 0..n_dims {
            n *= r.i32()? as usize;
        }
        r.bytes(name_len)?;
        let header = &src[start..r.pos];
        let size = match ttype {
            F32 => 4 * n,
            F16 | BF16 => 2 * n,
            Q8_0 => n / 32 * 34,
            _ => return Err(format!("unsupported tensor type {ttype}")),
        };
        let data = r.bytes(size)?;
        if ttype != weights || n_dims != 2 {
            out.write_all(header).map_err(io)?;
            out.write_all(data).map_err(io)?;
            continue;
        }
        buf.clear();
        if ttype == Q8_0 {
            // A block is an F16 scale and 32 signed bytes.
            for block in data.chunks_exact(34) {
                let d = f16(block);
                buf.extend(block[2..].iter().flat_map(|&q| bf16(q as i8 as f32 * d).to_le_bytes()));
            }
        } else {
            buf.extend(data.chunks_exact(2).flat_map(|h| bf16(f16(h)).to_le_bytes()));
        }
        out.write_all(&header[..8]).map_err(io)?;
        out.write_all(&BF16.to_le_bytes()).map_err(io)?;
        out.write_all(&header[12..]).map_err(io)?;
        out.write_all(&buf).map_err(io)?;
    }
    Ok(())
}

struct Reader<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let b = self.src.get(self.pos..self.pos + n).ok_or("model file is truncated")?;
        self.pos += n;
        Ok(b)
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

/// Rounds to the nearest BF16 (the upper half of an f32), ties to even.
fn bf16(x: f32) -> u16 {
    let bits = x.to_bits();
    ((bits + 0x7fff + (bits >> 16 & 1)) >> 16) as u16
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10 & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    sign * match exp {
        0 => man * 2f32.powi(-24),
        31 if man == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        _ => (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}
