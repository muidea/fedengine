use duckdb::{
    arrow::{
        array::*,
        datatypes::{DataType, TimeUnit},
    },
    types::Value,
};
use fedengine_contract::{Cell, PgType};
use fedengine_core::*;
pub(super) fn type_name(t: &str) -> Result<PgType> {
    Ok(match t {
        "BOOLEAN" => PgType::Bool,
        "TINYINT" | "SMALLINT" => PgType::Int2,
        "INTEGER" | "UTINYINT" | "USMALLINT" => PgType::Int4,
        "BIGINT" | "UINTEGER" => PgType::Int8,
        "FLOAT" => PgType::Float4,
        "DOUBLE" => PgType::Float8,
        "VARCHAR" | "NULL" => PgType::Text,
        "BLOB" => PgType::Bytea,
        "DATE" => PgType::Date,
        "TIME" => PgType::Time,
        "TIMESTAMP" | "TIMESTAMP_NS" | "TIMESTAMP_MS" | "TIMESTAMP_S" => PgType::Timestamp,
        "TIMESTAMP WITH TIME ZONE" => PgType::Timestamptz,
        "UUID" => PgType::Uuid,
        "JSON" => PgType::Json,
        v if v.starts_with("DECIMAL") || ["HUGEINT", "UHUGEINT", "UBIGINT"].contains(&v) => {
            PgType::Numeric
        }
        _ => {
            return Err(EngineError::unsupported(
                "result type has no verified PG encoding",
            ));
        }
    })
}
pub(super) fn pg_type(t: &DataType) -> Result<PgType> {
    Ok(match t {
        DataType::Boolean => PgType::Bool,
        DataType::Int8 | DataType::Int16 => PgType::Int2,
        DataType::UInt8 | DataType::UInt16 | DataType::Int32 => PgType::Int4,
        DataType::UInt32 | DataType::Int64 => PgType::Int8,
        DataType::UInt64 | DataType::Decimal128(..) | DataType::Decimal256(..) => PgType::Numeric,
        DataType::Float32 => PgType::Float4,
        DataType::Float64 => PgType::Float8,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Null => PgType::Text,
        DataType::Binary | DataType::LargeBinary => PgType::Bytea,
        DataType::Date32 => PgType::Date,
        DataType::Time64(_) => PgType::Time,
        DataType::Timestamp(_, tz) => {
            if tz.is_some() {
                PgType::Timestamptz
            } else {
                PgType::Timestamp
            }
        }
        _ => {
            return Err(EngineError::unsupported(
                "Arrow result type is not supported by PG encoder",
            ));
        }
    })
}
pub(super) fn cell(a: &dyn Array, i: usize) -> Result<Cell> {
    if a.is_null(i) {
        return Ok(Cell::Null);
    }
    macro_rules! value {
        ($ty:ty,$variant:ident) => {
            Cell::$variant(a.as_any().downcast_ref::<$ty>().unwrap().value(i).into())
        };
    }
    Ok(match a.data_type() {
        DataType::Boolean => value!(BooleanArray, Bool),
        DataType::Int8 => value!(Int8Array, I16),
        DataType::Int16 => value!(Int16Array, I16),
        DataType::UInt8 => value!(UInt8Array, I32),
        DataType::UInt16 => value!(UInt16Array, I32),
        DataType::Int32 => value!(Int32Array, I32),
        DataType::UInt32 => value!(UInt32Array, I64),
        DataType::Int64 => value!(Int64Array, I64),
        DataType::Float32 => value!(Float32Array, F32),
        DataType::Float64 => value!(Float64Array, F64),
        DataType::Utf8 => value!(StringArray, Text),
        DataType::LargeUtf8 => value!(LargeStringArray, Text),
        DataType::Binary => value!(BinaryArray, Bytes),
        DataType::LargeBinary => value!(LargeBinaryArray, Bytes),
        DataType::Date32 => value!(Date32Array, Date),
        DataType::UInt64 => Cell::Numeric(
            a.as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(i)
                .to_string(),
        ),
        DataType::Decimal128(_, scale) => Cell::Numeric(decimal(
            a.as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(i)
                .to_string(),
            *scale,
        )),
        DataType::Decimal256(_, scale) => Cell::Numeric(decimal(
            a.as_any()
                .downcast_ref::<Decimal256Array>()
                .unwrap()
                .value(i)
                .to_string(),
            *scale,
        )),
        DataType::Time64(TimeUnit::Microsecond) => value!(Time64MicrosecondArray, Time),
        DataType::Time64(TimeUnit::Nanosecond) => Cell::Time(
            a.as_any()
                .downcast_ref::<Time64NanosecondArray>()
                .unwrap()
                .value(i)
                / 1000,
        ),
        DataType::Timestamp(unit, _) => Cell::Timestamp(
            match unit {
                TimeUnit::Second => a
                    .as_any()
                    .downcast_ref::<TimestampSecondArray>()
                    .unwrap()
                    .value(i)
                    .checked_mul(1_000_000),
                TimeUnit::Millisecond => a
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .unwrap()
                    .value(i)
                    .checked_mul(1000),
                TimeUnit::Microsecond => Some(
                    a.as_any()
                        .downcast_ref::<TimestampMicrosecondArray>()
                        .unwrap()
                        .value(i),
                ),
                TimeUnit::Nanosecond => Some(
                    a.as_any()
                        .downcast_ref::<TimestampNanosecondArray>()
                        .unwrap()
                        .value(i)
                        / 1000,
                ),
            }
            .ok_or_else(|| EngineError::new("22008", "timestamp out of range"))?,
        ),
        _ => {
            return Err(EngineError::unsupported(
                "result value encoding is unsupported",
            ));
        }
    })
}
fn decimal(mut s: String, scale: i8) -> String {
    if scale <= 0 {
        s.push_str(&"0".repeat((-scale) as usize));
        return s;
    }
    let neg = s.starts_with('-');
    if neg {
        s.remove(0);
    }
    while s.len() <= scale as usize {
        s.insert(0, '0');
    }
    s.insert(s.len() - scale as usize, '.');
    if neg {
        s.insert(0, '-');
    }
    s
}
pub(super) fn to_duckdb(c: &Cell) -> Value {
    match c {
        Cell::Null => Value::Null,
        Cell::Bool(v) => Value::Boolean(*v),
        Cell::I16(v) => Value::SmallInt(*v),
        Cell::I32(v) => Value::Int(*v),
        Cell::I64(v) => Value::BigInt(*v),
        Cell::F32(v) => Value::Float(*v),
        Cell::F64(v) => Value::Double(*v),
        Cell::Text(v) | Cell::Numeric(v) => Value::Text(v.clone()),
        Cell::Bytes(v) => Value::Blob(v.clone()),
        Cell::Date(v) => Value::Date32(*v),
        Cell::Time(v) => Value::Time64(duckdb::types::TimeUnit::Microsecond, *v),
        Cell::Timestamp(v) => Value::Timestamp(duckdb::types::TimeUnit::Microsecond, *v),
        Cell::Uuid(v) => Value::Text(uuid_text(v)),
    }
}
fn uuid_text(v: &[u8; 16]) -> String {
    let h = v.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}
