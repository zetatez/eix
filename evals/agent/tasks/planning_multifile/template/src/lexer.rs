#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Number(i64),
    Plus,
    Minus,
    Star,
}

pub fn lex(input: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    for part in input.split(' ') {
        tokens.push(match part {
            "+" => Token::Plus,
            "-" => Token::Minus,
            "*" => Token::Star,
            number => Token::Number(number.parse().ok()?),
        });
    }
    Some(tokens)
}
