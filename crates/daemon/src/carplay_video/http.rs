use std::io::{self, Read, Write};

const MAXIMUM_HEAD_BYTES: usize = 16 * 1024;
const HEAD_END: &[u8] = b"\r\n\r\n";

pub(crate) fn read_head<R: Read + ?Sized>(reader: &mut R) -> io::Result<(String, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 2048];
    loop {
        if let Some(end) = find(&buffer, HEAD_END) {
            let early = buffer.split_off(end + HEAD_END.len());
            buffer.truncate(end);
            let head = String::from_utf8(buffer).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "HTTP head is not UTF-8")
            })?;
            return Ok((head, early));
        }
        if buffer.len() > MAXIMUM_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP head is too large",
            ));
        }
        let length = reader.read(&mut chunk)?;
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside an HTTP head",
            ));
        }
        buffer.extend_from_slice(&chunk[..length]);
    }
}

pub(crate) fn status(head: &str) -> Option<u16> {
    head.lines().next()?.split(' ').nth(1)?.parse().ok()
}

pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

pub(crate) fn read_body<R: Read + ?Sized>(
    head: &str,
    early: Vec<u8>,
    reader: &mut R,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut source = Prefixed::new(early, reader);
    let chunked = header(head, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    if chunked {
        return read_chunked(&mut source, limit);
    }
    if let Some(length) = header(head, "content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length"))?;
        if length > limit {
            return Err(too_large());
        }
        let mut body = vec![0_u8; length];
        source.read_exact(&mut body)?;
        return Ok(body);
    }
    let mut body = Vec::new();
    source.take(limit as u64 + 1).read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(too_large());
    }
    Ok(body)
}

fn read_chunked<R: Read>(source: &mut R, limit: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(source)?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            while !read_line(source)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > limit {
            return Err(too_large());
        }
        let start = body.len();
        body.resize(start + size, 0);
        source.read_exact(&mut body[start..])?;
        read_line(source)?;
    }
}

fn read_line<R: Read>(source: &mut R) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    while !line.ends_with(b"\r\n") {
        if line.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk line too long",
            ));
        }
        source.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk line"))
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "HTTP body exceeds its limit")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug)]
pub(crate) struct Prefixed<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<S: Read> Read for Prefixed<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset < self.prefix.len() {
            let available = &self.prefix[self.offset..];
            let length = available.len().min(buffer.len());
            buffer[..length].copy_from_slice(&available[..length]);
            self.offset += length;
            return Ok(length);
        }
        self.inner.read(buffer)
    }
}

impl<S: Write> Write for Prefixed<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn head_splits_from_early_body_bytes() {
        let mut source = Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".to_vec());
        let (head, early) = read_head(&mut source).expect("head");
        assert_eq!(status(&head), Some(200));
        assert_eq!(header(&head, "content-length"), Some("3"));
        assert_eq!(
            read_body(&head, early, &mut source, 16).expect("body"),
            b"abc"
        );
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked";
        let mut source = Cursor::new(b"4\r\n#EXT\r\n3\r\nM3U\r\n0\r\n\r\n".to_vec());
        assert_eq!(
            read_body(head, Vec::new(), &mut source, 64).expect("body"),
            b"#EXTM3U"
        );
        let mut source = Cursor::new(b"ff\r\n".to_vec());
        assert!(read_body(head, Vec::new(), &mut source, 16).is_err());
    }

    #[test]
    fn close_delimited_bodies_respect_the_limit() {
        let head = "HTTP/1.1 200 OK";
        let mut source = Cursor::new(vec![b'x'; 10]);
        assert_eq!(
            read_body(head, Vec::new(), &mut source, 10)
                .expect("body")
                .len(),
            10
        );
        let mut source = Cursor::new(vec![b'x'; 11]);
        assert!(read_body(head, Vec::new(), &mut source, 10).is_err());
    }
}
