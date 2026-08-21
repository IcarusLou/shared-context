pub struct SearchContract {
    pub items: Vec<String>,
}

pub fn search_contract() -> SearchContract {
    SearchContract { items: Vec::new() }
}

#[test]
fn contract_round_trip() {
    assert!(search_contract().items.is_empty());
}
