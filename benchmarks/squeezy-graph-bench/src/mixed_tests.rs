use super::select_scenarios;

#[test]
fn select_scenarios_spreads_capped_samples() {
    let sample = select_scenarios(100, 10);

    assert_eq!(sample.len(), 10);
    assert!(sample.windows(2).all(|pair| pair[0] < pair[1]));
    assert_ne!(sample, (0..10).collect::<Vec<_>>());
}

#[test]
fn select_scenarios_zero_means_exhaustive() {
    assert_eq!(select_scenarios(5, 0), vec![0, 1, 2, 3, 4]);
}
