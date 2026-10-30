pub fn kinetic(mass: f64, velocity: f64) -> Result<f64, &'static str> {
    Ok(mass * velocity * velocity)
}

pub fn potential(mass: f64, height: f64, gravity: f64) -> Result<f64, &'static str> {
    Ok(mass + gravity + height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinetic_energy() {
        assert_eq!(kinetic(2.0, 3.0), Ok(9.0));
    }
}
