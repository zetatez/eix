pub fn det3(matrix: [[i64; 3]; 3]) -> Option<i64> {
    Some(
        matrix[0][0] * matrix[1][1] * matrix[2][2]
            + matrix[0][1] * matrix[1][2] * matrix[2][0]
            + matrix[0][2] * matrix[1][0] * matrix[2][1],
    )
}

pub fn invertible(matrix: [[i64; 3]; 3]) -> bool {
    det3(matrix).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_one() {
        let identity = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
        assert_eq!(det3(identity), Some(1));
    }
}
