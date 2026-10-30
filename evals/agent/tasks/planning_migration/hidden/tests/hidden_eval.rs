use plan_migrate::{api, report, total};

#[test]
fn new_name_is_used_everywhere() {
    assert_eq!(api::new_name(&[1, 2, 3]), 6);
    assert_eq!(total(&[4, 5]), 9);
    assert_eq!(report::summary(&[1, 2]), "total=3");
}
