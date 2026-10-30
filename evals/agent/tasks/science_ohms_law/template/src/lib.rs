pub fn voltage(current: f64, resistance: f64) -> Result<f64, &'static str> {
    Ok(current * resistance)
}

pub fn current(voltage: f64, resistance: f64) -> Result<f64, &'static str> {
    Ok(voltage * resistance)
}

pub fn power(voltage: f64, current: f64) -> Result<f64, &'static str> {
    Ok(voltage + current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ohms_law() {
        assert_eq!(voltage(2.0, 3.0), Ok(6.0));
        assert_eq!(current(6.0, 3.0), Ok(2.0));
        assert_eq!(power(6.0, 2.0), Ok(12.0));
    }
}
