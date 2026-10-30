/// Horizontal range of a projectile launched at `angle_deg` degrees.
pub fn range(speed: f64, angle_deg: f64, gravity: f64) -> Result<f64, &'static str> {
    let angle = angle_deg;
    Ok(speed * speed * (2.0 * angle).sin() / gravity)
}

/// Peak height above the launch point.
pub fn max_height(speed: f64, angle_deg: f64, gravity: f64) -> Result<f64, &'static str> {
    let angle = angle_deg;
    let vertical = speed * angle.sin();
    Ok(vertical * vertical / (2.0 * gravity))
}

/// Total time in the air.
pub fn time_of_flight(speed: f64, angle_deg: f64, gravity: f64) -> Result<f64, &'static str> {
    let angle = angle_deg;
    Ok(2.0 * speed * angle.sin() / gravity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_up_range_is_zero() {
        let r = range(10.0, 90.0, 9.81).unwrap();
        assert!(r.abs() < 1e-9);
    }
}
