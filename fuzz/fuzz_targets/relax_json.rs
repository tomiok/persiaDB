//! Fuzz a query's `relax` (ROADMAP 8.3.1; SPEC §7.6, ADR-0045): arbitrary input never panics, and accepted options
//! validate and merge over the collection's defaults into a plan, as `RelaxPlan::new` checks them again, and the filter
//! step (ROADMAP 8.2.6) is planned, last, only with `filters: "last_resort"`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use persia_engine::relax::{RelaxLevel, RelaxOptions, RelaxPlan};
use persia_engine::schema::{FilterRelaxation, RelaxationConfig};

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
        // The filter step (ROADMAP 8.2.6, ADR-0047) is planned only by the explicit opt-in, and then last.
        let opted = options.filters == Some(FilterRelaxation::LastResort);
        let filters = levels.contains(&RelaxLevel::Filters);
        assert_eq!(
            filters,
            opted && options.enabled != Some(false),
            "{levels:?}"
        );
        if filters {
            assert_eq!(levels.last(), Some(&RelaxLevel::Filters));
            assert!(plan.relaxes());
            assert_eq!(plan.filters(), FilterRelaxation::LastResort);
            let recorded = plan
                .settings()
                .expect("a plan that relaxes records its settings");
            assert_eq!(recorded.levels().last(), Some(RelaxLevel::Filters));
        }
        // `relax: false` (`enabled: false`) runs L0 alone and records no settings, as a plain search.
        if options.enabled == Some(false) {
            assert_eq!(levels, [RelaxLevel::Exact]);
            assert_eq!(plan.settings(), None);
        }
    }
});
