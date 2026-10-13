//! Lossless JSON transport for SQL's non-finite floating-point values.
use serde::{Deserialize, Deserializer, Serializer, de::Error};

pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_nan() {
        serializer.serialize_str("NaN")
    } else if *value == f64::INFINITY {
        serializer.serialize_str("Infinity")
    } else if *value == f64::NEG_INFINITY {
        serializer.serialize_str("-Infinity")
    } else {
        serializer.serialize_f64(*value)
    }
}
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Finite(f64),
        Special(String),
    }
    match Number::deserialize(deserializer)? {
        Number::Finite(value) => Ok(value),
        Number::Special(value) => match value.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            _ => Err(D::Error::custom("unsupported floating-point token")),
        },
    }
}
#[cfg(test)]
mod tests {
    use crate::ScalarValue;
    #[test]
    fn typed_nonfinite_cells_round_trip_without_becoming_sql_null() {
        for (value, token) in [
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            let json = serde_json::to_string(&ScalarValue::Float64(value)).unwrap();
            assert!(json.contains(token));
            let ScalarValue::Float64(decoded) = serde_json::from_str(&json).unwrap() else {
                panic!("float type lost")
            };
            assert!(if value.is_nan() {
                decoded.is_nan()
            } else {
                decoded == value
            });
        }
        assert_eq!(
            serde_json::to_string(&ScalarValue::Float64(1.25)).unwrap(),
            r#"{"Float64":1.25}"#
        );
        assert_eq!(
            serde_json::from_str::<ScalarValue>(r#""Null""#).unwrap(),
            ScalarValue::Null
        );
        assert!(serde_json::from_str::<ScalarValue>(r#"{"Float64":"garbage"}"#).is_err());
    }
}
