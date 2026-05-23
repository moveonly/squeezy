use super::{Runner, Service};

struct MockService;

impl Service for MockService {
    fn execute(&self) -> usize {
        41
    }
}

#[test]
fn run_calls_helper() {
    assert_eq!(Runner.run(&MockService), 42);
}
