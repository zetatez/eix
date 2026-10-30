pub fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn clamp_percent(value: i32) -> u8 {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp01_works() {
        assert_eq!(clamp01(0.5), 0.5);
    }
}
