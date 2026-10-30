use crate::api;

pub fn summary(values: &[i64]) -> String {
    format!("total={}", api::old_name(values))
}
