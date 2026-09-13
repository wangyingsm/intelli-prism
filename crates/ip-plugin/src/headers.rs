use http::{HeaderMap, HeaderName, HeaderValue};

use crate::error::HeaderBlockError;

/// Writes headers as the block a header plugin reads: one `name: value` per line, ended
/// by CRLF, with a repeated name written once per value.
pub fn encode_headers(headers: &HeaderMap) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in headers {
        block.extend_from_slice(name.as_str().as_bytes());
        block.extend_from_slice(b": ");
        block.extend_from_slice(value.as_bytes());
        block.extend_from_slice(b"\r\n");
    }
    block
}

/// Reads a header block back. A bare LF also ends a line and blank lines are skipped;
/// anything else that does not parse as a header is refused with its line number.
pub fn decode_headers(block: &[u8]) -> Result<HeaderMap, HeaderBlockError> {
    let mut headers = HeaderMap::new();
    for (index, raw) in block.split(|byte| *byte == b'\n').enumerate() {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(HeaderBlockError {
                line: number,
                problem: "has no colon",
            })?;
        let name =
            HeaderName::from_bytes(line[..colon].trim_ascii()).map_err(|_| HeaderBlockError {
                line: number,
                problem: "names no valid header",
            })?;
        let value = HeaderValue::from_bytes(line[colon + 1..].trim_ascii()).map_err(|_| {
            HeaderBlockError {
                line: number,
                problem: "carries a value a header cannot hold",
            }
        })?;
        headers.append(name, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn headers_are_written_one_per_line() {
        let block = encode_headers(&headers(&[
            ("content-type", "application/json"),
            ("x-ip-trace", "abc"),
        ]));
        assert_eq!(
            block,
            b"content-type: application/json\r\nx-ip-trace: abc\r\n"
        );
    }

    #[test]
    fn a_repeated_name_is_written_once_per_value() {
        let block = encode_headers(&headers(&[
            ("accept", "text/plain"),
            ("accept", "text/html"),
        ]));
        assert_eq!(block, b"accept: text/plain\r\naccept: text/html\r\n");
    }

    #[test]
    fn headers_survive_a_round_trip() {
        let original = headers(&[
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
            ("accept", "application/json"),
            ("x-ip-trace", "abc"),
        ]);
        assert_eq!(decode_headers(&encode_headers(&original)), Ok(original));
    }

    #[test]
    fn no_headers_is_an_empty_block_both_ways() {
        assert!(encode_headers(&HeaderMap::new()).is_empty());
        assert_eq!(decode_headers(b""), Ok(HeaderMap::new()));
    }

    #[test]
    fn a_value_beyond_ascii_crosses_untouched() {
        let mut original = HeaderMap::new();
        original.insert(
            "x-opaque",
            HeaderValue::from_bytes(b"caf\xc3\xa9 \xff").unwrap(),
        );
        assert_eq!(decode_headers(&encode_headers(&original)), Ok(original));
    }

    #[test]
    fn a_bare_line_feed_also_ends_a_line() {
        assert_eq!(
            decode_headers(b"a: 1\nb: 2\n"),
            Ok(headers(&[("a", "1"), ("b", "2")]))
        );
    }

    #[test]
    fn blank_lines_and_surrounding_space_are_ignored() {
        assert_eq!(
            decode_headers(b"\r\n  x-a  :   spaced out   \r\n\r\nx-b:tight\r\n"),
            Ok(headers(&[("x-a", "spaced out"), ("x-b", "tight")]))
        );
    }

    #[test]
    fn a_name_in_capitals_is_folded_to_lower_case() {
        let decoded = decode_headers(b"X-Tenant-Policy: strict\r\n").unwrap();
        assert_eq!(decoded.get("x-tenant-policy").unwrap(), "strict");
    }

    #[test]
    fn a_line_without_a_colon_is_refused_with_its_number() {
        assert_eq!(
            decode_headers(b"a: 1\r\nnot a header\r\n"),
            Err(HeaderBlockError {
                line: 2,
                problem: "has no colon",
            })
        );
    }

    #[test]
    fn a_name_a_header_cannot_have_is_refused() {
        assert_eq!(
            decode_headers(b"bad name: 1\r\n"),
            Err(HeaderBlockError {
                line: 1,
                problem: "names no valid header",
            })
        );
    }

    #[test]
    fn a_value_carrying_a_control_character_is_refused() {
        assert_eq!(
            decode_headers(b"x-a: split\rhere\r\n"),
            Err(HeaderBlockError {
                line: 1,
                problem: "carries a value a header cannot hold",
            })
        );
    }

    #[test]
    fn the_error_names_the_line_for_a_plugin_author() {
        let error = decode_headers(b"oops\r\n").unwrap_err();
        assert_eq!(error.to_string(), "line 1 of the header block has no colon");
    }
}
