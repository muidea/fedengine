//! PostgreSQL scalar codecs and scrubbed SQLSTATE responses.
mod catalog;
pub use catalog::*;
mod catalog_codec;
mod session;
pub use session::*;
mod planner;
pub use planner::*;
mod execution;
mod result_types;
use bytes::{BufMut, BytesMut};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
pub use execution::{EXECUTION_SETTINGS, execution_sql};
use fedengine_contract::*;
use fedengine_core::*;
use pgwire::{
    api::{
        Type,
        portal::Format,
        results::{FieldFormat, FieldInfo},
    },
    error::{ErrorInfo, PgWireError},
    messages::data::DataRow,
};
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
const PG_EPOCH_DAYS: i32 = 10957;
pub fn pg_error(e: EngineError) -> PgWireError {
    let message = if e.outcome_unknown {
        format!(
            "{}; write outcome is unknown, do not retry automatically",
            e.message
        )
    } else {
        e.message
    };
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".into(),
        e.sqlstate.into(),
        message,
    )))
}
pub fn fields(columns: &[ColumnMeta], format: &Format) -> Vec<FieldInfo> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            FieldInfo::new(
                c.name.clone(),
                c.table_oid.map(|id| id as i32),
                c.column_attnum,
                Type::from_oid(c.data_type.oid()).unwrap(),
                format.format_for(i),
            )
            .with_type_modifier(c.type_modifier)
        })
        .collect()
}
pub fn encode_row(row: &[Cell], columns: &[ColumnMeta], format: &Format) -> Result<DataRow> {
    if row.len() != columns.len() || row.len() > i16::MAX as usize {
        return Err(EngineError::new("08P01", "row/column count mismatch"));
    }
    let mut data = BytesMut::new();
    for (i, (cell, col)) in row.iter().zip(columns).enumerate() {
        if matches!(cell, Cell::Null) {
            data.put_i32(-1);
            continue;
        }
        let value = if format.format_for(i) == FieldFormat::Binary {
            binary(cell, col.data_type)?
        } else {
            match catalog_codec::text(cell, col.data_type)? {
                Some(v) => v.into_bytes(),
                None => {
                    let mut value = text(cell)?;
                    if col.data_type == PgType::Timestamptz {
                        value.push_str("+00");
                    }
                    value.into_bytes()
                }
            }
        };
        if value.len() > i32::MAX as usize {
            return Err(EngineError::new(
                "54000",
                "field exceeds protocol size limit",
            ));
        }
        data.put_i32(value.len() as i32);
        data.extend_from_slice(&value);
    }
    Ok(DataRow::new(data, row.len() as i16))
}
fn invalid() -> EngineError {
    EngineError::new("22P02", "invalid parameter representation")
}
fn binary(c: &Cell, t: PgType) -> Result<Vec<u8>> {
    if let PgType::Catalog(oid) = t {
        return catalog_codec::binary(c, oid);
    }
    Ok(match (t, c) {
        (PgType::Bool, Cell::Bool(v)) => vec![u8::from(*v)],
        (PgType::Int2, Cell::I16(v)) => v.to_be_bytes().to_vec(),
        (PgType::Int4, Cell::I32(v)) => v.to_be_bytes().to_vec(),
        (PgType::Int8, Cell::I64(v)) => v.to_be_bytes().to_vec(),
        (PgType::Float4, Cell::F32(v)) => v.to_be_bytes().to_vec(),
        (PgType::Float8, Cell::F64(v)) => v.to_be_bytes().to_vec(),
        (PgType::Text | PgType::Json, Cell::Text(v)) => v.as_bytes().to_vec(),
        (PgType::Bytea, Cell::Bytes(v)) => v.clone(),
        (PgType::Numeric, Cell::Numeric(v)) => encode_numeric(v)?,
        (PgType::Date, Cell::Date(v)) => v
            .checked_sub(PG_EPOCH_DAYS)
            .ok_or_else(invalid)?
            .to_be_bytes()
            .to_vec(),
        (PgType::Time, Cell::Time(v)) => v.to_be_bytes().to_vec(),
        (PgType::Timestamp | PgType::Timestamptz, Cell::Timestamp(v)) => v
            .checked_sub(PG_EPOCH_MICROS)
            .ok_or_else(invalid)?
            .to_be_bytes()
            .to_vec(),
        (PgType::Uuid, Cell::Uuid(v)) => v.to_vec(),
        (PgType::Uuid, Cell::Text(v)) => uuid::Uuid::parse_str(v)
            .map_err(|_| invalid())?
            .as_bytes()
            .to_vec(),
        // Numeric result schemas may promote an integer, without truncating it.
        (PgType::Numeric, Cell::I64(v)) => encode_numeric(&v.to_string())?,
        _ => return Err(EngineError::unsupported("result type/value codec mismatch")),
    })
}
fn text(c: &Cell) -> Result<String> {
    Ok(match c {
        Cell::Oid(v) => v.to_string(),
        Cell::NamedOid { name, .. } => name.clone(),
        Cell::Array { .. } => {
            return Err(EngineError::unsupported(
                "array requires its declared catalog type",
            ));
        }
        Cell::Null => String::new(),
        Cell::Bool(v) => if *v { "t" } else { "f" }.into(),
        Cell::I16(v) => v.to_string(),
        Cell::I32(v) => v.to_string(),
        Cell::I64(v) => v.to_string(),
        Cell::F32(v) => float_text(*v as f64),
        Cell::F64(v) => float_text(*v),
        Cell::Text(v) | Cell::Numeric(v) => v.clone(),
        Cell::Bytes(v) => format!(
            "\\x{}",
            v.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        Cell::Date(d) => NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .checked_add_signed(chrono::Duration::days(*d as i64))
            .ok_or_else(invalid)?
            .to_string(),
        Cell::Time(t) => {
            if !(0..86_400_000_000).contains(t) {
                return Err(invalid());
            }
            NaiveTime::from_num_seconds_from_midnight_opt(
                (t / 1_000_000) as u32,
                ((t % 1_000_000) * 1000) as u32,
            )
            .ok_or_else(invalid)?
            .format("%H:%M:%S%.f")
            .to_string()
        }
        Cell::Timestamp(v) => chrono::DateTime::from_timestamp_micros(*v)
            .ok_or_else(invalid)?
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string(),
        Cell::Uuid(v) => uuid::Uuid::from_bytes(*v).to_string(),
    })
}
fn float_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "Infinity".into()
    } else if v == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        v.to_string()
    }
}
pub fn decode_parameter(bytes: Option<&[u8]>, t: PgType, binary_format: bool) -> Result<Cell> {
    let Some(b) = bytes else {
        return Ok(Cell::Null);
    };
    if b.len() > 1024 * 1024 {
        return Err(EngineError::new("54000", "parameter exceeds 1 MiB limit"));
    }
    if let PgType::Catalog(oid) = t {
        return catalog_codec::decode(b, oid, binary_format);
    }
    if !binary_format {
        return decode_text(std::str::from_utf8(b).map_err(|_| invalid())?, t);
    }
    macro_rules! number {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(<$ty>::from_be_bytes(b.try_into().map_err(|_| invalid())?))
        };
    }
    Ok(match t {
        PgType::Catalog(_) => unreachable!("catalog parameters dispatched above"),
        PgType::Bool => {
            if b.len() != 1 || b[0] > 1 {
                return Err(invalid());
            }
            Cell::Bool(b[0] == 1)
        }
        PgType::Int2 => number!(i16, I16),
        PgType::Int4 => number!(i32, I32),
        PgType::Int8 => number!(i64, I64),
        PgType::Float4 => number!(f32, F32),
        PgType::Float8 => number!(f64, F64),
        PgType::Text | PgType::Json => {
            Cell::Text(std::str::from_utf8(b).map_err(|_| invalid())?.into())
        }
        PgType::Bytea => Cell::Bytes(b.into()),
        PgType::Numeric => Cell::Numeric(decode_numeric(b)?),
        PgType::Date => Cell::Date(
            i32::from_be_bytes(b.try_into().map_err(|_| invalid())?)
                .checked_add(PG_EPOCH_DAYS)
                .ok_or_else(invalid)?,
        ),
        PgType::Time => {
            let v = i64::from_be_bytes(b.try_into().map_err(|_| invalid())?);
            if !(0..86_400_000_000).contains(&v) {
                return Err(invalid());
            }
            Cell::Time(v)
        }
        PgType::Timestamp | PgType::Timestamptz => Cell::Timestamp(
            i64::from_be_bytes(b.try_into().map_err(|_| invalid())?)
                .checked_add(PG_EPOCH_MICROS)
                .ok_or_else(invalid)?,
        ),
        PgType::Uuid => Cell::Uuid(b.try_into().map_err(|_| invalid())?),
    })
}
fn decode_text(v: &str, t: PgType) -> Result<Cell> {
    macro_rules! parse {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(v.parse::<$ty>().map_err(|_| invalid())?)
        };
    }
    Ok(match t {
        PgType::Catalog(oid) => return catalog_codec::decode(v.as_bytes(), oid, false),
        PgType::Bool => Cell::Bool(match v.to_lowercase().as_str() {
            "t" | "true" | "1" => true,
            "f" | "false" | "0" => false,
            _ => return Err(invalid()),
        }),
        PgType::Int2 => parse!(i16, I16),
        PgType::Int4 => parse!(i32, I32),
        PgType::Int8 => parse!(i64, I64),
        PgType::Float4 => parse!(f32, F32),
        PgType::Float8 => parse!(f64, F64),
        PgType::Text | PgType::Json => Cell::Text(v.into()),
        PgType::Numeric => {
            encode_numeric(v)?;
            Cell::Numeric(v.into())
        }
        PgType::Bytea => {
            let h = v.strip_prefix("\\x").ok_or_else(invalid)?;
            if h.len() % 2 != 0 {
                return Err(invalid());
            }
            Cell::Bytes(
                (0..h.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| invalid()))
                    .collect::<Result<_>>()?,
            )
        }
        PgType::Date => {
            let d = NaiveDate::parse_from_str(v, "%Y-%m-%d").map_err(|_| invalid())?;
            Cell::Date(
                d.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
                    .num_days()
                    .try_into()
                    .map_err(|_| invalid())?,
            )
        }
        PgType::Time => {
            let t = NaiveTime::parse_from_str(v, "%H:%M:%S%.f").map_err(|_| invalid())?;
            Cell::Time(
                t.num_seconds_from_midnight() as i64 * 1_000_000 + t.nanosecond() as i64 / 1000,
            )
        }
        PgType::Timestamp => Cell::Timestamp(
            NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S%.f")
                .map_err(|_| invalid())?
                .and_utc()
                .timestamp_micros(),
        ),
        PgType::Timestamptz => Cell::Timestamp(
            chrono::DateTime::parse_from_rfc3339(v)
                .or_else(|_| chrono::DateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S%.f%#z"))
                .map_err(|_| invalid())?
                .timestamp_micros(),
        ),
        PgType::Uuid => Cell::Uuid(*uuid::Uuid::parse_str(v).map_err(|_| invalid())?.as_bytes()),
    })
}
fn encode_numeric(v: &str) -> Result<Vec<u8>> {
    let neg = v.starts_with('-');
    let v = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    if int.is_empty()
        || int.len() + frac.len() > 1000
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    let mut digits = format!(
        "{}{}{}{}",
        "0".repeat((4 - int.len() % 4) % 4),
        int,
        frac,
        "0".repeat((4 - frac.len() % 4) % 4)
    );
    let mut weight = int.len().div_ceil(4) as i16 - 1;
    let mut groups = Vec::new();
    for d in digits.as_bytes().chunks(4) {
        groups.push(
            std::str::from_utf8(d)
                .unwrap()
                .parse::<i16>()
                .map_err(|_| invalid())?,
        );
    }
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    digits.clear();
    let mut b = BytesMut::new();
    b.put_i16(groups.len() as i16);
    b.put_i16(if groups.is_empty() { 0 } else { weight });
    b.put_i16(if neg && !groups.is_empty() { 0x4000 } else { 0 });
    b.put_i16(frac.len() as i16);
    for g in groups {
        b.put_i16(g);
    }
    Ok(b.to_vec())
}
fn decode_numeric(b: &[u8]) -> Result<String> {
    if b.len() < 8 || !b.len().is_multiple_of(2) {
        return Err(invalid());
    }
    let get = |i| i16::from_be_bytes([b[i], b[i + 1]]);
    let n = get(0);
    let weight = get(2);
    let sign = get(4);
    let scale = get(6);
    if n < 0
        || b.len() != 8 + n as usize * 2
        || !matches!(sign, 0 | 0x4000)
        || !(0..=1000).contains(&scale)
        || !(-250..=250).contains(&weight)
    {
        return Err(invalid());
    }
    let mut groups = Vec::new();
    for i in 0..n as usize {
        let d = get(8 + i * 2);
        if !(0..10000).contains(&d) {
            return Err(invalid());
        }
        groups.push(d);
    }
    let digit = |position: i16| -> i16 {
        let offset = weight - position;
        if offset < 0 {
            0
        } else {
            groups.get(offset as usize).copied().unwrap_or(0)
        }
    };
    let mut int = if weight < 0 {
        "0".into()
    } else {
        (0..=weight)
            .rev()
            .map(|p| {
                if p == weight {
                    digit(p).to_string()
                } else {
                    format!("{:04}", digit(p))
                }
            })
            .collect::<String>()
    };
    if scale > 0 {
        int.push('.');
        let frac = (1..=((scale + 3) / 4))
            .map(|p| format!("{:04}", digit(-p)))
            .collect::<String>();
        int.push_str(&frac[..scale as usize]);
    }
    if sign == 0x4000 {
        int.insert(0, '-');
    }
    Ok(int)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_binary_preserves_precision_scale_and_sign() {
        for value in [
            "0",
            "-0.0001",
            "12345678901234567890123456789012345678.25",
            "10000",
            "0.00100",
            "-123.4500",
        ] {
            let encoded = encode_numeric(value).unwrap();
            let decoded = decode_numeric(&encoded).unwrap();
            assert_eq!(decoded, value);
        }
    }
    #[test]
    fn binary_scalar_codecs_and_null_round_trip() {
        for (c, t) in [
            (Cell::I32(123), PgType::Int4),
            (Cell::Date(0), PgType::Date),
            (Cell::Timestamp(0), PgType::Timestamp),
            (Cell::Time(123456), PgType::Time),
            (Cell::Bool(true), PgType::Bool),
        ] {
            let b = binary(&c, t).unwrap();
            assert_eq!(
                text(&decode_parameter(Some(&b), t, true).unwrap()).unwrap(),
                text(&c).unwrap()
            );
        }
        assert!(matches!(
            decode_parameter(None, PgType::Text, true).unwrap(),
            Cell::Null
        ));
    }
    #[test]
    fn malformed_binary_parameters_fail_without_panics() {
        for t in [
            PgType::Int4,
            PgType::Bool,
            PgType::Date,
            PgType::Timestamp,
            PgType::Numeric,
            PgType::Uuid,
        ] {
            assert!(decode_parameter(Some(&[42, 43, 44]), t, true).is_err());
        }
    }
}
