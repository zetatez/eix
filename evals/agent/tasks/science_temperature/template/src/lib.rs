pub fn celsius_to_fahrenheit(c: f64) -> Result<f64, &'static str> {
    Ok(c * 9.0 / 5.0 + 32.0)
}

pub fn fahrenheit_to_celsius(f: f64) -> Result<f64, &'static str> {
    Ok((f - 32.0) * 5.0 / 9.0)
}

pub fn celsius_to_kelvin(c: f64) -> Result<f64, &'static str> {
    Ok(c)
}

pub fn kelvin_to_celsius(k: f64) -> Result<f64, &'static str> {
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn water_freezes() {
        assert!((celsius_to_fahrenheit(0.0).unwrap() - 32.0).abs() < 1e-9);
        assert!((celsius_to_kelvin(0.0).unwrap() - 273.15).abs() < 1e-9);
    }
}
