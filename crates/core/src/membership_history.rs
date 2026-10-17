//! Evidence-bounded, forward-only reconstruction of index membership. No I/O.
use crate::{Date, DateRange, pool::PoolMembership};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Reconstruction semantics included in every frozen artifact.
pub const RULES_VERSION: &str = "membership-history-2";
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
        if self.date < *calendar.first()? {
            return None;
        }
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
    /// Reviewed image transcription, or None for original text.
    pub transcription: Option<String>,
}
/// Explicit identity association; different documents may prove name and code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecurityEvidence {
    /// Internal date-effective instrument.
    pub instrument: String,
    /// Name appearing in the membership document and identity reference.
    pub name: String,
    /// Membership/name references.
    pub membership: Vec<Citation>,
    /// References jointly associating this name with its exchange code.
    pub identity: Vec<Citation>,
}
/// Outcome of one identified periodic review, never a whole-quarter assertion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewOutcome {
    /// Existing range/category completeness assessment.
    Coverage,
    /// A periodic review has identified changes, bound to event IDs.
    Changes {
        /// Named periodic review.
        review_id: String,
        /// Verified effective boundary.
        at: Moment,
        /// Exact current event IDs.
        events: BTreeSet<String>,
    },
    /// Explicit evidence states no membership changes for this review only.
    NoChange {
        /// Named periodic review.
        review_id: String,
        /// Verified effective boundary.
        at: Moment,
    },
    /// Unverified material cannot supply any coverage.
    Unverified {
        /// Named periodic review still requiring evidence.
        review_id: String,
    },
}
/// A fully enumerated roster at an explicit boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    /// Explicit revision, starting at one.
    pub revision: u32,
    /// Preceding revision explicitly replaced.
    pub supersedes: Option<u32>,
    /// References explaining this correction; required after revision one.
    pub correction: Vec<Citation>,
    /// Per-security identity associations.
    pub securities: Vec<SecurityEvidence>,
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
    /// References explaining this correction; required after revision one.
    pub correction: Vec<Citation>,
    /// Per-security identity associations.
    pub securities: Vec<SecurityEvidence>,
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
    /// Scope and result; periodic observations do not establish interim coverage.
    pub outcome: ReviewOutcome,
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
    /// Format 2 only.
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
    /// Logical object independent of diagnostic ID formatting.
    pub subject: String,
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
    /// Evidence references already supplied for this subject.
    pub evidence: Vec<String>,
    /// Missing event categories, when diagnosing continuity.
    pub categories: BTreeSet<Category>,
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
    /// Format 2.
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
    /// Superseded deficiencies retained separately from current blockers.
    pub trace: Vec<Issue>,
}
/// Append a stable issue without erasing other independently discoverable problems.
pub fn issue(issues: &mut Vec<Issue>, subject: &str, reason: &str, range: Option<DateRange>) {
    issues.push(Issue {
        subject: subject.into(),
        id: format!(
            "{subject}:{reason}:{}",
            range
                .as_ref()
                .map_or_else(|| "input".into(), |r| format!("{}..{}", r.start, r.end))
        ),
        reason: reason.into(),
        details: None,
        range,
        next_action: match reason {
            "anchor_effective_boundary_unverified" | "event_effective_boundary_unverified" => {
                "link_evidence_for_explicit_open_or_close_boundary"
            }
            "anchor_precedes_frozen_calendar" => {
                "select_frozen_calendar_covering_anchor_and_intervening_sessions"
            }
            "point_in_time_unverified" => "link_original_publication_time_before_task_open",
            "missing_event_categories" => {
                "review_complete_delivery_for_listed_range_and_categories"
            }
            "evidence_verification_failed" => {
                "restore_original_bytes_or_register_verified_new_document"
            }
            "identity_or_citation_unverified" => {
                "associate_verified_index_identity_and_exact_references"
            }
            "security_identity_or_roster_reference_unverified" => {
                "associate_each_member_name_and_date_effective_code"
            }
            "revision_chain_invalid" => {
                "resolve_conflicting_revision_and_explicit_correction_references"
            }
            "capture_content_unusable" => {
                "capture_actual_document_body_or_supply_verified_original"
            }
            _ => "inspect_subject_evidence_and_resolve_reported_conflict",
        }
        .into(),
        evidence: vec![],
        categories: BTreeSet::new(),
    });
}
/// Deterministic revision selection shared by reconstruction and evidence verification.
pub struct ResolvedHistory {
    /// Only unique current revisions (unresolved chains remain invalid).
    pub history: History,
    /// Structural revision conflicts.
    pub issues: Vec<Issue>,
    /// Superseded semantic deficiencies, never current approvals.
    pub trace: Vec<Issue>,
    /// Logical subjects whose revision chain cannot be used.
    pub invalid: BTreeSet<String>,
}
/// Select first, then validate semantics. No file or hash failure is erased here.
pub fn resolve_revisions(history: &History) -> ResolvedHistory {
    fn select<T: Clone + PartialEq>(
        items: &[T],
        fields: impl Fn(&T) -> (&str, u32, Option<u32>, &[Citation], &Moment),
        issues: &mut Vec<Issue>,
        trace: &mut Vec<Issue>,
        invalid: &mut BTreeSet<String>,
    ) -> Vec<T> {
        let mut grouped = BTreeMap::<&str, BTreeMap<u32, &T>>::new();
        for item in items {
            let (id, revision, _, _, _) = fields(item);
            if let Some(old) = grouped.entry(id).or_default().insert(revision, item)
                && old != item
            {
                invalid.insert(id.to_owned());
            }
        }
        let mut selected = vec![];
        for (id, versions) in grouped {
            let mut previous = None;
            for (revision, item) in &versions {
                let (_, _, supersedes, correction, _) = fields(item);
                if *revision == 0
                    || previous.is_none() && *revision != 1
                    || supersedes != previous
                    || previous.is_some() && correction.is_empty()
                {
                    invalid.insert(id.to_owned());
                }
                previous = Some(*revision);
            }
            if invalid.contains(id) {
                issue(issues, id, "revision_chain_invalid", None);
                selected.extend(items.iter().filter(|i| fields(i).0 == id).cloned());
            } else if let Some((last, current)) = versions.last_key_value() {
                selected.push((*current).clone());
                for (revision, old) in &versions {
                    if revision != last {
                        let (_, _, _, _, at) = fields(old);
                        issue(
                            trace,
                            &format!("{id}@{revision}"),
                            "superseded_revision",
                            Some(DateRange {
                                start: at.date,
                                end: at.date,
                            }),
                        );
                        if at.boundary == Boundary::Unspecified {
                            issue(
                                trace,
                                &format!("{id}@{revision}"),
                                "superseded_boundary_unverified",
                                Some(DateRange {
                                    start: at.date,
                                    end: at.date,
                                }),
                            );
                        }
                    }
                }
            }
        }
        selected
    }
    let mut resolved = ResolvedHistory {
        history: history.clone(),
        issues: vec![],
        trace: vec![],
        invalid: BTreeSet::new(),
    };
    resolved.history.anchors = select(
        &history.anchors,
        |a| (&a.id, a.revision, a.supersedes, &a.correction, &a.at),
        &mut resolved.issues,
        &mut resolved.trace,
        &mut resolved.invalid,
    );
    resolved.history.events = select(
        &history.events,
        |e| (&e.id, e.revision, e.supersedes, &e.correction, &e.at),
        &mut resolved.issues,
        &mut resolved.trace,
        &mut resolved.invalid,
    );
    resolved
}
fn known_before_open(known: Option<DateTime<Utc>>, day: Date) -> bool {
    known.is_some_and(|k| k <= day.and_hms_opt(1, 30, 0).unwrap().and_utc())
}
fn failure_day(at: &Moment, calendar: &[Date]) -> Option<Date> {
    if at.date < calendar[0] {
        return None;
    }
    at.opening(calendar)
        .or_else(|| calendar.iter().copied().find(|d| *d >= at.date))
}
fn anchor_context(
    a: &Anchor,
    calendar: &[Date],
    issues: &mut Vec<Issue>,
    conflicted: &mut BTreeSet<Date>,
) {
    if a.at.boundary == Boundary::Unspecified {
        issue(
            issues,
            &a.id,
            "anchor_effective_boundary_unverified",
            Some(DateRange {
                start: a.at.date,
                end: a.at.date,
            }),
        );
        if let Some(day) = failure_day(&a.at, calendar) {
            conflicted.insert(day);
        }
    }
    if a.at.date < calendar[0] {
        issue(
            issues,
            &a.id,
            "anchor_precedes_frozen_calendar",
            Some(DateRange {
                start: a.at.date,
                end: calendar[0],
            }),
        );
    }
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
        anchor_context(a, calendar, issues, &mut conflicted);
        if invalid.contains(&a.id) {
            if let Some(d) = failure_day(&a.at, calendar) {
                conflicted.insert(d);
            }
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
            if let Some(day) = failure_day(&a.at, calendar) {
                conflicted.insert(day);
            }
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
                if let Some(day) = failure_day(at, calendar) {
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
        if bad_events.contains(&id) {
            issue(issues, &id, "event_invalid_or_revision_conflict", None);
            for e in versions.values() {
                if let Some(day) = failure_day(&e.at, calendar) {
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
fn valid_reviews(
    history: &History,
    calendar: &[Date],
    invalid: &BTreeSet<String>,
    issues: &mut Vec<Issue>,
) -> Vec<Review> {
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
            match &r.outcome {
                ReviewOutcome::Coverage => reviews.push((*r).clone()),
                ReviewOutcome::Unverified { .. } => issue(
                    issues,
                    &r.id,
                    "periodic_review_unverified",
                    Some(r.range.clone()),
                ),
                ReviewOutcome::NoChange { review_id, at }
                | ReviewOutcome::Changes { review_id, at, .. } => {
                    let events: BTreeSet<_> = history
                        .events
                        .iter()
                        .filter(|e| e.category == Category::Review && e.at == *at)
                        .map(|e| e.id.clone())
                        .collect();
                    let correct = match &r.outcome {
                        ReviewOutcome::NoChange { .. } => events.is_empty(),
                        ReviewOutcome::Changes {
                            events: expected, ..
                        } => !expected.is_empty() && *expected == events,
                        _ => false,
                    };
                    if review_id.trim().is_empty()
                        || at.boundary == Boundary::Unspecified
                        || !r.range.contains(at.date)
                        || !correct
                    {
                        issue(
                            issues,
                            &r.id,
                            "periodic_review_conflict",
                            Some(DateRange {
                                start: at.opening(calendar).unwrap_or(at.date),
                                end: at.opening(calendar).unwrap_or(at.date),
                            }),
                        );
                    } else if let Some(day) = at.opening(calendar) {
                        let mut scoped = (*r).clone();
                        scoped.range = DateRange {
                            start: day,
                            end: day,
                        };
                        scoped.categories = BTreeSet::from([Category::Review]);
                        reviews.push(scoped);
                    }
                }
            }
        }
    }
    reviews
}
fn coverage_issues(report: &mut Report, dates: &[Date], reviews: &[Review], history: &History) {
    let mut gaps: Vec<(DateRange, BTreeSet<Category>)> = vec![];
    let mut last_missing = false;
    for day in dates {
        if !report.missing_dates.contains(day) {
            last_missing = false;
            continue;
        }
        let covered: BTreeSet<_> = reviews
            .iter()
            .filter(|r| r.range.contains(*day))
            .flat_map(|r| r.categories.iter().copied())
            .collect();
        let missing = Category::all()
            .difference(&covered)
            .copied()
            .collect::<BTreeSet<_>>();
        if last_missing
            && let Some((range, categories)) = gaps.last_mut()
            && *categories == missing
        {
            range.end = *day;
        } else {
            gaps.push((
                DateRange {
                    start: *day,
                    end: *day,
                },
                missing,
            ));
        }
        last_missing = true;
    }
    for (range, categories) in gaps {
        issue(
            &mut report.issues,
            "coverage",
            if categories.is_empty() {
                "missing_verified_anchor_or_event_chain"
            } else {
                "missing_event_categories"
            },
            Some(range),
        );
        report.issues.last_mut().unwrap().categories = categories;
    }
    if !report.missing_dates.is_empty() {
        issue(
            &mut report.issues,
            "coverage",
            "incomplete_event_chain_or_anchor",
            Some(history.requested.clone()),
        );
    }
}
/// Reconstruct only from verified subjects. Storage excludes subjects with invalid evidence.
pub fn reconstruct(
    history: &History,
    calendar: &[Date],
    invalid: &BTreeSet<String>,
) -> Result<Report, String> {
    if history.schema_version != 2
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
    let resolved = resolve_revisions(history);
    let history = &resolved.history;
    let invalid: &BTreeSet<String> = &invalid.union(&resolved.invalid).cloned().collect();
    let mut report = Report {
        schema_version: 2,
        rules_version: RULES_VERSION.into(),
        requested: history.requested.clone(),
        segments: vec![],
        missing_dates: dates.iter().copied().collect(),
        issues: resolved.issues,
        trace: resolved.trace,
    };
    let (anchors, conflicted) = index_anchors(history, calendar, invalid, &mut report.issues);
    let (batches, mut bad_days) = index_events(history, calendar, invalid, &mut report.issues);
    let reviews = valid_reviews(history, calendar, invalid, &mut report.issues);
    for r in &report.issues {
        if r.reason == "periodic_review_conflict"
            && let Some(range) = &r.range
            && range.start >= calendar[0]
            && let Some(day) = calendar.iter().copied().find(|d| *d >= range.start)
        {
            bad_days.insert(day);
        }
    }
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
                if bad_days.contains(&day) || conflicted.contains(&day) {
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
                // A checkpoint can contradict a proven chain, not an already broken one.
                // Unknown changes inside a later gap do not invalidate the proven prefix.
                if chain && (members != ordered[i + 1].1.members || conflicted.contains(&day)) {
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
    coverage_issues(&mut report, &dates, &reviews, history);
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
            transcription: None,
            document: "original".into(),
            excerpt: "original".into(),
        }]
    }
    fn fixture() -> History {
        History {
            schema_version: 2,
            official_code: "746059".into(),
            index_name: "Index".into(),
            identity: citations(),
            requested: DateRange {
                start: d("2021-09-30"),
                end: d("2021-10-12"),
            },
            anchors: vec![Anchor {
                revision: 1,
                supersedes: None,
                correction: vec![],
                securities: vec![],
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
                outcome: ReviewOutcome::Coverage,
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
            correction: vec![],
            securities: vec![],
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
        corrected.correction = citations();
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
    #[test]
    fn history_superseded_missing_boundary_and_late_correction() {
        let mut h = fixture();
        let mut old = event();
        old.at.boundary = Boundary::Unspecified;
        let mut corrected = old.clone();
        corrected.revision = 2;
        corrected.supersedes = Some(1);
        corrected.correction = citations();
        corrected.at.boundary = Boundary::AfterClose;
        corrected.known_at = Some("2021-10-11T10:00:00Z".parse().unwrap());
        h.events = vec![old, corrected];
        let r = run(&h);
        assert!(r.missing_dates.is_empty());
        assert!(
            !r.issues
                .iter()
                .any(|i| i.reason == "event_effective_boundary_unverified")
        );
        assert!(
            r.trace
                .iter()
                .any(|i| i.reason == "superseded_boundary_unverified")
        );
        assert!(!r.segments[0].point_in_time_dates.contains(&d("2021-10-08")));
        assert!(r.segments[0].point_in_time_dates.contains(&d("2021-10-12")));
        let mut corrected = h.anchors[0].clone();
        h.anchors[0].at.boundary = Boundary::Unspecified;
        corrected.revision = 2;
        corrected.supersedes = Some(1);
        corrected.correction = citations();
        h.anchors.push(corrected);
        assert!(run(&h).missing_dates.is_empty());
    }
    #[test]
    fn history_no_change_only_covers_named_review_not_interim_range() {
        let mut h = fixture();
        h.reviews[0].outcome = ReviewOutcome::NoChange {
            review_id: "quarter".into(),
            at: h.anchors[0].at.clone(),
        };
        assert_eq!(run(&h).missing_dates.len(), 3);
        h.reviews[0].outcome = ReviewOutcome::Unverified {
            review_id: "quarter".into(),
        };
        assert!(
            run(&h)
                .issues
                .iter()
                .any(|i| i.reason == "periodic_review_unverified")
        );
        h.reviews[0].outcome = ReviewOutcome::NoChange {
            review_id: "quarter".into(),
            at: event().at,
        };
        h.events.push(event());
        assert!(
            run(&h)
                .issues
                .iter()
                .any(|i| i.reason == "periodic_review_conflict")
        );
    }
    #[test]
    fn history_cyclic_and_broken_anchor_revision_cannot_supersede() {
        let mut h = fixture();
        let mut a = h.anchors[0].clone();
        a.revision = 2;
        a.supersedes = Some(2);
        a.correction = citations();
        h.anchors.push(a);
        assert!(run(&h).segments.is_empty());
        assert!(
            run(&h)
                .issues
                .iter()
                .any(|i| i.reason == "revision_chain_invalid")
        );
    }
    #[test]
    fn history_calendar_cannot_jump_from_prelaunch_anchor_to_first_available_session() {
        let mut h = fixture();
        h.anchors[0].at.date = d("2021-08-31");
        let r = run(&h);
        assert!(r.segments.is_empty());
        assert!(
            r.issues
                .iter()
                .any(|i| i.reason == "anchor_precedes_frozen_calendar")
        );
    }
    #[test]
    fn history_invalid_prior_calendar_subject_does_not_poison_new_complete_anchor() {
        let mut h = fixture();
        let mut e = event();
        e.at.date = d("2021-08-31");
        e.at.boundary = Boundary::Unspecified;
        h.events.push(e);
        let mut old = h.anchors[0].clone();
        old.id = "prior".into();
        old.at.date = d("2021-08-31");
        old.at.boundary = Boundary::Unspecified;
        h.anchors.push(old);
        let r = reconstruct(
            &h,
            &calendar(),
            &BTreeSet::from(["prior".into(), "e".into()]),
        )
        .unwrap();
        assert!(r.missing_dates.is_empty());
        assert!(
            r.issues
                .iter()
                .any(|i| i.reason == "anchor_effective_boundary_unverified")
        );
    }
    #[test]
    fn history_later_gap_and_changed_checkpoint_preserve_proven_prefix() {
        let mut h = fixture();
        h.reviews[0].range.end = d("2021-10-08");
        let mut later = h.anchors[0].clone();
        later.id = "after-gap".into();
        later.at.date = d("2021-10-12");
        later.members = BTreeSet::from(["SZ000001".into()]);
        h.anchors.push(later);
        let r = run(&h);
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.segments[0].membership.coverage.end, d("2021-10-08"));
        assert_eq!(r.segments[1].membership.coverage.start, d("2021-10-12"));
        assert_eq!(r.missing_dates, BTreeSet::from([d("2021-10-11")]));
    }
}
