use duckdb::{
    arrow::{
        array::*,
        datatypes::{DataType, TimeUnit},
    },
    types::Value,
};
use fedengine_contract::{Cell, PgType};
use fedengine_core::*;
pub(super) fn type_modifier(t: &str) -> i32 {
    if let Some(args) = t.strip_prefix("DECIMAL(").and_then(|s| s.strip_suffix(')'))
        && let Some((p, s)) = args.split_once(',')
        && let (Ok(p), Ok(s)) = (p.trim().parse::<i32>(), s.trim().parse::<i32>())
    {
        return ((p << 16) | s) + 4;
    }
    -1
}
pub(super) fn type_name(t: &str) -> Result<PgType> {
    Ok(match t {
        "BOOLEAN" => PgType::Bool,
        "TINYINT" | "SMALLINT" => PgType::Int2,
        "INTEGER" | "UTINYINT" | "USMALLINT" => PgType::Int4,
        "BIGINT" | "UINTEGER" => PgType::Int8,
        "FLOAT" => PgType::Float4,
        "DOUBLE" => PgType::Float8,
        "VARCHAR" | "NULL" | "\"NULL\"" => PgType::Text,
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
pub(super) fn cell(a: &dyn Array, i: usize) -> Result<Cell> {
    if matches!(a.data_type(), DataType::Null) || a.is_null(i) {
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
        DataType::List(_) => {
            let values = a.as_any().downcast_ref::<ListArray>().unwrap().value(i);
            Cell::Array {
                lower_bound: 1,
                values: (0..values.len())
                    .map(|i| cell(values.as_ref(), i))
                    .collect::<Result<_>>()?,
            }
        }
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
        Cell::Oid(v) => Value::UInt(*v),
        Cell::NamedOid { oid, .. } => Value::UInt(*oid),
        Cell::Array { values, .. } => Value::List(values.iter().map(to_duckdb).collect()),
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

/// Normalize physical Arrow values to the proven PostgreSQL result type, checking narrowing.
pub(super) fn coerce(c: Cell, t: PgType) -> Result<Cell> {
    if matches!(c, Cell::Null) {
        return Ok(c);
    }
    let number = match &c {
        Cell::I16(v) => Some(i64::from(*v)),
        Cell::I32(v) => Some(i64::from(*v)),
        Cell::I64(v) => Some(*v),
        _ => None,
    };
    let range = || {
        EngineError::new(
            "22003",
            "metadata integer is outside its PostgreSQL type range",
        )
    };
    Ok(match (t, number, c) {
        (PgType::Int2, Some(n), _) => Cell::I16(n.try_into().map_err(|_| range())?),
        (PgType::Int4, Some(n), _) => Cell::I32(n.try_into().map_err(|_| range())?),
        (PgType::Int8, Some(n), _) => Cell::I64(n),
        (PgType::Catalog(24 | 26 | 28 | 29 | 2205 | 2206), Some(n), _) => {
            Cell::Oid(n.try_into().map_err(|_| range())?)
        }
        (PgType::Catalog(oid), _, Cell::Array { values, .. }) => {
            let element = match oid {
                22 | 1005 => PgType::Int2,
                30 | 1028 => PgType::Catalog(26),
                1007 => PgType::Int4,
                1016 => PgType::Int8,
                1000 => PgType::Bool,
                _ => PgType::Text,
            };
            Cell::Array {
                lower_bound: if matches!(oid, 22 | 30) { 0 } else { 1 },
                values: values
                    .into_iter()
                    .map(|c| coerce(c, element))
                    .collect::<Result<_>>()?,
            }
        }
        (_, _, c) => c,
    })
}
