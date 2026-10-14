//! Documents built byte by byte at test time: PDF, zip (stored), tar and gzip
//! (stored deflate blocks). Nothing here depends on an archive library, so the
//! bytes the adapters read are exactly the formats' specifications.
#![allow(dead_code)]

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PDF whose page `i` shows the lines of `pages[i]` in Helvetica.
pub fn pdf(pages: &[&[&str]]) -> Vec<u8> {
    let count = pages.len();
    let font = 3 + 2 * count;
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {count} >>",
            (0..count)
                .map(|i| format!("{} 0 R", 3 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        )
        .into_bytes(),
    ];
    for (i, lines) in pages.iter().enumerate() {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /Font << /F1 {font} 0 R >> >> >>",
                4 + 2 * i
            )
            .into_bytes(),
        );
        let mut stream = String::from("BT /F1 12 Tf 72 712 Td");
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                stream.push_str(" 0 -14 Td");
            }
            stream.push_str(&format!(
                " ({}) Tj",
                line.replace('(', "\\(").replace(')', "\\)")
            ));
        }
        stream.push_str(" ET");
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            )
            .into_bytes(),
        );
    }
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// A zip of stored members; a name ending in `/` is a directory.
pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    const DATE: u16 = (40 << 9) | (1 << 5) | 1;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        let name_len = name.len() as u16;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for field in [20u16, 0, 0, 0, DATE] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc, size, size] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        let external: u32 = if name.ends_with('/') { 0x10 } else { 0 };
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for field in [20u16, 20, 0, 0, 0, DATE] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        for field in [crc, size, size] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        for field in [name_len, 0, 0, 0, 0] {
            central.extend_from_slice(&field.to_le_bytes());
        }
        central.extend_from_slice(&external.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    let count = entries.len() as u16;
    for field in [0u16, 0, count, count] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A ustar archive of regular files.
pub fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// A gzip stream of stored deflate blocks.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}
