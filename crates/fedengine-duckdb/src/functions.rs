//! Bounded scalar compatibility functions; registered once and inherited by clones.
use duckdb::{
    Connection,
    arrow::{
        array::{Array, BinaryBuilder, RecordBatch, StringArray, StringBuilder},
        datatypes::DataType,
    },
    vscalar::{ArrowFunctionSignature, VArrowScalar},
};
use std::{error::Error, sync::Arc};

pub(super) fn register(conn: &Connection) -> duckdb::Result<()> {
    conn.register_scalar_function::<Decode>("__fed_pg_decode")?;
    conn.register_scalar_function::<NameInput<true>>("__fed_pg_regclass_input")?;
    conn.register_scalar_function::<NameInput<false>>("__fed_pg_relation_parts")?;
    conn.register_scalar_function::<QuoteIdent>("__fed_pg_quote_ident")
}

struct NameInput<const NUMERIC: bool>;
impl<const NUMERIC: bool> VArrowScalar for NameInput<NUMERIC> {
    type State = ();
    fn invoke(_: &(), input: RecordBatch) -> Result<Arc<dyn Array>, Box<dyn Error>> {
        let a = input
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("__fed_pg_relation_name")?;
        let mut output = StringBuilder::new();
        for value in a.iter() {
            let value = value.and_then(|name| {
                if NUMERIC {
                    fedengine_pg_compat::regclass_input(name)
                } else {
                    fedengine_pg_compat::relation_name_parts(name)
                        .ok()
                        .map(|parts| serde_json::json!({"parts":parts}).to_string())
                }
            });
            output.append_option(value);
        }
        Ok(Arc::new(output.finish()))
    }
    fn signatures() -> Vec<ArrowFunctionSignature> {
        vec![ArrowFunctionSignature::exact(
            vec![DataType::Utf8],
            DataType::Utf8,
        )]
    }
}
struct QuoteIdent;
impl VArrowScalar for QuoteIdent {
    type State = ();
    fn invoke(_: &(), input: RecordBatch) -> Result<Arc<dyn Array>, Box<dyn Error>> {
        let a = input
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("__fed_pg_relation_name")?;
        let mut output = StringBuilder::new();
        for value in a.iter() {
            output.append_option(value.map(fedengine_pg_compat::display_identifier));
        }
        Ok(Arc::new(output.finish()))
    }
    fn signatures() -> Vec<ArrowFunctionSignature> {
        vec![ArrowFunctionSignature::exact(
            vec![DataType::Utf8],
            DataType::Utf8,
        )]
    }
}

struct Decode;
impl VArrowScalar for Decode {
    type State = ();
    fn invoke(_: &(), input: RecordBatch) -> Result<Arc<dyn Array>, Box<dyn Error>> {
        let a = input
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("__fed_pg_decode")?;
        let b = input
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("__fed_pg_decode")?;
        let mut output = BinaryBuilder::new();
        for (a, b) in a.iter().zip(b.iter()) {
            match (a, b) {
                (Some(a), Some(b)) => output.append_value(decode(a, b).ok_or(
                    if b.eq_ignore_ascii_case("escape") {
                        "__fed_pg_decode_escape"
                    } else {
                        "__fed_pg_decode"
                    },
                )?),
                _ => output.append_null(),
            }
        }
        Ok(Arc::new(output.finish()))
    }
    fn signatures() -> Vec<ArrowFunctionSignature> {
        vec![ArrowFunctionSignature::exact(
            vec![DataType::Utf8, DataType::Utf8],
            DataType::Binary,
        )]
    }
}

fn whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}
fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
fn decode(text: &str, format: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    match format.to_ascii_lowercase().as_str() {
        "hex" => {
            let mut out = Vec::with_capacity(bytes.len() / 2);
            let mut i = 0;
            while i < bytes.len() {
                if whitespace(bytes[i]) {
                    i += 1;
                    continue;
                }
                out.push(hex(bytes[i])? * 16 + hex(*bytes.get(i + 1)?)?);
                i += 2;
            }
            Some(out)
        }
        "base64" => {
            // Match PG's streaming decoder, including its persistent padding
            // state across groups; a generic strict RFC decoder differs here.
            let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
            let (mut buffer, mut position, mut end) = (0u32, 0, 0);
            for &byte in bytes {
                if whitespace(byte) {
                    continue;
                }
                let value = match byte {
                    b'A'..=b'Z' => byte - b'A',
                    b'a'..=b'z' => byte - b'a' + 26,
                    b'0'..=b'9' => byte - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    b'=' => {
                        if end == 0 {
                            end = match position {
                                2 => 1,
                                3 => 2,
                                _ => return None,
                            };
                        }
                        0
                    }
                    _ => return None,
                };
                buffer = (buffer << 6) + u32::from(value);
                position += 1;
                if position == 4 {
                    out.push((buffer >> 16) as u8);
                    if end == 0 || end > 1 {
                        out.push((buffer >> 8) as u8);
                    }
                    if end == 0 {
                        out.push(buffer as u8);
                    }
                    buffer = 0;
                    position = 0;
                }
            }
            (position == 0).then_some(out)
        }
        "escape" => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] != b'\\' {
                    out.push(bytes[i]);
                    i += 1;
                } else if bytes.get(i + 1) == Some(&b'\\') {
                    out.push(b'\\');
                    i += 2;
                } else {
                    let a = *bytes.get(i + 1)?;
                    let b = *bytes.get(i + 2)?;
                    let c = *bytes.get(i + 3)?;
                    if !(b'0'..=b'3').contains(&a)
                        || !(b'0'..=b'7').contains(&b)
                        || !(b'0'..=b'7').contains(&c)
                    {
                        return None;
                    }
                    out.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                    i += 4;
                }
            }
            Some(out)
        }
        _ => None,
    }
}
