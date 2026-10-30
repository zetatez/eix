#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub name: Option<String>,
    pub verbose: bool,
    pub count: u32,
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    let _ = args;
    Ok(Args::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbose() {
        let args = parse(&["--verbose".to_string()]).unwrap();
        assert!(args.verbose);
    }
}
