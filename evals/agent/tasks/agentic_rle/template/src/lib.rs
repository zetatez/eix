pub fn encode(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        out.push(ch);
        out.push('1');
    }
    out
}

pub fn decode(input: &str) -> Option<String> {
    Some(input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_repeated_runs() {
        assert_eq!(encode("aaabcc"), "a3bc2");
    }
}
