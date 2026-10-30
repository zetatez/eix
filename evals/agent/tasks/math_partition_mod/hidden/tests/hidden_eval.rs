use partition_mod::partitions;

const P: u64 = 1_000_000_007;

#[test]
fn matches_known_partition_numbers() {
    assert_eq!(partitions(0, P), Some(1));
    assert_eq!(partitions(1, P), Some(1));
    assert_eq!(partitions(5, P), Some(7));
    assert_eq!(partitions(10, P), Some(42));
    assert_eq!(partitions(20, P), Some(627));
    assert_eq!(partitions(100, P), Some(190_569_292));
}

#[test]
fn applies_an_arbitrary_modulus() {
    assert_eq!(partitions(1000, P), Some(709_496_666));
    assert_eq!(partitions(1000, 97), Some(15));
}

#[test]
fn rejects_zero_modulus() {
    assert_eq!(partitions(10, 0), None);
}
