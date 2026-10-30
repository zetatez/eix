pub fn to_roman(mut value: u32) -> Option<String> {
    if value == 0 || value > 3999 {
        return None;
    }
    let mut out = String::new();
    for (amount, numeral) in [
        (1000, "M"), (500, "D"), (100, "C"), (50, "L"),
        (10, "X"), (5, "V"), (1, "I"),
    ] {
        while value >= amount {
            out.push_str(numeral);
            value -= amount;
        }
    }
    Some(out)
}

pub fn from_roman(input: &str) -> Option<u32> {
    let mut total = 0;
    for ch in input.chars() {
        total += match ch {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1000,
            _ => return None,
        };
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_small_numbers() {
        assert_eq!(to_roman(1), Some("I".to_string()));
    }
}
