pub fn wavelength(speed: f64, frequency: f64) -> Result<f64, &'static str> {
    Ok(speed * frequency)
}

pub fn frequency(speed: f64, wavelength: f64) -> Result<f64, &'static str> {
    Ok(speed * wavelength)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wavelength_from_speed() {
        assert_eq!(wavelength(10.0, 2.0), Ok(5.0));
    }
}
