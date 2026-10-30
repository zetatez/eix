pub fn normalize_deg(degrees: f64) -> Option<f64> {
    Some(degrees % 360.0)
}

pub fn to_radians(degrees: f64) -> Option<f64> {
    Some(degrees.to_radians())
}

pub fn to_degrees(radians: f64) -> Option<f64> {
    Some(radians.to_degrees())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_positive_angles() {
        assert_eq!(normalize_deg(90.0), Some(90.0));
    }
}
