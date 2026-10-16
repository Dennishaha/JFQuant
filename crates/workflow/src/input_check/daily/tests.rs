use super::*;

fn reference_requirements(
    plan: &InputCheckPlan,
    calendar: &[Date],
    pool: &PoolMembership,
    benchmark: &str,
) -> RequiredInputs {
    let mut needs = RequiredInputs::new();
    let dependencies = plan.input_dependencies();
    for phase in &plan.phases {
        let phase_lo = calendar.partition_point(|d| *d < phase.effective.start);
        let phase_hi = calendar.partition_point(|d| *d <= phase.effective.end);
        for (id, memberships) in &pool.intervals {
            for membership in memberships {
                let lo = phase_lo.max(calendar.partition_point(|d| *d < membership.start));
                let hi = phase_hi.min(calendar.partition_point(|d| *d <= membership.end));
                if lo >= hi {
                    continue;
                }
                for (input, windows) in &dependencies {
                    let benchmark_input = input.starts_with("banchmarkindex");
                    let input = match input.as_str() {
                        "banchmarkindexclose" => "close",
                        "banchmarkindexopen" => "open",
                        v => v,
                    };
                    for window in windows {
                        let seed = if plan.context["seed"] == "parquet-full-history" {
                            0
                        } else {
                            calendar
                                .partition_point(|d| d.year() < calendar[lo].year())
                                .saturating_sub(756.max(window.last + 1))
                        };
                        let read_lo = if window.recursive {
                            seed
                        } else {
                            lo.saturating_sub(window.last)
                        };
                        let read_hi = hi.saturating_sub(window.first);
                        if read_lo >= read_hi {
                            continue;
                        }
                        if window.cross_sectional && !benchmark_input {
                            let context_lo = lo.saturating_sub(window.context_last);
                            let context_hi = hi.saturating_sub(window.context_first);
                            for (other, rs) in &pool.intervals {
                                for r in rs {
                                    let a =
                                        context_lo.max(calendar.partition_point(|d| *d < r.start));
                                    let b =
                                        context_hi.min(calendar.partition_point(|d| *d <= r.end));
                                    if a >= b {
                                        continue;
                                    }
                                    let a = if window.recursive {
                                        seed
                                    } else {
                                        a.saturating_sub(
                                            window.last.saturating_sub(window.context_last),
                                        )
                                    };
                                    let b = b.saturating_sub(
                                        window.first.saturating_sub(window.context_first),
                                    );
                                    reference_insert(&mut needs, other, input, a, b);
                                }
                            }
                        } else {
                            reference_insert(
                                &mut needs,
                                if benchmark_input { benchmark } else { id },
                                input,
                                read_lo,
                                read_hi,
                            );
                        }
                    }
                }
                if plan.context["seed"] != "parquet-full-history" {
                    for field in ["close", "factor"] {
                        reference_insert(&mut needs, id, field, lo, hi);
                    }
                }
                if matches!(phase.phase.as_str(), "train" | "validation") {
                    // Labels are next-open / following-open, clipped to this split.
                    reference_insert(
                        &mut needs,
                        id,
                        "open",
                        lo.saturating_add(1).min(phase_hi),
                        hi.saturating_add(2).min(phase_hi),
                    );
                }
            }
        }
    }
    // Normalization and panel-row admission are real dependencies on history too.
    for fields in needs
        .values_mut()
        .filter(|_| plan.context["seed"] != "parquet-full-history")
    {
        let dates: BTreeSet<_> = fields.values().flatten().copied().collect();
        for field in ["close", "factor"] {
            fields.entry(field.into()).or_default().extend(&dates);
        }
    }
    needs
}
fn reference_insert(needs: &mut RequiredInputs, id: &str, input: &str, lo: usize, hi: usize) {
    for (field, extra) in raw_inputs(input) {
        let dates = needs
            .entry(id.into())
            .or_default()
            .entry(field.into())
            .or_default();
        dates.extend(lo.saturating_sub(extra)..hi.saturating_sub(extra));
    }
}

#[test]
fn union_dependencies_match_per_observation_reference_across_membership_and_seeds() {
    use analytics_factor::dependencies::Window;
    let calendar: Vec<_> = (0..1600)
        .map(|offset| Date::from_ymd_opt(2020, 1, 1).unwrap() + chrono::Duration::days(offset))
        .collect();
    let range = |lo: usize, hi: usize| analytics_core::DateRange {
        start: calendar[lo],
        end: calendar[hi],
    };
    let mut random = 7u64;
    let mut next = |n: usize| {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((random >> 32) as usize) % n
    };
    for case in 0..32 {
        let pool = PoolMembership {
            coverage: range(0, 1599),
            intervals: (0..8)
                .map(|id| {
                    (
                        format!("s{id}"),
                        vec![
                            range(next(200), 400 + next(300)),
                            range(850 + next(200), 1300 + next(300)),
                        ],
                    )
                })
                .collect(),
        };
        let mut dependencies = BTreeMap::new();
        for factor in 0..4 {
            let mut inputs = BTreeMap::new();
            for input in ["close", "volume", "ret", "tr", "banchmarkindexclose"] {
                let windows = (0..3)
                    .map(|_| {
                        let first = next(5);
                        let last = first + next(60);
                        Window {
                            first,
                            last,
                            recursive: next(2) == 0,
                            cross_sectional: next(2) == 0,
                            context_first: first.min(next(5)),
                            context_last: last.min(next(40)),
                        }
                    })
                    .collect();
                inputs.insert(input.to_owned(), windows);
            }
            dependencies.insert(format!("factor{factor}"), inputs);
        }
        let plan = InputCheckPlan {
            schema_version: 1,
            purpose: "test".into(),
            context: serde_json::json!({"seed": if case % 2 == 0 {"parquet-full-history"} else {"annual"}}),
            factor_refs: vec![],
            factor_ids: vec![],
            formula_version: "test".into(),
            phases: [
                ("train", 300, 650),
                ("validation", 930, 990),
                ("prediction", 1090, 1530),
            ]
            .into_iter()
            .map(|(phase, lo, hi)| PhaseRange {
                phase: phase.into(),
                requested: range(lo, hi),
                effective: range(lo, hi),
            })
            .collect(),
            dependencies,
        };
        assert_eq!(
            requirements(&plan, &calendar, &pool, "index"),
            reference_requirements(&plan, &calendar, &pool, "index"),
            "case {case}"
        );
    }
}
