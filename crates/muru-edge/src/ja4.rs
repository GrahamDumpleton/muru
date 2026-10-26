//! TLS fingerprints (design specification section 4.14): the JA4 of a
//! client's ClientHello, read from the connection before the handshake
//! takes it, as a client class input and for the application.
//!
//! JA4 is FoxIO's: `t13d1516h2_8daaf6152771_b186095e22b6`. The first
//! part is the transport, the highest TLS version offered, whether a
//! name was asked for, the counts of cipher suites and extensions and
//! the first and last characters of the first ALPN protocol; the second
//! is a hash of the cipher suites, sorted; the third a hash of the
//! extensions, sorted, without the name and ALPN ones, followed by the
//! signature algorithms in the order offered. GREASE values are left
//! out throughout.

use std::time::Duration;

use tokio::net::TcpStream;

/// The largest TLS record, with its header.
const MOST: usize = 5 + 16384;

/// The fingerprint of the ClientHello a TLS connection opens with,
/// looked at without taking it from the socket, within `timeout`; none
/// when it does not arrive whole in one record in that time or cannot be
/// read.
pub async fn peek(stream: &TcpStream, timeout: Duration) -> Option<String> {
    let mut buffer = vec![0u8; MOST];

    tokio::time::timeout(timeout, async {
        loop {
            let read = stream.peek(&mut buffer).await.ok()?;

            if read == 0 || buffer[0] != 0x16 {
                return None;
            }

            if read >= 5 {
                let needed = 5 + usize::from(u16::from_be_bytes([buffer[3], buffer[4]]));

                if needed > MOST {
                    return None;
                }

                if read >= needed {
                    return fingerprint(&buffer[5..needed]);
                }
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .ok()
    .flatten()
}

/// What JA4 is computed from.
#[derive(Debug, Default, PartialEq, Eq)]
struct Hello {
    version: u16,
    ciphers: Vec<u16>,
    extensions: Vec<u16>,
    server_name: bool,
    alpn: Option<Vec<u8>>,
    versions: Vec<u16>,
    signatures: Vec<u16>,
}

/// The JA4 of a handshake record's payload holding a ClientHello.
pub fn fingerprint(record: &[u8]) -> Option<String> {
    parse(record).map(|hello| hello.ja4())
}

fn parse(record: &[u8]) -> Option<Hello> {
    let mut reader = Reader(record);

    if reader.u8()? != 1 {
        return None;
    }

    let length = reader.u24()?;
    let mut body = Reader(reader.take(length)?);
    let mut hello = Hello {
        version: body.u16()?,
        ..Hello::default()
    };
    body.take(32)?;
    let session = usize::from(body.u8()?);
    body.take(session)?;
    let mut ciphers = body.vector16()?;

    while !ciphers.0.is_empty() {
        hello.ciphers.push(ciphers.u16()?);
    }

    let compression = usize::from(body.u8()?);
    body.take(compression)?;

    if body.0.is_empty() {
        return Some(hello);
    }

    let mut extensions = body.vector16()?;

    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let mut data = extensions.vector16()?;
        hello.extensions.push(kind);

        match kind {
            0x0000 => hello.server_name = true,
            0x0010 => {
                let mut list = data.vector16()?;
                let first = usize::from(list.u8()?);
                hello.alpn = Some(list.take(first)?.to_vec());
            }
            0x000d => {
                let mut list = data.vector16()?;

                while !list.0.is_empty() {
                    hello.signatures.push(list.u16()?);
                }
            }
            0x002b => {
                let mut list = data.vector8()?;

                while !list.0.is_empty() {
                    hello.versions.push(list.u16()?);
                }
            }
            _ => {}
        }
    }

    Some(hello)
}

impl Hello {
    fn ja4(&self) -> String {
        let version = self
            .versions
            .iter()
            .copied()
            .filter(|value| !grease(*value))
            .max()
            .unwrap_or(self.version);

        let version = match version {
            0x0304 => "13",
            0x0303 => "12",
            0x0302 => "11",
            0x0301 => "10",
            0x0300 => "s3",
            0x0002 => "s2",
            0xfeff => "d1",
            0xfefd => "d2",
            0xfefc => "d3",
            _ => "00",
        };

        let ciphers: Vec<u16> = self
            .ciphers
            .iter()
            .copied()
            .filter(|v| !grease(*v))
            .collect();
        let extensions: Vec<u16> = self
            .extensions
            .iter()
            .copied()
            .filter(|v| !grease(*v))
            .collect();
        let alpn = self.alpn.as_deref().map_or_else(|| "00".to_owned(), alpn);
        let first = format!(
            "t{version}{}{:02}{:02}{alpn}",
            if self.server_name { 'd' } else { 'i' },
            ciphers.len().min(99),
            extensions.len().min(99),
        );

        let mut sorted = ciphers;
        sorted.sort_unstable();
        let second = hash(&hex_list(&sorted));

        let mut sorted: Vec<u16> = extensions
            .into_iter()
            .filter(|kind| *kind != 0x0000 && *kind != 0x0010)
            .collect();
        sorted.sort_unstable();
        let signatures: Vec<u16> = self
            .signatures
            .iter()
            .copied()
            .filter(|v| !grease(*v))
            .collect();
        let mut text = hex_list(&sorted);

        if !signatures.is_empty() {
            text.push('_');
            text.push_str(&hex_list(&signatures));
        }

        let third = if sorted.is_empty() {
            hash("")
        } else {
            hash(&text)
        };

        format!("{first}_{second}_{third}")
    }
}

/// GREASE values (RFC 8701), which clients send to keep servers honest
/// and which JA4 leaves out: 0x0a0a, 0x1a1a and so on to 0xfafa.
fn grease(value: u16) -> bool {
    value & 0x0f0f == 0x0a0a && value >> 8 == value & 0xff
}

/// The first and last characters of the first ALPN protocol, or of its
/// hex form when either is not alphanumeric.
fn alpn(protocol: &[u8]) -> String {
    let (Some(&first), Some(&last)) = (protocol.first(), protocol.last()) else {
        return "00".to_owned();
    };

    if first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric() {
        return format!("{}{}", char::from(first), char::from(last));
    }

    let hex = format!("{first:02x}{last:02x}");
    format!("{}{}", &hex[..1], &hex[3..])
}

fn hex_list(values: &[u16]) -> String {
    values
        .iter()
        .map(|value| format!("{value:04x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The first 12 hex digits of the SHA-256 of `text`, or twelve zeros for
/// nothing.
fn hash(text: &str) -> String {
    if text.is_empty() {
        return "0".repeat(12);
    }

    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    digest.as_ref()[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Big-endian fields from the front of a slice.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.0.len() < count {
            return None;
        }

        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Some(taken)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// A field with a one-byte length before it.
    fn vector8(&mut self) -> Option<Reader<'a>> {
        let length = usize::from(self.u8()?);
        self.take(length).map(Reader)
    }

    /// A field with a two-byte length before it.
    fn vector16(&mut self) -> Option<Reader<'a>> {
        let length = usize::from(self.u16()?);
        self.take(length).map(Reader)
    }

    fn u24(&mut self) -> Option<usize> {
        self.take(3).map(|bytes| {
            usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello handshake message with these fields.
    fn client_hello(ciphers: &[u16], extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[7u8; 32]);
        body.push(0);
        body.extend_from_slice(&u16::try_from(ciphers.len() * 2).unwrap().to_be_bytes());

        for cipher in ciphers {
            body.extend_from_slice(&cipher.to_be_bytes());
        }

        body.extend_from_slice(&[1, 0]);
        let mut block = Vec::new();

        for (kind, data) in extensions {
            block.extend_from_slice(&kind.to_be_bytes());
            block.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
            block.extend_from_slice(data);
        }

        body.extend_from_slice(&u16::try_from(block.len()).unwrap().to_be_bytes());
        body.extend_from_slice(&block);
        let mut message = vec![1];
        message.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }

    #[test]
    fn a_client_hello_has_the_fingerprint_ja4_describes() {
        let server_name = vec![0, 6, 0, 0, 3, b'a', b'.', b'b'];
        let alpn = vec![0, 6, 2, b'h', b'2', 2, b'h', b'1'];
        let signatures = vec![0, 6, 0x04, 0x03, 0x08, 0x04, 0x2a, 0x2a];
        let versions = vec![6, 0x3a, 0x3a, 0x03, 0x04, 0x03, 0x03];
        let hello = client_hello(
            &[0x1a1a, 0x1302, 0x1301, 0xc02f],
            &[
                (0x2a2a, Vec::new()),
                (0x0000, server_name),
                (0x0010, alpn),
                (0x000d, signatures),
                (0x002b, versions),
                (0x0017, Vec::new()),
            ],
        );

        // GREASE is left out of every count and list; the counts keep the
        // name and ALPN extensions, the third hash does not.

        let expected = format!(
            "t13d0305h2_{}_{}",
            hash("1301,1302,c02f"),
            hash("000d,0017,002b_0403,0804")
        );
        assert_eq!(fingerprint(&hello).unwrap(), expected);
    }

    #[test]
    fn a_bare_client_hello_has_zeros_for_what_it_lacks() {
        let hello = client_hello(&[0x002f], &[]);
        let expected = format!("t12i010000_{}_000000000000", hash("002f"));
        assert_eq!(fingerprint(&hello).unwrap(), expected);
        assert_eq!(fingerprint(&[2, 0, 0, 0]), None);
        assert!(grease(0xdada) && !grease(0x0a1a));
        assert_eq!(alpn(b"\x00ab\xff"), "0f");
    }
}
