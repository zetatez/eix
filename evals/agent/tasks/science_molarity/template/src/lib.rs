pub fn moles(mass: f64, molar_mass: f64) -> Result<f64, &'static str> {
    Ok(mass * molar_mass)
}

pub fn molarity(moles: f64, volume_liters: f64) -> Result<f64, &'static str> {
    Ok(moles * volume_liters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_moles() {
        assert_eq!(moles(36.0, 18.0), Ok(2.0));
    }
}
