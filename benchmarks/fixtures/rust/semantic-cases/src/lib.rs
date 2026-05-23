pub mod service {
    pub trait Service {
        fn execute(&self) -> usize;
    }

    pub struct Runner;

    impl Runner {
        pub fn run<S: Service>(&self, svc: &S) -> usize {
            let base = helper();
            base + svc.execute()
        }
    }

    fn helper() -> usize {
        1
    }

    macro_rules! local_macro {
        () => {
            helper()
        };
    }

    pub fn macro_user() -> usize {
        local_macro!()
    }

    #[cfg(test)]
    #[path = "lib_tests.rs"]
    mod tests;
}
