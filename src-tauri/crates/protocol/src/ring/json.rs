//! Strict JSON for signed and wire data: one object, no duplicate keys at any depth. serde
//! keeps the first or fails on a duplicate depending on the target type, and JavaScript keeps
//! the last, so a duplicate could make two implementations read one signed payload two ways.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;

/// Parses `bytes` as a JSON object, refusing duplicate keys anywhere in it.
pub(crate) fn strict_object(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    NoDuplicates
        .deserialize(&mut de)
        .map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    match serde_json::from_slice::<Value>(bytes).map_err(|e| e.to_string())? {
        Value::Object(m) => Ok(m),
        _ => Err("not a JSON object".into()),
    }
}

struct NoDuplicates;

impl<'de> DeserializeSeed<'de> for NoDuplicates {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NoDuplicates {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(de::Error::custom("duplicate key"));
            }
            map.next_value_seed(NoDuplicates)?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(NoDuplicates)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_duplicates_at_any_depth() {
        assert!(strict_object(br#"{"a":1,"b":{"c":[{"d":1}]}}"#).is_ok());
        assert!(strict_object(br#"{"a":1,"a":2}"#).is_err());
        assert!(strict_object(br#"{"a":{"b":1,"b":1}}"#).is_err());
        assert!(strict_object(br#"{"a":[{"b":1,"b":1}]}"#).is_err());
        assert!(strict_object(br#"[1]"#).is_err());
        assert!(strict_object(br#"{"a":1} x"#).is_err());
        assert!(strict_object(b"").is_err());
        // serde_json's recursion limit bounds hostile nesting.
        let deep = "[".repeat(100_000);
        assert!(strict_object(deep.as_bytes()).is_err());
    }
}
