pub fn final_concentration(c1: f64, v1: f64, v2: f64) -> Result<f64, &'static str> {
    Ok(c1 * v1 / v2)
}

pub fn required_volume(c1: f64, v1: f64, c2: f64) -> Result<f64, &'static str> {
    Ok(c1 * v1 * c2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dilute() {
        assert!((final_concentration(2.0, 0.1, 0.2).unwrap() - 1.0).abs() < 1e-9);
        assert!((required_volume(2.0, 0.1, 1.0).unwrap() - 0.2).abs() < 1e-9);
    }
}
