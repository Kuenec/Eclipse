use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct FrameRateLimit(u8);

impl FrameRateLimit {
    pub const MIN: u8 = 1;

    pub const MAX: u8 = 240;

    pub(crate) const ACCEPTED: &'static str =
        "`null` or a whole number of frames per second from 1 to 240";

    #[must_use]
    pub fn new(per_second: u8) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&per_second)
            .then_some(Self(per_second))
    }

    #[must_use]
    pub const fn per_second(self) -> u8 {
        self.0
    }

    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Option<Self>> {
        match value {
            serde_json::Value::Null => Some(None),
            serde_json::Value::Number(number) => number
                .as_u64()
                .and_then(|per_second| u8::try_from(per_second).ok())
                .and_then(Self::new)
                .map(Some),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_whole_numbers_from_1_to_240_are_limits() {
        assert_eq!(
            FrameRateLimit::from_json(&serde_json::json!(null)),
            Some(None)
        );
        for per_second in [1, 30, 240] {
            let limit = FrameRateLimit::from_json(&serde_json::json!(per_second))
                .expect("a limit")
                .expect("a set limit");
            assert_eq!(limit.per_second(), per_second);
            assert_eq!(
                serde_json::to_value(limit).expect("serialize"),
                serde_json::json!(per_second)
            );
        }
    }

    #[test]
    fn zero_out_of_range_fractional_and_other_json_types_are_rejected() {
        for value in [
            serde_json::json!(0),
            serde_json::json!(241),
            serde_json::json!(256),
            serde_json::json!(-1),
            serde_json::json!(30.5),
            serde_json::json!(30.0),
            serde_json::json!("30"),
            serde_json::json!(true),
            serde_json::json!([30]),
        ] {
            assert_eq!(FrameRateLimit::from_json(&value), None, "{value}");
        }
        assert_eq!(FrameRateLimit::new(0), None);
        assert_eq!(FrameRateLimit::new(241), None);
    }
}
