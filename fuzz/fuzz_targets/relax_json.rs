//! Fuzz a query's `relax` (ROADMAP 8.3.1; SPEC §7.6, ADR-0045): arbitrary input never panics, and accepted options
//! validate and merge over the collection's defaults into a plan, as `RelaxPlan::new` checks them again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::relax::{RelaxOptions, RelaxPlan};
use persia_engine::schema::RelaxationConfig;

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(options) = RelaxOptions::from_json(json) {
        assert_eq!(options.validate(), Ok(()));
        let plan =
            RelaxPlan::new(&RelaxationConfig::default(), &options).expect("validated options plan");
        // A plan runs L0, and records settings exactly when it relaxes.
        assert!(plan.levels().len() >= 1);
        assert_eq!(plan.settings().is_some(), plan.relaxes());
    }
});
