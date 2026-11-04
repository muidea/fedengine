use super::{binary as scalar_binary, decode_parameter, invalid, text as scalar_text};
use bytes::{BufMut, BytesMut};
use fedengine_contract::{Cell, PgType};
use fedengine_core::*;

fn element(oid: u32) -> Option<PgType> {
    Some(match oid {
        22 | 1005 => PgType::Int2,
        30 | 1028 => PgType::Catalog(26),
        1007 => PgType::Int4,
        1016 => PgType::Int8,
        1000 => PgType::Bool,
        1002 => PgType::Catalog(18),
        1003 => PgType::Catalog(19),
        1009 | 1014 | 1015 | 1034 => PgType::Text,
        _ => return None,
    })
}
fn oid_value(c: &Cell) -> Result<u32> {
    match c {
        Cell::Oid(v) => Ok(*v),
        Cell::NamedOid { oid, .. } => Ok(*oid),
        Cell::I32(v) => u32::try_from(*v).map_err(|_| invalid()),
        Cell::I64(v) => u32::try_from(*v).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}
pub(super) fn binary(c: &Cell, oid: u32) -> Result<Vec<u8>> {
    if let Some(t) = element(oid) {
        let Cell::Array {
            lower_bound,
            values,
        } = c
        else {
            return Err(invalid());
        };
        if values.len() > 100_000
            || matches!(oid, 22 | 30) && values.iter().any(|v| matches!(v, Cell::Null))
        {
            return Err(invalid());
        }
        let mut b = BytesMut::new();
        let ndim = if values.is_empty() && !matches!(oid, 22 | 30) {
            0
        } else {
            1
        };
        b.put_i32(ndim);
        b.put_i32(i32::from(values.iter().any(|v| matches!(v, Cell::Null))));
        b.put_u32(t.oid());
        if ndim == 1 {
            b.put_i32(i32::try_from(values.len()).map_err(|_| invalid())?);
            b.put_i32(if matches!(oid, 22 | 30) {
                0
            } else {
                *lower_bound
            });
        }
        for v in values {
            if matches!(v, Cell::Null) {
                b.put_i32(-1);
            } else {
                let data = scalar_binary(v, t)?;
                b.put_i32(data.len().try_into().map_err(|_| invalid())?);
                b.extend_from_slice(&data);
            }
        }
        return Ok(b.to_vec());
    }
    Ok(match oid {
        26 | 28 | 29 | 24 | 2205 | 2206 => oid_value(c)?.to_be_bytes().to_vec(),
        18 => {
            let Cell::Text(v) = c else {
                return Err(invalid());
            };
            if v.is_empty() {
                vec![0]
            } else {
                if v.len() != 1 {
                    return Err(invalid());
                }
                v.as_bytes().to_vec()
            }
        }
        19 | 194 => {
            let Cell::Text(v) = c else {
                return Err(invalid());
            };
            v.as_bytes().to_vec()
        }
        _ => {
            return Err(EngineError::unsupported(
                "catalog type has no non-NULL binary codec",
            ));
        }
    })
}
pub(super) fn text(c: &Cell, t: PgType) -> Result<Option<String>> {
    let PgType::Catalog(oid) = t else {
        return Ok(None);
    };
    let Some(_) = element(oid) else {
        return Ok(None);
    };
    let Cell::Array {
        lower_bound,
        values,
    } = c
    else {
        return Err(invalid());
    };
    let vector = matches!(oid, 22 | 30);
    let items = values
        .iter()
        .map(|v| {
            if matches!(v, Cell::Null) {
                if vector {
                    return Err(invalid());
                }
                return Ok("NULL".into());
            }
            let s = scalar_text(v)?;
            if vector {
                Ok(s)
            } else {
                Ok(format!(
                    "\"{}\"",
                    s.replace('\\', "\\\\").replace('"', "\\\"")
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if vector {
        return Ok(Some(items.join(" ")));
    }
    let dims = if *lower_bound == 1 || values.is_empty() {
        String::new()
    } else {
        let upper =
            i64::from(*lower_bound) + i64::try_from(values.len()).map_err(|_| invalid())? - 1;
        format!("[{lower_bound}:{upper}]=")
    };
    Ok(Some(format!("{dims}{{{}}}", items.join(","))))
}
pub(super) fn decode(b: &[u8], oid: u32, is_binary: bool) -> Result<Cell> {
    if let Some(t) = element(oid) {
        if !is_binary {
            return array_text(std::str::from_utf8(b).map_err(|_| invalid())?, oid, t);
        }
        let mut input = b;
        fn read(input: &mut &[u8]) -> Result<i32> {
            if input.len() < 4 {
                return Err(invalid());
            }
            let (n, rest) = input.split_at(4);
            *input = rest;
            Ok(i32::from_be_bytes(n.try_into().unwrap()))
        }
        let ndim = read(&mut input)?;
        let nulls = read(&mut input)?;
        let elem = read(&mut input)? as u32;
        if !matches!(nulls, 0 | 1) || elem != t.oid() {
            return Err(invalid());
        }
        if ndim == 0 {
            if matches!(oid, 22 | 30) || !input.is_empty() {
                return Err(invalid());
            }
            return Ok(Cell::Array {
                lower_bound: 1,
                values: vec![],
            });
        }
        if ndim != 1 {
            return Err(EngineError::unsupported(
                "only one-dimensional catalog arrays are supported",
            ));
        }
        let len = read(&mut input)?;
        let lower_bound = read(&mut input)?;
        if !(0..=100_000).contains(&len)
            || i64::from(lower_bound) + i64::from(len) - 1 > i64::from(i32::MAX)
            || matches!(oid, 22 | 30) && lower_bound != 0
        {
            return Err(invalid());
        }
        let mut values = Vec::with_capacity(len as usize);
        for _ in 0..len {
            let n = read(&mut input)?;
            if n == -1 {
                if nulls == 0 || matches!(oid, 22 | 30) {
                    return Err(invalid());
                }
                values.push(Cell::Null);
                continue;
            }
            if n < 0 || n as usize > input.len() {
                return Err(invalid());
            }
            let (v, rest) = input.split_at(n as usize);
            input = rest;
            values.push(decode_parameter(Some(v), t, true)?);
        }
        if !input.is_empty() {
            return Err(invalid());
        }
        return Ok(Cell::Array {
            lower_bound,
            values,
        });
    }
    if matches!(oid, 26 | 28 | 29 | 24 | 2205 | 2206) {
        return Ok(Cell::Oid(if is_binary {
            u32::from_be_bytes(b.try_into().map_err(|_| invalid())?)
        } else {
            std::str::from_utf8(b)
                .map_err(|_| invalid())?
                .parse()
                .map_err(|_| invalid())?
        }));
    }
    if !matches!(oid, 18 | 19 | 194) {
        return Err(EngineError::unsupported(
            "catalog parameter type is not supported",
        ));
    }
    if oid == 18 && b == [0] {
        return Ok(Cell::Text(String::new()));
    }
    let v = std::str::from_utf8(b).map_err(|_| invalid())?;
    if v.as_bytes().contains(&0) || oid == 18 && v.len() > 1 {
        return Err(invalid());
    }
    Ok(Cell::Text(v.into()))
}
fn array_text(s: &str, oid: u32, t: PgType) -> Result<Cell> {
    if matches!(oid, 22 | 30) {
        let values = s
            .split_whitespace()
            .map(|v| decode_parameter(Some(v.as_bytes()), t, false))
            .collect::<Result<_>>()?;
        return Ok(Cell::Array {
            lower_bound: 0,
            values,
        });
    }
    let mut lower_bound = 1;
    let mut declared_upper = None;
    let s = if let Some(dims) = s.strip_prefix('[') {
        let (dims, rest) = dims.split_once("]=").ok_or_else(invalid)?;
        let (l, u) = dims.split_once(':').ok_or_else(invalid)?;
        lower_bound = l.parse::<i32>().map_err(|_| invalid())?;
        declared_upper = Some(u.parse::<i32>().map_err(|_| invalid())?);
        rest
    } else {
        s
    };
    let body = s
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(invalid)?;
    let mut chars = body.chars().peekable();
    let mut values = Vec::new();
    while chars.peek().is_some() {
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
        }
        let mut item = String::new();
        let mut closed = !quoted;
        let mut escaped = false;
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                escaped = true;
                item.push(chars.next().ok_or_else(invalid)?);
                continue;
            }
            if quoted && ch == '"' {
                closed = true;
                break;
            }
            if !quoted && ch == ',' {
                break;
            }
            if !quoted && matches!(ch, '{' | '}' | '"') {
                return Err(invalid());
            }
            item.push(ch);
        }
        if !closed {
            return Err(invalid());
        }
        if quoted && chars.peek().is_some() && chars.next() != Some(',') {
            return Err(invalid());
        }
        let item = if quoted { item } else { item.trim().into() };
        if !quoted && item.is_empty() {
            return Err(invalid());
        }
        values.push(
            if !quoted && !escaped && item.eq_ignore_ascii_case("null") {
                Cell::Null
            } else {
                decode_parameter(Some(item.as_bytes()), t, false)?
            },
        );
        if values.len() > 100_000 {
            return Err(invalid());
        }
    }
    if body.ends_with(',')
        || declared_upper
            .is_some_and(|u| i64::from(u) != i64::from(lower_bound) + values.len() as i64 - 1)
    {
        return Err(invalid());
    }
    Ok(Cell::Array {
        lower_bound,
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oid_is_unsigned_and_binary_vectors_have_zero_based_bounds() {
        assert!(matches!(
            decode(&u32::MAX.to_be_bytes(), 26, true).unwrap(),
            Cell::Oid(u32::MAX)
        ));
        let v = Cell::Array {
            lower_bound: 0,
            values: vec![Cell::I16(1), Cell::I16(32767)],
        };
        let b = binary(&v, 22).unwrap();
        assert_eq!(
            &b[..20],
            &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 21, 0, 0, 0, 2, 0, 0, 0, 0]
        );
        let Cell::Array {
            lower_bound,
            values,
        } = decode(&b, 22, true).unwrap()
        else {
            panic!()
        };
        assert_eq!(lower_bound, 0);
        assert_eq!(values.len(), 2);
        assert_eq!(
            text(&v, PgType::Catalog(22)).unwrap().as_deref(),
            Some("1 32767")
        );
    }
    #[test]
    fn nullable_text_arrays_preserve_quotes_escapes_and_bounds() {
        let v = decode(br#"[0:3]={"NULL",NULL,"a,b","c\\d"}"#, 1009, false).unwrap();
        let b = binary(&v, 1009).unwrap();
        let decoded = decode(&b, 1009, true).unwrap();
        assert_eq!(
            text(&v, PgType::Catalog(1009)).unwrap(),
            text(&decoded, PgType::Catalog(1009)).unwrap()
        );
        assert!(
            matches!(decode(br#"{N\ULL}"#,1009,false).unwrap(),Cell::Array{values,..} if matches!(&values[0],Cell::Text(v) if v=="NULL"))
        );
    }
    #[test]
    fn malformed_arrays_are_rejected_without_panics() {
        for input in ["{,x}", "{x,}", "{{1}}", "[0:3]={1,2}", "{\"unterminated}"] {
            assert!(decode(input.as_bytes(), 1009, false).is_err(), "{input}");
        }
        let valid = binary(
            &Cell::Array {
                lower_bound: 0,
                values: vec![Cell::I16(1)],
            },
            22,
        )
        .unwrap();
        for n in 0..valid.len() {
            assert!(decode(&valid[..n], 22, true).is_err());
        }
        let mut wrong = valid.clone();
        wrong[11] = 23;
        assert!(decode(&wrong, 22, true).is_err());
        let mut wrong = valid.clone();
        wrong[19] = 1;
        assert!(decode(&wrong, 22, true).is_err());
        let mut wrong = valid.clone();
        wrong.extend([0]);
        assert!(decode(&wrong, 22, true).is_err());
        assert!(
            binary(
                &Cell::Array {
                    lower_bound: 0,
                    values: vec![Cell::Null]
                },
                22
            )
            .is_err()
        );
    }
}
