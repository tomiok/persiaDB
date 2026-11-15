//! Fuzz a query's `relax` (ROADMAP 8.3.1; SPEC §7.6, ADR-0045): arbitrary input never panics, and accepted options
//! validate and merge over the collection's defaults into a plan, as `RelaxPlan::new` checks them again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::RelaxationConfig;

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(options) = RelaxOptions::from_json(json) {
        assert_eq!(options.validate(), Ok(()));
        let plan =
            RelaxPlan::new(&RelaxationConfig::default(), &options).expect("validated options plan");
        // L0 runs first, and the levels follow in cascade order, each once.
        let levels: Vec<RelaxLevel> = plan.levels().collect();
        assert_eq!(levels.first(), Some(&RelaxLevel::Exact));
        assert!(levels.windows(2).all(|w| w[0] < w[1]), "{levels:?}");
        // `relax: false` (`enabled: false`) runs L0 alone and records no settings, as a plain search.
        if options.enabled == Some(false) {
            assert_eq!(levels, [RelaxLevel::Exact]);
            assert_eq!(plan.settings(), None);
        }
    }
});
