//! Parsing of the payload samples the probes emit.
//!
//! The kernel decides only that a write is worth shipping — a request line, a
//! database command, a query — and copies its first [`SAMPLE`] bytes into a
//! perf ring buffer. Everything that turns those bytes into a host name or a
//! statement happens here, which is why adding a protocol costs a function
//! rather than a slice of the verifier's instruction budget.
//!
//! Three formats are understood today: an HTTP/1.x `Host:` header, a MySQL
//! `COM_QUERY` / `COM_STMT_PREPARE` packet, and a PostgreSQL simple query or
//! `Parse` message. HTTP/2 keeps its headers in a compressed HPACK block that
//! cannot be read without connection state, so it contributes nothing here.

use lidlesseye_core::servicemap::protocol;

/// Bytes of a write the probe copies. Mirrors `L7_SAMPLE` in `net.bpf.c`.
pub const SAMPLE: usize = 256;

/// Fixed part of `struct l7_event`: `flow_key` (40) + cgroup (8) + len (4) +
/// flags (4).
pub const HEADER: usize = 56;

/// `flags & HAS_FLOW`: the sample carries the connection it belongs to.
pub const HAS_FLOW: u32 = 1;

/// Longest host name and statement kept for display. Anything longer is cut
/// and marked, because the panel shows one line of context.
const MAX_HOST: usize = 128;
const MAX_SQL: usize = 200;

/// The parsed value of one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    /// Application protocol the payload was recognised as.
    pub protocol: u32,
    /// `Host:` header of an HTTP request.
    pub host: Option<String>,
    /// Statement carried by a database command.
    pub sql: Option<String>,
}

/// One decoded ring-buffer record.
#[derive(Debug, Clone)]
pub struct Sample<'a> {
    /// The connection the sample belongs to, as the kernel's raw key. `None`
    /// for a TLS sample, which has no socket to key on.
    pub flow: Option<[u8; 40]>,
    /// Cgroup of the writing task; zero when the probe could not read one.
    pub cgroup: u64,
    pub payload: &'a [u8],
}

/// Decode one record from the ring buffer.
///
/// The kernel pads a sample up to its alignment, so `raw` may be longer than
/// [`HEADER`] + [`SAMPLE`]; only the declared length is used.
pub fn decode(raw: &[u8]) -> Option<Sample<'_>> {
    if raw.len() < HEADER {
        return None;
    }
    let len = u32::from_ne_bytes(raw[48..52].try_into().ok()?) as usize;
    if len == 0 || len > SAMPLE || HEADER + len > raw.len() {
        return None;
    }
    let flags = u32::from_ne_bytes(raw[52..56].try_into().ok()?);
    let flow = if flags & HAS_FLOW != 0 {
        Some(raw[..40].try_into().ok()?)
    } else {
        None
    };
    Some(Sample {
        flow,
        cgroup: u64::from_ne_bytes(raw[40..48].try_into().ok()?),
        payload: &raw[HEADER..HEADER + len],
    })
}

/// Extract what the payload says, if anything.
pub fn parse(payload: &[u8]) -> Option<Detail> {
    if payload.len() < 8 {
        return None;
    }
    if is_http_request(payload) {
        return Some(Detail {
            protocol: protocol::HTTP,
            host: http_host(payload),
            sql: None,
        });
    }
    // A response carries no Host, but recognising it is what names the
    // protocol of a server-side flow: the kernel no longer classifies, and an
    // accepted socket only ever sends responses.
    if payload.starts_with(b"HTTP/1.") {
        return Some(Detail {
            protocol: protocol::HTTP,
            host: None,
            sql: None,
        });
    }
    if let Some(sql) = mysql_sql(payload) {
        return Some(Detail {
            protocol: protocol::MYSQL,
            host: None,
            sql: Some(sql),
        });
    }
    if let Some(sql) = pg_sql(payload) {
        return Some(Detail {
            protocol: protocol::POSTGRES,
            host: None,
            sql: Some(sql),
        });
    }
    None
}

/// A request line is the only place the client's intent can be read; matching
/// the method keeps a response or a chunked body from being mistaken for one.
fn is_http_request(payload: &[u8]) -> bool {
    const METHODS: [&[u8]; 7] = [
        b"GET ",
        b"POST ",
        b"PUT ",
        b"HEAD ",
        b"DELETE ",
        b"OPTIONS ",
        b"PATCH ",
    ];
    METHODS.iter().any(|method| payload.starts_with(method))
}

fn http_host(payload: &[u8]) -> Option<String> {
    // Only the header block: a body may contain the word "Host:" as data.
    let head_end = payload
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(payload.len());
    for line in payload[..head_end].split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            continue;
        };
        if line[..colon].eq_ignore_ascii_case(b"host") {
            let value = trim_ascii(&line[colon + 1..]);
            if value.is_empty() {
                return None;
            }
            return Some(clip(value, MAX_HOST));
        }
    }
    None
}

/// MySQL packets are `length(3) + sequence(1) + command + body`. A write can
/// hold several of them, so walk the chain until one carries a statement.
fn mysql_sql(payload: &[u8]) -> Option<String> {
    let mut rest = payload;
    for _ in 0..4 {
        if rest.len() < 5 || rest[3] != 0 {
            return None;
        }
        let length = usize::from(rest[0]) | usize::from(rest[1]) << 8 | usize::from(rest[2]) << 16;
        if length == 0 {
            return None;
        }
        let packet_end = 4 + length;
        let truncated = packet_end > rest.len();
        let end = packet_end.min(rest.len());
        // COM_QUERY carries the text directly; COM_STMT_PREPARE carries the
        // statement a driver wants a handle for. Both are what a client sends.
        if (rest[4] == 0x03 || rest[4] == 0x16) && end > 5 {
            let sql = sql_text(&rest[5..end], truncated);
            if !sql.is_empty() {
                return Some(sql);
            }
        }
        if truncated || packet_end >= rest.len() {
            return None;
        }
        rest = &rest[packet_end..];
    }
    None
}

/// PostgreSQL messages are `type(1) + length(4, big endian, self-inclusive)`.
/// `Q` is a simple query and carries the statement from byte 5. `P` is a
/// Parse whose body is `statement-name\0 query\0`; the prepared statement text
/// is the part that names tables and columns.
fn pg_sql(payload: &[u8]) -> Option<String> {
    let mut rest = payload;
    for _ in 0..4 {
        if rest.len() < 5 {
            return None;
        }
        let length = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
        if length < 4 {
            return None;
        }
        let declared_end = 1 + length;
        let end = declared_end.min(rest.len());
        let body = &rest[5..end];
        // Only a shortfall of more than the terminator means the statement
        // itself was cut: TCP is free to split a message exactly before the
        // NUL, and a Parse keeps more fields after the query anyway.
        let cut = declared_end > rest.len() + 1;
        let sql = match rest[0] {
            b'Q' => (!body.is_empty()).then(|| sql_text(body, cut)),
            b'P' => body
                .iter()
                .position(|&byte| byte == 0)
                .map(|zero| sql_text(&body[zero + 1..], cut)),
            _ => None,
        };
        if let Some(sql) = sql
            && !sql.is_empty()
        {
            return Some(sql);
        }
        if end >= rest.len() {
            return None;
        }
        rest = &rest[end..];
    }
    None
}

fn sql_text(bytes: &[u8], truncated: bool) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let mut text = String::from_utf8_lossy(trim_ascii(&bytes[..end])).into_owned();
    if text.is_empty() {
        // A statement that is empty once trimmed is not a statement. Marking
        // it truncated would print a lone ellipsis on the edge.
        return text;
    }
    let clipped = text.chars().count() > MAX_SQL;
    if clipped {
        text = text.chars().take(MAX_SQL).collect();
    }
    if truncated || clipped {
        text.push('…');
    }
    text
}

fn clip(value: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(value).into_owned();
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max).collect();
    cut.push('…');
    cut
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end.max(start)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(payload: &[u8]) -> Detail {
        parse(payload).expect("payload should be recognised")
    }

    #[test]
    fn host_header_is_found_and_trimmed() {
        let request = b"GET /api/orders?page=2 HTTP/1.1\r\nHost:  shop.example.com:8443 \r\nAccept: */*\r\n\r\n";
        assert_eq!(
            detail(request).host.as_deref(),
            Some("shop.example.com:8443")
        );
    }

    #[test]
    fn the_header_name_is_case_insensitive() {
        let request = b"POST /cart HTTP/1.1\r\nhOsT: cart.internal\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(detail(request).host.as_deref(), Some("cart.internal"));
    }

    #[test]
    fn a_response_names_the_protocol_but_never_a_host() {
        let response = b"HTTP/1.1 200 OK\r\nHost: ignored\r\n\r\n";
        let detail = detail(response);
        assert_eq!(detail.protocol, protocol::HTTP);
        assert_eq!(detail.host, None, "a response cannot name a requested host");
    }

    #[test]
    fn a_body_cannot_inject_a_host() {
        let request = b"PUT /upload HTTP/1.1\r\nContent-Type: text/plain\r\n\r\nHost: evil.example";
        assert_eq!(detail(request).host, None);
    }

    #[test]
    fn mysql_com_query_carries_the_statement() {
        let sql = "SELECT id, total FROM orders WHERE id = 42";
        let mut packet = vec![0u8; 5];
        let length = (sql.len() + 1) as u32;
        packet[0] = (length & 0xff) as u8;
        packet[1] = ((length >> 8) & 0xff) as u8;
        packet[2] = ((length >> 16) & 0xff) as u8;
        packet[3] = 0;
        packet[4] = 0x03;
        packet.extend_from_slice(sql.as_bytes());
        assert_eq!(detail(&packet).sql.as_deref(), Some(sql));
    }

    #[test]
    fn mysql_prepared_statements_are_read_too() {
        let sql = "INSERT INTO carts (id) VALUES (?)";
        let mut packet = vec![0u8; 5];
        let length = (sql.len() + 1) as u32;
        packet[0] = (length & 0xff) as u8;
        packet[1] = ((length >> 8) & 0xff) as u8;
        packet[2] = ((length >> 16) & 0xff) as u8;
        packet[4] = 0x16;
        packet.extend_from_slice(sql.as_bytes());
        assert_eq!(detail(&packet).sql.as_deref(), Some(sql));
    }

    #[test]
    fn postgres_simple_query_carries_the_statement() {
        let sql = "select * from payments where id = $1";
        let mut message = vec![b'Q'];
        message.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
        message.extend_from_slice(sql.as_bytes());
        message.push(0);
        assert_eq!(detail(&message).sql.as_deref(), Some(sql));
    }

    #[test]
    fn a_message_split_before_its_terminator_is_not_marked_truncated() {
        let sql = "select * from orders";
        let length = 4 + sql.len() + 1;
        let mut message = vec![b'Q'];
        message.extend_from_slice(&(length as u32).to_be_bytes());
        message.extend_from_slice(sql.as_bytes());
        // The NUL terminator arrives in the next write; TCP is allowed to split
        // there, and the statement this sample carries is complete.
        assert_eq!(detail(&message).sql.as_deref(), Some(sql));
    }

    #[test]
    fn postgres_parse_skips_the_statement_name() {
        let sql = "select count(*) from refunds";
        let mut body = Vec::new();
        body.extend_from_slice(b"s1\0");
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        let mut message = vec![b'P'];
        message.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        message.extend_from_slice(&body);
        assert_eq!(detail(&message).sql.as_deref(), Some(sql));
    }

    #[test]
    fn a_statement_cut_by_the_sample_is_marked() {
        let sql = "SELECT a, b, c FROM very_wide_table WHERE ".repeat(20);
        let mut packet = vec![0u8; 5];
        let length = (sql.len() + 1) as u32;
        packet[0] = (length & 0xff) as u8;
        packet[1] = ((length >> 8) & 0xff) as u8;
        packet[2] = ((length >> 16) & 0xff) as u8;
        packet[4] = 0x03;
        packet.extend_from_slice(&sql.as_bytes()[..SAMPLE - 8]);
        let text = detail(&packet)
            .sql
            .expect("a truncated statement is still one");
        assert!(text.ends_with('…'));
        assert!(text.chars().count() <= MAX_SQL + 1);
    }

    #[test]
    fn binary_noise_is_ignored() {
        assert!(parse(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc]).is_none());
    }

    #[test]
    fn a_record_decodes_its_header() {
        let mut raw = vec![0u8; HEADER + SAMPLE];
        raw[48..52].copy_from_slice(&(9u32).to_ne_bytes());
        raw[52..56].copy_from_slice(&HAS_FLOW.to_ne_bytes());
        raw[40..48].copy_from_slice(&7u64.to_ne_bytes());
        raw[HEADER..HEADER + 9].copy_from_slice(b"GET / HTT");
        let sample = decode(&raw).expect("a well-formed record");
        assert_eq!(sample.cgroup, 7);
        assert_eq!(sample.payload, b"GET / HTT");
        assert!(sample.flow.is_some());
    }

    /// The C struct and this decoder have to agree on one layout; the object
    /// is compiled separately, so a source contract is the only place the two
    /// can be compared.
    #[test]
    fn the_probe_and_the_parser_agree_on_the_sample_layout() {
        let source = include_str!("../bpf/net.bpf.c");
        assert!(source.contains(&format!("#define L7_SAMPLE {SAMPLE}")));
        assert!(source.contains(&format!("#define L7_HAS_FLOW {HAS_FLOW}U")));
        assert!(source.contains("bpf_perf_event_output"));
    }
}
