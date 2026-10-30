pub const R: f64 = 8.314;

pub fn pressure(n: f64, t: f64, v: f64) -> Result<f64, &'static str> {
    Ok(n * R * t / v)
}

pub fn volume(n: f64, t: f64, p: f64) -> Result<f64, &'static str> {
    Ok(n * R * t * p)
}

pub fn temperature(p: f64, v: f64, n: f64) -> Result<f64, &'static str> {
    Ok(p * v / (n * R))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = pressure(1.0, 273.15, 0.0224).unwrap();
        let v = volume(1.0, 273.15, p).unwrap();
        assert!((v - 0.0224).abs() < 1e-6);
    }
}
