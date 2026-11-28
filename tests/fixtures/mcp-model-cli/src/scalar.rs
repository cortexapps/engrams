use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

// Parse objects without silently discarding duplicate keys at any depth.
struct JsonSeed(usize);
impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonSeed {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded JSON without duplicate keys")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let remaining = self
            .0
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("excessive depth"))?;
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(JsonSeed(remaining))? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let remaining = self
            .0
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("excessive depth"))?;
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate key"));
            }
            values.insert(key, map.next_value_seed(JsonSeed(remaining))?);
        }
        Ok(Value::Object(values))
    }
}

pub fn configured_object(name: &str, limit: usize, depth: usize) -> super::Result<Option<Value>> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(_) => return Err("invalid JSON environment".into()),
    };
    parse_object(&raw, limit, depth).map(Some)
}

pub fn parse_object(raw: &str, limit: usize, depth: usize) -> super::Result<Value> {
    if raw.len() > limit {
        return Err("JSON configuration exceeds size limit".into());
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    // JsonSeed bounds every container. The decoder's separate recursion cap
    // must not reject valid fields when a plan adds its enclosing containers.
    deserializer.disable_recursion_limit();
    let value = JsonSeed(depth).deserialize(&mut deserializer)?;
    deserializer.end()?;
    if !value.is_object() {
        return Err("JSON configuration must be an object".into());
    }
    Ok(value)
}

pub fn digest(value: &Value) -> super::Result<String> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let ordered: std::collections::BTreeMap<_, _> = map.iter().collect();
                Value::Object(
                    ordered
                        .into_iter()
                        .map(|(key, value)| (key.clone(), sorted(value)))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            value => value.clone(),
        }
    }
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&sorted(value))?)
    ))
}
