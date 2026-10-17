//! Evidence-bounded, forward-only reconstruction of index membership. No I/O.
use crate::{Date, DateRange, pool::PoolMembership};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Reconstruction semantics included in every frozen artifact.
pub const RULES_VERSION: &str = "membership-history-1";
/// Market boundary; an after-close state first applies at the next trading open.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// Original does not establish an opening/closing boundary; not executable.
    Unspecified,
    /// State at this date's opening.
    BeforeOpen,
    /// State following this date's close.
    AfterClose,
}
/// Explicit effective boundary. Non-trading dates map forward, never backwards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Moment {
    /// Original effective date.
    pub date: Date,
    /// Original effective boundary.
    pub boundary: Boundary,
}
impl Moment {
    /// First applicable trading open in the supplied complete calendar.
    pub fn opening(&self, calendar: &[Date]) -> Option<Date> {
        calendar.iter().copied().find(|d| match self.boundary {
            Boundary::Unspecified => false,
            Boundary::BeforeOpen => *d >= self.date,
            Boundary::AfterClose => *d > self.date,
        })
    }
}
/// Exact excerpt in an immutable original document, verified by storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    /// Document identifier.
    pub document: String,
    /// Exact excerpt, allowing whitespace normalization only.
    pub excerpt: String,
}
/// A fully enumerated roster at an explicit boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    /// Stable identity.
    pub id: String,
    /// Membership state boundary.
    pub at: Moment,
    /// Earliest verified public availability; unknown is not inferred.
    pub known_at: Option<DateTime<Utc>>,
    /// Complete roster using existing internal instrument identities.
    pub members: BTreeSet<String>,
    /// Roster and completeness references.
    pub citations: Vec<Citation>,
}
/// Categories that a continuity review must cover, even when no change occurred.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Periodic index review.
    Review,
    /// Interim additions or deletions.
    Interim,
    /// Security code or identity change.
    CodeChange,
    /// Merger-related membership change only.
    Merger,
    /// Spin-off membership change only.
    Spinoff,
}
impl Category {
    /// Complete required category set for a full continuity claim.
    pub fn all() -> BTreeSet<Self> {
        [
            Self::Review,
            Self::Interim,
            Self::CodeChange,
            Self::Merger,
            Self::Spinoff,
        ]
        .into_iter()
        .collect()
    }
}
/// One atomic membership event; account entitlements are deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Stable event identity; identical repeats are idempotent.
    pub id: String,
    /// Explicit revision number.
    pub revision: u32,
    /// Previous revision of this same event, if explicitly corrected.
    pub supersedes: Option<u32>,
    /// Membership effective boundary.
    pub at: Moment,
    /// Earliest verified public availability.
    pub known_at: Option<DateTime<Utc>>,
    /// Event category.
    pub category: Category,
    /// Simultaneous additions.
    pub add: BTreeSet<String>,
    /// Simultaneous removals.
    pub remove: BTreeSet<String>,
    /// Original implementation/correction references.
    pub citations: Vec<Citation>,
}
/// Evidence-backed human or source-delivery completeness assessment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// Stable review identity.
    pub id: String,
    /// Reviewed effective trading-date interval.
    pub range: DateRange,
    /// Reviewed event categories.
    pub categories: BTreeSet<Category>,
    /// Named reviewer, not a boolean approval switch.
    pub reviewer: String,
    /// Explanation of source coverage and reconciliation performed.
    pub basis: String,
    /// Source coverage/completeness references.
    pub citations: Vec<Citation>,
}
/// Portable facts, separate from evidence locations and workspace resources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// Format 1 only.
    pub schema_version: u32,
    /// Publisher's index identifier, verified against the selected configuration.
    pub official_code: String,
    /// Publisher's index title, verified in the identity citations.
    pub index_name: String,
    /// Explicit index identity proof.
    pub identity: Vec<Citation>,
    /// Requested effective date range.
    pub requested: DateRange,
    /// Complete dated rosters.
    pub anchors: Vec<Anchor>,
    /// Effective changes, including explicitly linked revisions.
    pub events: Vec<Event>,
    /// Independent continuity assessments.
    pub reviews: Vec<Review>,
}
/// Stable structured issue, shared by all clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    /// Stable subject/category identifier.
    pub id: String,
    /// Diagnostic category.
    pub reason: String,
    /// Original verification details, separate from the stable issue category.
    pub details: Option<String>,
    /// Relevant date interval, when known.
    pub range: Option<DateRange>,
    /// Suggested corrective action.
    pub next_action: String,
}
/// One continuously proven interval, independently publishable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    /// Facts backed by a complete event chain.
    pub membership: PoolMembership,
    /// Dates with independently verified availability before the trading open.
    pub point_in_time_dates: BTreeSet<Date>,
}
/// Factual coverage, completeness and point-in-time readiness remain separate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    /// Format 1.
    pub schema_version: u32,
    /// Rules contract.
    pub rules_version: String,
    /// User request, never widened.
    pub requested: DateRange,
    /// Proven continuous intervals.
    pub segments: Vec<Segment>,
    /// Dates without a proven full membership chain.
    pub missing_dates: BTreeSet<Date>,
    /// Independent diagnostic issues.
    pub issues: Vec<Issue>,
}
/// Append a stable issue without erasing other independently discoverable problems.
pub fn issue(issues: &mut Vec<Issue>, subject: &str, reason: &str, range: Option<DateRange>) {
    issues.push(Issue {
        id: format!(
            "{subject}:{reason}:{}",
            range
                .as_ref()
                .map_or_else(|| "input".into(), |r| format!("{}..{}", r.start, r.end))
        ),
        reason: reason.into(),
        details: None,
        range,
        next_action:
            "provide_verified_identity_boundary_or_complete_source_and_reconcile_conflicts".into(),
    });
}
fn known_before_open(known: Option<DateTime<Utc>>, day: Date) -> bool {
    known.is_some_and(|k| k <= day.and_hms_opt(1, 30, 0).unwrap().and_utc())
}
fn index_anchors(
    history: &History,
    calendar: &[Date],
    invalid: &BTreeSet<String>,
    issues: &mut Vec<Issue>,
) -> (BTreeMap<Date, Anchor>, BTreeSet<Date>) {
    let mut seen = BTreeMap::<String, Anchor>::new();
    let mut anchors = BTreeMap::<Date, Anchor>::new();
    let mut conflicted = BTreeSet::new();
    for a in &history.anchors {
        if invalid.contains(&a.id) {
            continue;
        }
        if a.id.is_empty()
            || a.members.is_empty()
            || a.citations.is_empty()
            || a.members
                .iter()
                .any(|id| crate::pool::validate_instrument(id).is_err())
        {
            issue(issues, &a.id, "invalid_anchor", None);
            continue;
        }
        if let Some(old) = seen.insert(a.id.clone(), a.clone())
            && old != *a
        {
            issue(issues, &a.id, "anchor_id_conflict", None);
            if let Some(d) = old.at.opening(calendar) {
                conflicted.insert(d);
            }
            if let Some(d) = a.at.opening(calendar) {
                conflicted.insert(d);
            }
        }
        if let Some(day) = a.at.opening(calendar) {
            if let Some(old) = anchors.get(&day)
                && old != a
            {
                conflicted.insert(day);
                issue(
                    issues,
                    &a.id,
                    "anchor_boundary_conflict",
                    Some(DateRange {
                        start: day,
                        end: day,
                    }),
                );
            } else {
                anchors.insert(day, a.clone());
            }
        } else if a.at.boundary == Boundary::Unspecified {
            issue(
                issues,
                &a.id,
                "anchor_effective_boundary_unverified",
                Some(DateRange {
                    start: a.at.date,
                    end: a.at.date,
                }),
            );
        }
    }
    (anchors, conflicted)
}
fn index_events(
    history: &History,
    calendar: &[Date],
    invalid: &BTreeSet<String>,
    issues: &mut Vec<Issue>,
) -> (BTreeMap<Date, Vec<Event>>, BTreeSet<Date>) {
    let mut revisions = BTreeMap::<String, BTreeMap<u32, Event>>::new();
    let mut bad_events = BTreeSet::new();
    let mut bad_days = BTreeSet::new();
    for e in &history.events {
        if e.at.boundary == Boundary::Unspecified {
            issue(
                issues,
                &e.id,
                "event_effective_boundary_unverified",
                Some(DateRange {
                    start: e.at.date,
                    end: e.at.date,
                }),
            );
            bad_events.insert(e.id.clone());
        }
        let variants = revisions.entry(e.id.clone()).or_default();
        if let Some(old) = variants.insert(e.revision, e.clone())
            && old != *e
        {
            bad_events.insert(e.id.clone());
            // Keep both boundaries: the conflicting revision must not be resolved
            // by import order, even when the retained variant takes effect later.
            for at in [&old.at, &e.at] {
                if let Some(day) = at
                    .opening(calendar)
                    .or_else(|| calendar.iter().copied().find(|d| *d >= at.date))
                {
                    bad_days.insert(day);
                }
            }
        }
        if invalid.contains(&e.id)
            || e.id.is_empty()
            || e.revision == 0
            || e.citations.is_empty()
            || e.add.is_empty() && e.remove.is_empty()
            || !e.add.is_disjoint(&e.remove)
            || e.add
                .iter()
                .chain(&e.remove)
                .any(|id| crate::pool::validate_instrument(id).is_err())
        {
            bad_events.insert(e.id.clone());
        }
    }
    let mut batches = BTreeMap::<Date, Vec<Event>>::new();
    for (id, versions) in revisions {
        let mut prev = None;
        if versions
            .first_key_value()
            .is_some_and(|(revision, _)| *revision != 1)
        {
            bad_events.insert(id.clone());
        }
        for e in versions.values() {
            if e.supersedes != prev {
                bad_events.insert(id.clone());
            }
            prev = Some(e.revision);
        }
        if bad_events.contains(&id) {
            issue(issues, &id, "event_invalid_or_revision_conflict", None);
            for e in versions.values() {
                if let Some(day) =
                    e.at.opening(calendar)
                        .or_else(|| calendar.iter().copied().find(|d| *d >= e.at.date))
                {
                    bad_days.insert(day);
                }
            }
        } else if let Some(e) = versions.last_key_value().map(|(_, e)| e)
            && let Some(day) = e.at.opening(calendar)
        {
            batches.entry(day).or_default().push(e.clone());
        }
    }
    (batches, bad_days)
}
fn valid_reviews<'a>(
    history: &'a History,
    invalid: &BTreeSet<String>,
    issues: &mut Vec<Issue>,
) -> Vec<&'a Review> {
    let mut reviews = vec![];
    let mut review_ids = BTreeMap::new();
    let mut bad_reviews = BTreeSet::new();
    for r in &history.reviews {
        if let Some(old) = review_ids.insert(&r.id, r)
            && old != r
        {
            bad_reviews.insert(r.id.clone());
        }
    }
    for r in review_ids.values() {
        if invalid.contains(&r.id)
            || bad_reviews.contains(&r.id)
            || r.id.is_empty()
            || r.reviewer.trim().is_empty()
            || r.basis.trim().is_empty()
            || r.citations.is_empty()
            || r.range.start > r.range.end
        {
            issue(
                issues,
                &r.id,
                "invalid_completeness_review",
                Some(r.range.clone()),
            );
        } else {
            reviews.push(*r);
        }
    }
    reviews
}
/// Reconstruct only from verified subjects. Storage excludes subjects with invalid evidence.
pub fn reconstruct(
    history: &History,
    calendar: &[Date],
    invalid: &BTreeSet<String>,
) -> Result<Report, String> {
    if history.schema_version != 1
        || history.requested.start > history.requested.end
        || calendar.is_empty()
        || calendar.windows(2).any(|w| w[0] >= w[1])
    {
        return Err("invalid history format/range/calendar".into());
    }
    let dates: Vec<_> = calendar
        .iter()
        .copied()
        .filter(|d| history.requested.contains(*d))
        .collect();
    if dates.is_empty() {
        return Err("history has no requested trading dates".into());
    }
    let mut report = Report {
        schema_version: 1,
        rules_version: RULES_VERSION.into(),
        requested: history.requested.clone(),
        segments: vec![],
        missing_dates: dates.iter().copied().collect(),
        issues: vec![],
    };
    let (anchors, conflicted) = index_anchors(history, calendar, invalid, &mut report.issues);
    let (batches, bad_days) = index_events(history, calendar, invalid, &mut report.issues);
    let reviews = valid_reviews(history, invalid, &mut report.issues);
    // Independently start each anchor. A later complete anchor can establish a new segment.
    let ordered: Vec<_> = anchors.iter().collect();
    let mut proven = BTreeMap::<Date, (BTreeSet<String>, bool)>::new();
    for (i, (first, anchor)) in ordered.iter().enumerate() {
        if conflicted.contains(first) || bad_days.contains(first) {
            continue;
        }
        if batches.get(first).is_some_and(|events| {
            events.iter().any(|event| {
                !event.add.is_subset(&anchor.members) || !event.remove.is_disjoint(&anchor.members)
            })
        }) {
            issue(
                &mut report.issues,
                &anchor.id,
                "anchor_event_conflict",
                Some(DateRange {
                    start: **first,
                    end: **first,
                }),
            );
            continue;
        }
        let next = ordered.get(i + 1).map(|(day, _)| **day);
        let mut members = anchor.members.clone();
        let mut pit = anchor.known_at;
        let mut chain = true;
        let mut candidate = vec![];
        for day in calendar.iter().copied().filter(|d| {
            *d >= **first && *d <= history.requested.end && next.is_none_or(|n| *d <= n)
        }) {
            if day > **first {
                if bad_days.contains(&day) {
                    chain = false;
                }
                if let Some(events) = batches.get(&day) {
                    let mut add = BTreeSet::new();
                    let mut remove = BTreeSet::new();
                    let mut ok = true;
                    for e in events {
                        if e.add.iter().any(|id| !add.insert(id.clone()))
                            || e.remove.iter().any(|id| !remove.insert(id.clone()))
                        {
                            ok = false;
                        }
                        pit = match (pit, e.known_at) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            _ => None,
                        };
                    }
                    if !add.is_disjoint(&remove)
                        || !add.is_disjoint(&members)
                        || !remove.is_subset(&members)
                    {
                        ok = false;
                    }
                    if ok {
                        members.retain(|id| !remove.contains(id));
                        members.extend(add);
                    } else {
                        chain = false;
                        issue(
                            &mut report.issues,
                            &events[0].id,
                            "atomic_batch_conflict",
                            Some(DateRange {
                                start: day,
                                end: day,
                            }),
                        );
                    }
                }
                let covered: BTreeSet<_> = reviews
                    .iter()
                    .filter(|r| r.range.contains(day))
                    .flat_map(|r| r.categories.iter().copied())
                    .collect();
                if covered != Category::all() {
                    chain = false;
                }
            }
            if next == Some(day) {
                if members != ordered[i + 1].1.members || conflicted.contains(&day) {
                    candidate.clear();
                    issue(
                        &mut report.issues,
                        &anchor.id,
                        "checkpoint_mismatch",
                        Some(DateRange {
                            start: **first,
                            end: day,
                        }),
                    );
                }
                break;
            }
            if chain && history.requested.contains(day) {
                candidate.push((day, members.clone(), known_before_open(pit, day)));
            }
        }
        for (day, members, pit) in candidate {
            proven.insert(day, (members, pit));
        }
    }
    let mut last = None;
    for day in &dates {
        let Some((members, pit)) = proven.get(day) else {
            last = None;
            continue;
        };
        report.missing_dates.remove(day);
        if last.is_none() {
            report.segments.push(Segment {
                membership: PoolMembership {
                    coverage: DateRange {
                        start: *day,
                        end: *day,
                    },
                    intervals: BTreeMap::new(),
                },
                point_in_time_dates: BTreeSet::new(),
            });
        }
        let s = report.segments.last_mut().unwrap();
        s.membership.coverage.end = *day;
        for id in members {
            let ranges = s.membership.intervals.entry(id.clone()).or_default();
            if let Some(r) = ranges.last_mut()
                && Some(r.end) == last
            {
                r.end = *day;
            } else {
                ranges.push(DateRange {
                    start: *day,
                    end: *day,
                });
            }
        }
        if *pit {
            s.point_in_time_dates.insert(*day);
        }
        last = Some(*day);
    }
    for segment in &report.segments {
        if calendar
            .iter()
            .filter(|d| segment.membership.coverage.contains(**d))
            .any(|d| !segment.point_in_time_dates.contains(d))
        {
            issue(
                &mut report.issues,
                "availability",
                "point_in_time_unverified",
                Some(segment.membership.coverage.clone()),
            );
        }
    }
    if !report.missing_dates.is_empty() {
        issue(
            &mut report.issues,
            "coverage",
            "incomplete_event_chain_or_anchor",
            Some(history.requested.clone()),
        );
    }
    report.issues.sort_by(|a, b| a.id.cmp(&b.id));
    report.issues.dedup();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn calendar() -> Vec<Date> {
        ["2021-09-30", "2021-10-08", "2021-10-11", "2021-10-12"]
            .map(d)
            .into()
    }
    fn citations() -> Vec<Citation> {
        vec![Citation {
            document: "original".into(),
            excerpt: "original".into(),
        }]
    }
    fn fixture() -> History {
        History {
            schema_version: 1,
            official_code: "746059".into(),
            index_name: "Index".into(),
            identity: citations(),
            requested: DateRange {
                start: d("2021-09-30"),
                end: d("2021-10-12"),
            },
            anchors: vec![Anchor {
                id: "a".into(),
                at: Moment {
                    date: d("2021-09-30"),
                    boundary: Boundary::BeforeOpen,
                },
                known_at: Some("2021-09-29T00:00:00Z".parse().unwrap()),
                members: BTreeSet::from(["SH600000".into()]),
                citations: citations(),
            }],
            events: vec![],
            reviews: vec![Review {
                id: "r".into(),
                range: DateRange {
                    start: d("2021-09-30"),
                    end: d("2021-10-12"),
                },
                categories: Category::all(),
                reviewer: "reviewer".into(),
                basis: "original complete source reconciled".into(),
                citations: citations(),
            }],
        }
    }
    fn event() -> Event {
        Event {
            id: "e".into(),
            revision: 1,
            supersedes: None,
            at: Moment {
                date: d("2021-09-30"),
                boundary: Boundary::AfterClose,
            },
            known_at: Some("2021-09-29T00:00:00Z".parse().unwrap()),
            category: Category::Review,
            add: BTreeSet::from(["SZ000001".into()]),
            remove: BTreeSet::from(["SH600000".into()]),
            citations: citations(),
        }
    }
    fn run(h: &History) -> Report {
        reconstruct(h, &calendar(), &BTreeSet::new()).unwrap()
    }
    #[test]
    fn history_after_close_long_holiday_and_idempotence() {
        let mut h = fixture();
        h.events = vec![event(), event()];
        let r = run(&h);
        assert!(r.missing_dates.is_empty());
        assert_eq!(
            r.segments[0].membership.on(d("2021-09-30")),
            BTreeSet::from(["SH600000".into()])
        );
        assert_eq!(
            r.segments[0].membership.on(d("2021-10-08")),
            BTreeSet::from(["SZ000001".into()])
        );
    }
    #[test]
    fn history_batch_order_and_variable_member_count() {
        let mut h = fixture();
        let mut a = event();
        a.remove.clear();
        let mut b = event();
        b.id = "remove".into();
        b.add.clear();
        h.events = vec![a.clone(), b.clone()];
        let before = run(&h);
        h.events.reverse();
        assert_eq!(before, run(&h));
        h.events = vec![a];
        assert_eq!(run(&h).segments[0].membership.on(d("2021-10-08")).len(), 2);
    }
    #[test]
    fn history_review_cannot_hide_known_event_conflict() {
        let mut h = fixture();
        let mut e = event();
        e.remove = BTreeSet::from(["SH600001".into()]);
        h.events.push(e);
        let r = run(&h);
        assert_eq!(r.segments[0].membership.coverage.end, d("2021-09-30"));
        assert!(r.issues.iter().any(|i| i.reason == "atomic_batch_conflict"));
    }
    #[test]
    fn history_partial_categories_and_missing_chain_do_not_bridge() {
        let mut h = fixture();
        h.reviews[0].categories.remove(&Category::Interim);
        let r = run(&h);
        assert_eq!(r.missing_dates.len(), 3);
        h.reviews[0].categories = Category::all();
        h.reviews[0].range.end = d("2021-10-08");
        let r = run(&h);
        assert_eq!(r.segments[0].membership.coverage.end, d("2021-10-08"));
    }
    #[test]
    fn history_independent_anchor_recovers_after_gap() {
        let mut h = fixture();
        h.reviews.clear();
        let mut a = h.anchors[0].clone();
        a.id = "later".into();
        a.at.date = d("2021-10-12");
        h.anchors.push(a);
        let r = run(&h);
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.missing_dates.len(), 2);
    }
    #[test]
    fn history_checkpoint_conflict_invalidates_prior_chain() {
        let mut h = fixture();
        let mut a = h.anchors[0].clone();
        a.id = "checkpoint".into();
        a.at.date = d("2021-10-12");
        a.members = BTreeSet::from(["SZ000001".into()]);
        h.anchors.push(a);
        let r = run(&h);
        assert_eq!(r.missing_dates.len(), 3);
        assert!(r.issues.iter().any(|i| i.reason == "checkpoint_mismatch"));
    }
    #[test]
    fn history_explicit_revision_chain_and_conflicting_repeat() {
        let mut h = fixture();
        let e = event();
        let mut corrected = e.clone();
        corrected.revision = 2;
        corrected.supersedes = Some(1);
        corrected.add = BTreeSet::from(["SZ000002".into()]);
        h.events = vec![corrected.clone(), e.clone()];
        assert!(run(&h).missing_dates.is_empty());
        corrected.supersedes = None;
        h.events[0] = corrected;
        assert_eq!(run(&h).missing_dates.len(), 3);
        let mut duplicate = e.clone();
        duplicate.add = BTreeSet::from(["SZ000002".into()]);
        h.events = vec![e, duplicate];
        assert_eq!(run(&h).missing_dates.len(), 3);
    }
    #[test]
    fn history_conflicting_revision_dates_block_from_earliest_in_either_order() {
        let mut h = fixture();
        let early = event();
        let mut late = early.clone();
        late.at.date = d("2021-10-11");
        h.events = vec![early, late];
        let forward = run(&h);
        assert_eq!(forward.missing_dates.len(), 3);
        assert_eq!(forward.segments[0].membership.coverage.end, d("2021-09-30"));
        h.events.reverse();
        assert_eq!(forward, run(&h));
    }
    #[test]
    fn history_factual_vs_known_time_and_outside_gaps() {
        let mut h = fixture();
        h.anchors[0].known_at = None;
        let r = run(&h);
        assert!(r.missing_dates.is_empty());
        assert!(r.segments[0].point_in_time_dates.is_empty());
        h.requested.end = d("2021-10-08");
        let before = run(&h);
        let mut e = event();
        e.at.date = d("2021-10-11");
        e.remove = BTreeSet::from(["SH600001".into()]);
        h.events.push(e);
        assert_eq!(before, run(&h));
    }
    #[test]
    fn history_anchor_cannot_hide_same_open_conflict_or_unknown_boundary() {
        let mut h = fixture();
        let mut e = event();
        e.at = h.anchors[0].at.clone();
        h.events.push(e);
        assert!(run(&h).segments.is_empty());
        h.events.clear();
        h.anchors[0].at.boundary = Boundary::Unspecified;
        let r = run(&h);
        assert!(r.segments.is_empty());
        assert!(
            r.issues
                .iter()
                .any(|i| i.reason == "anchor_effective_boundary_unverified")
        );
    }
}
