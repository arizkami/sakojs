//! Decoding of text-format column values by type OID.

/// A decoded column value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  Null,
  Bool(bool),
  /// int2 / int4 / oid
  Int(i32),
  /// float4 / float8
  Float(f64),
  /// text, varchar, uuid, int8, numeric, and anything else
  Text(String),
  /// json / jsonb (unparsed)
  Json(String),
  Bytes(Vec<u8>),
  /// date / timestamp / timestamptz, as milliseconds since the Unix epoch
  Date(f64),
  Array(Vec<Value>),
}

pub mod oid {
  pub const BOOL: u32 = 16;
  pub const BYTEA: u32 = 17;
  pub const INT8: u32 = 20;
  pub const INT2: u32 = 21;
  pub const INT4: u32 = 23;
  pub const OID: u32 = 26;
  pub const JSON: u32 = 114;
  pub const FLOAT4: u32 = 700;
  pub const FLOAT8: u32 = 701;
  pub const DATE: u32 = 1082;
  pub const TIMESTAMP: u32 = 1114;
  pub const TIMESTAMPTZ: u32 = 1184;
  pub const NUMERIC: u32 = 1700;
  pub const JSONB: u32 = 3802;
}

/// Element type of an array type, if known.
pub fn element_oid(array_oid: u32) -> Option<u32> {
  Some(match array_oid {
    1000 => oid::BOOL,
    1001 => oid::BYTEA,
    1005 => oid::INT2,
    1007 => oid::INT4,
    1016 => oid::INT8,
    1021 => oid::FLOAT4,
    1022 => oid::FLOAT8,
    1009 | 1015 | 1014 | 1003 | 2951 => 25, // text, varchar, bpchar, name, uuid
    199 => oid::JSON,
    3807 => oid::JSONB,
    1231 => oid::NUMERIC,
    1182 => oid::DATE,
    1115 => oid::TIMESTAMP,
    1185 => oid::TIMESTAMPTZ,
    1028 => oid::OID,
    _ => return None,
  })
}

/// Types requested in binary result format once a statement's columns are
/// known: the server copies fixed-size values instead of formatting text
/// (timestamp formatting in particular is expensive), and bytea skips hex.
pub fn prefers_binary(type_oid: u32) -> bool {
  matches!(
    type_oid,
    oid::BOOL
      | oid::INT2
      | oid::INT4
      | oid::INT8
      | oid::FLOAT4
      | oid::FLOAT8
      | oid::NUMERIC
      | oid::DATE
      | oid::TIMESTAMP
      | oid::TIMESTAMPTZ
      | oid::BYTEA
  )
}

/// Binary NUMERIC (base-10000 digit groups) to its exact decimal text, as
/// PostgreSQL prints it.
pub fn numeric_to_string(raw: &[u8]) -> Option<String> {
  let u16_at = |i: usize| raw.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
  let ndigits = u16_at(0)? as usize;
  let weight = u16_at(2)? as i16 as i32;
  let sign = u16_at(4)?;
  let dscale = u16_at(6)? as usize;
  match sign {
    0xC000 => return Some("NaN".into()),
    0xD000 => return Some("Infinity".into()),
    0xF000 => return Some("-Infinity".into()),
    _ => {}
  }
  if raw.len() < 8 + 2 * ndigits {
    return None;
  }
  let group = |i: i32| -> u16 { if i >= 0 && (i as usize) < ndigits { u16_at(8 + 2 * i as usize).unwrap() } else { 0 } };
  let push4 = |out: &mut Vec<u8>, g: u16| {
    out.extend_from_slice(&[b'0' + (g / 1000) as u8, b'0' + (g / 100 % 10) as u8, b'0' + (g / 10 % 10) as u8, b'0' + (g % 10) as u8]);
  };
  let mut out: Vec<u8> = Vec::with_capacity(4 * (weight.max(0) as usize + 1) + dscale + 2);
  if sign == 0x4000 {
    out.push(b'-');
  }
  if weight < 0 {
    out.push(b'0');
  } else {
    for i in 0..=weight {
      let g = group(i);
      if i == 0 {
        // first group without leading zeros
        let start = out.len();
        push4(&mut out, g);
        let zeros = out[start..start + 3].iter().take_while(|&&c| c == b'0').count();
        out.drain(start..start + zeros);
      } else {
        push4(&mut out, g);
      }
    }
  }
  if dscale > 0 {
    out.push(b'.');
    let start = out.len();
    let mut i = weight + 1;
    while out.len() - start < dscale {
      push4(&mut out, group(i));
      i += 1;
    }
    out.truncate(start + dscale);
  }
  // Only ASCII digits, '-' and '.' were written.
  Some(String::from_utf8(out).unwrap())
}

/// Microseconds between 1970-01-01 and 2000-01-01 (the PostgreSQL epoch).
const PG_EPOCH_MS: f64 = 946_684_800_000.0;

/// Decodes a binary-format value of a [`prefers_binary`] type.
pub fn decode_binary(type_oid: u32, raw: Option<&[u8]>) -> Value {
  let Some(raw) = raw else { return Value::Null };
  let be = |n: usize| -> Option<[u8; 8]> {
    let mut b = [0u8; 8];
    b[8 - n..].copy_from_slice(raw.get(..n)?);
    Some(b)
  };
  let int = |n: usize| be(n).map(|b| {
    // sign-extend
    let shift = 64 - 8 * n as u32;
    (i64::from_be_bytes(b) << shift) >> shift
  });
  match type_oid {
    oid::BOOL => Value::Bool(raw.first() == Some(&1)),
    oid::INT2 => int(2).map_or(Value::Null, |v| Value::Int(v as i32)),
    oid::INT4 => int(4).map_or(Value::Null, |v| Value::Int(v as i32)),
    oid::INT8 => int(8).map_or(Value::Null, |v| Value::Text(v.to_string())),
    oid::FLOAT4 => raw.get(..4).map_or(Value::Null, |b| Value::Float(f32::from_be_bytes(b.try_into().unwrap()) as f64)),
    oid::FLOAT8 => raw.get(..8).map_or(Value::Null, |b| Value::Float(f64::from_be_bytes(b.try_into().unwrap()))),
    oid::TIMESTAMP | oid::TIMESTAMPTZ => match int(8) {
      Some(i64::MAX) => Value::Text("infinity".into()),
      Some(i64::MIN) => Value::Text("-infinity".into()),
      Some(us) => Value::Date(PG_EPOCH_MS + (us as f64 / 1000.0).floor()),
      None => Value::Null,
    },
    oid::DATE => match int(4) {
      Some(v) if v == i32::MAX as i64 => Value::Text("infinity".into()),
      Some(v) if v == i32::MIN as i64 => Value::Text("-infinity".into()),
      Some(days) => Value::Date(PG_EPOCH_MS + days as f64 * 86_400_000.0),
      None => Value::Null,
    },
    oid::BYTEA => Value::Bytes(raw.to_vec()),
    oid::NUMERIC => numeric_to_string(raw).map_or(Value::Null, Value::Text),
    _ => Value::Text(String::from_utf8_lossy(raw).into_owned()),
  }
}

pub fn decode(type_oid: u32, raw: Option<&[u8]>) -> Value {
  let Some(raw) = raw else { return Value::Null };
  let text = || String::from_utf8_lossy(raw).into_owned();
  match type_oid {
    oid::BOOL => Value::Bool(raw == b"t"),
    oid::INT2 | oid::INT4 => std::str::from_utf8(raw).ok().and_then(|s| s.parse().ok()).map_or_else(|| Value::Text(text()), Value::Int),
    oid::OID => std::str::from_utf8(raw)
      .ok()
      .and_then(|s| s.parse::<u32>().ok())
      .map_or_else(|| Value::Text(text()), |v| Value::Float(v as f64)),
    oid::FLOAT4 | oid::FLOAT8 => {
      let s = std::str::from_utf8(raw).unwrap_or("NaN");
      Value::Float(match s {
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => s.parse().unwrap_or(f64::NAN),
      })
    }
    oid::JSON | oid::JSONB => Value::Json(text()),
    oid::BYTEA => decode_bytea(raw).map_or_else(|| Value::Text(text()), Value::Bytes),
    oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => {
      std::str::from_utf8(raw).ok().and_then(parse_timestamp).map_or_else(|| Value::Text(text()), Value::Date)
    }
    _ => match element_oid(type_oid) {
      Some(elem) => std::str::from_utf8(raw).ok().and_then(|s| parse_array(s, elem)).unwrap_or_else(|| Value::Text(text())),
      None => Value::Text(text()),
    },
  }
}

/// `\x` hex format (the PostgreSQL default since 9.0).
pub fn decode_bytea(raw: &[u8]) -> Option<Vec<u8>> {
  let hex = raw.strip_prefix(b"\\x")?;
  if hex.len() % 2 != 0 {
    return None;
  }
  hex
    .chunks(2)
    .map(|p| {
      let s = std::str::from_utf8(p).ok()?;
      u8::from_str_radix(s, 16).ok()
    })
    .collect()
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
  let y = if m <= 2 { y - 1 } else { y };
  let era = if y >= 0 { y } else { y - 399 } / 400;
  let yoe = y - era * 400;
  let mp = (m + 9) % 12;
  let doy = (153 * mp + 2) / 5 + d - 1;
  let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  era * 146097 + doe - 719468
}

/// Parses `YYYY-MM-DD[ HH:MM:SS[.ffffff]][+HH[:MM]]` (ISO DateStyle).
/// Timestamps without a zone are taken as UTC.
pub fn parse_timestamp(s: &str) -> Option<f64> {
  let s = s.trim();
  if s.ends_with(" BC") || s == "infinity" || s == "-infinity" {
    return None;
  }
  let (date, rest) = s.split_at(s.find([' ', 'T']).unwrap_or(s.len()));
  let mut dp = date.splitn(3, '-');
  let y: i64 = dp.next()?.parse().ok()?;
  let mo: i64 = dp.next()?.parse().ok()?;
  let d: i64 = dp.next()?.parse().ok()?;
  let mut ms = days_from_civil(y, mo, d) as f64 * 86_400_000.0;
  let rest = rest.trim_start_matches([' ', 'T']);
  if rest.is_empty() {
    return Some(ms);
  }
  // Split the time from a trailing zone offset.
  let zone_at = rest.rfind(['+', '-']).filter(|&i| i >= 5);
  let (time, zone) = match zone_at {
    Some(i) => (&rest[..i], Some(&rest[i..])),
    None => (rest.trim_end_matches('Z'), None),
  };
  let mut tp = time.splitn(3, ':');
  let h: f64 = tp.next()?.parse().ok()?;
  let mi: f64 = tp.next()?.parse().ok()?;
  let sec: f64 = tp.next().unwrap_or("0").parse().ok()?;
  // Whole milliseconds (like JS Dates and the binary path): sub-millisecond
  // digits are dropped.
  ms += (h * 3600.0 + mi * 60.0) * 1000.0 + ((sec * 1_000_000.0).round() / 1000.0).floor();
  if let Some(z) = zone {
    let sign = if z.starts_with('-') { -1.0 } else { 1.0 };
    let mut zp = z[1..].split(':');
    let zh: f64 = zp.next()?.parse().ok()?;
    let zm: f64 = zp.next().unwrap_or("0").parse().ok()?;
    let zs: f64 = zp.next().unwrap_or("0").parse().ok()?;
    ms -= sign * (zh * 3600.0 + zm * 60.0 + zs) * 1000.0;
  }
  Some(ms)
}

/// Parses an array literal such as `{1,2,NULL,"a \"b\""}` (nested for
/// multi-dimensional arrays).
pub fn parse_array(s: &str, elem: u32) -> Option<Value> {
  let bytes = s.as_bytes();
  // Optional dimension decoration: "[1:3]={...}"
  let start = if bytes.first() == Some(&b'[') { s.find('=')? + 1 } else { 0 };
  let mut pos = start;
  let v = parse_level(bytes, &mut pos, elem)?;
  Some(v)
}

fn parse_level(b: &[u8], pos: &mut usize, elem: u32) -> Option<Value> {
  if b.get(*pos) != Some(&b'{') {
    return None;
  }
  *pos += 1;
  let mut items = Vec::new();
  if b.get(*pos) == Some(&b'}') {
    *pos += 1;
    return Some(Value::Array(items));
  }
  loop {
    match b.get(*pos)? {
      b'{' => items.push(parse_level(b, pos, elem)?),
      b'"' => {
        *pos += 1;
        let mut out = Vec::new();
        loop {
          match b.get(*pos)? {
            b'\\' => {
              out.push(*b.get(*pos + 1)?);
              *pos += 2;
            }
            b'"' => {
              *pos += 1;
              break;
            }
            c => {
              out.push(*c);
              *pos += 1;
            }
          }
        }
        items.push(decode(elem, Some(&out)));
      }
      _ => {
        let begin = *pos;
        while !matches!(b.get(*pos)?, b',' | b'}') {
          *pos += 1;
        }
        let tok = &b[begin..*pos];
        items.push(if tok.eq_ignore_ascii_case(b"NULL") { Value::Null } else { decode(elem, Some(tok)) });
      }
    }
    match b.get(*pos)? {
      b',' => *pos += 1,
      b'}' => {
        *pos += 1;
        return Some(Value::Array(items));
      }
      _ => return None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scalars() {
    assert_eq!(decode(oid::INT4, Some(b"-42")), Value::Int(-42));
    assert_eq!(decode(oid::INT8, Some(b"9007199254740993")), Value::Text("9007199254740993".into()));
    assert_eq!(decode(oid::BOOL, Some(b"t")), Value::Bool(true));
    assert_eq!(decode(oid::BYTEA, Some(b"\\x6869")), Value::Bytes(b"hi".to_vec()));
    assert_eq!(decode(oid::FLOAT8, Some(b"-Infinity")), Value::Float(f64::NEG_INFINITY));
    assert_eq!(decode(25, None), Value::Null);
  }

  #[test]
  fn timestamps() {
    assert_eq!(parse_timestamp("1970-01-01"), Some(0.0));
    assert_eq!(parse_timestamp("2000-03-01 12:30:45.5"), Some(951_913_845_500.0));
    assert_eq!(parse_timestamp("2000-03-01 12:30:45.5+02"), Some(951_906_645_500.0));
    assert_eq!(parse_timestamp("2000-03-01 12:30:45.5-05:30"), Some(951_933_645_500.0));
  }

  #[test]
  fn arrays() {
    assert_eq!(
      parse_array("{1,NULL,3}", oid::INT4),
      Some(Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]))
    );
    assert_eq!(
      parse_array(r#"{"a,b","c \"d\"",e}"#, 25),
      Some(Value::Array(vec![Value::Text("a,b".into()), Value::Text("c \"d\"".into()), Value::Text("e".into())]))
    );
    assert_eq!(
      parse_array("{{1,2},{3,4}}", oid::INT4),
      Some(Value::Array(vec![
        Value::Array(vec![Value::Int(1), Value::Int(2)]),
        Value::Array(vec![Value::Int(3), Value::Int(4)])
      ]))
    );
  }
}
