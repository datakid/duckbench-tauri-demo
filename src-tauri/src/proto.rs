use std::io::{Read, Write};

pub fn write_frame<W: Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}
