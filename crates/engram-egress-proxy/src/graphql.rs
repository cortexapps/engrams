//! ADR 0059: GraphQL request-body parsing for egress gating.
//!
//! GitHub's `gh` CLI sends many operations as GraphQL — a single endpoint
//! `POST /graphql` carrying the real operation in the JSON body
//! (`{"query":"mutation {...}","variables":...,"operationName":...}`).
//! Method+path gating can't tell a read query from a destructive mutation (they
//! are all `POST /graphql`), so the proxy parses the body here and gates by
//! `(operation, field)` against the connector's granted operations (see
//! [`crate::intercept`]).
//!
//! This is a **security boundary**, so the posture is strict + fail-closed:
//! anything we can't parse cleanly or fully understand returns `None`, and the
//! caller rejects the request (403, nothing forwarded upstream). We deliberately
//! use a strict, all-or-nothing parser (`async-graphql-parser`) — an
//! error-*resilient* parser that returns a partial tree on malformed input would
//! be a smuggling vector (a second operation hidden past a syntax error the gate
//! skips but the server would execute).

use async_graphql_parser::types::{DocumentOperations, OperationType, Selection};
use serde::Deserialize;

use crate::registry::GraphqlOperation;

/// The body-parsed top-level operation of a GraphQL request: the selected
/// operation's type plus its top-level selection-set field names (aliases
/// resolved to the underlying field). **Every** entry must be authorized for the
/// request to pass (set coverage — see `intercept`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedGraphql {
    pub top_level: Vec<(GraphqlOperation, String)>,
    /// The request's `variables` value, retained for the observe engine's
    /// `$.vars.<dotted>` asset extractors (a GraphQL response echoes only the
    /// client's selection set, so mutation *inputs* — title, branch names — are
    /// often available nowhere else). Plays no part in gating, which stays
    /// structural. `None` when the envelope carried no `variables`.
    pub variables: Option<serde_json::Value>,
}

/// The JSON envelope of a GraphQL-over-HTTP request: the query document, the
/// optional operation selector, and the `variables` value (carried through to
/// `ParsedGraphql` for asset extraction; gating never reads it).
#[derive(Deserialize)]
struct GraphqlRequest {
    query: Option<String>,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
    variables: Option<serde_json::Value>,
}

/// Parse a GraphQL HTTP request body into its selected operation's top-level
/// `(operation, field)` selections. **Fail-closed → `None`** on every ambiguity
/// (see module docs): non-UTF8 / non-JSON body, a JSON array (batched request —
/// unsupported; GitHub uses single ops), missing/empty `query`, a GraphQL syntax
/// error, a multi-operation document with no `operationName` (or one that doesn't
/// resolve), a top-level fragment spread / inline fragment (resolving fragments
/// expands attacker-controlled surface — `gh` never spreads at the root), or an
/// empty selection set.
pub fn parse_request_body(body: &[u8]) -> Option<ParsedGraphql> {
    // The body must be a single JSON object. A top-level array (batched request)
    // fails this deserialize → denied: set-coverage is per-document, and batches
    // multiply the gate surface for no `gh` benefit.
    let req: GraphqlRequest = serde_json::from_slice(body).ok()?;
    let query = req.query.filter(|q| !q.trim().is_empty())?;

    let doc = async_graphql_parser::parse_query(&query).ok()?;

    // Select the operation. `async-graphql-parser` models an *anonymous* op
    // (`{ ... }`) as `Single`, but a *named* op as `Multiple` — even when it's the
    // only one. `gh` always names its single operation (e.g. `query RepositoryInfo`)
    // and sends no `operationName` (it's unambiguous), so the single-named case is
    // the common path and must be accepted; only a genuinely multi-operation
    // document needs `operationName` to disambiguate.
    let op = match &doc.operations {
        DocumentOperations::Single(op) => &op.node,
        DocumentOperations::Multiple(ops) => match req.operation_name.as_deref() {
            // An explicit `operationName` must resolve to one of the operations.
            Some(name) => &ops.iter().find(|(k, _)| k.as_str() == name)?.1.node,
            // No `operationName`: unambiguous iff the document has exactly one op.
            None if ops.len() == 1 => &ops.values().next()?.node,
            // 2+ operations with no selector — ambiguous, fail closed.
            None => return None,
        },
    };

    let operation = match op.ty {
        OperationType::Query => GraphqlOperation::Query,
        OperationType::Mutation => GraphqlOperation::Mutation,
        OperationType::Subscription => GraphqlOperation::Subscription,
    };

    let mut top_level = Vec::with_capacity(op.selection_set.node.items.len());
    for sel in &op.selection_set.node.items {
        match &sel.node {
            // Resolve aliases to the underlying field name — an alias
            // (`a: mergePullRequest`) must NEVER bypass the gate.
            Selection::Field(f) => {
                top_level.push((operation, f.node.name.node.as_str().to_string()))
            }
            // Top-level fragments are denied (fail-closed): resolving them is
            // attacker-controlled (nested spreads, cycles) and `gh` never uses
            // them at the operation root.
            Selection::FragmentSpread(_) | Selection::InlineFragment(_) => return None,
        }
    }

    if top_level.is_empty() {
        return None; // nothing to authorize → deny
    }
    Some(ParsedGraphql {
        top_level,
        variables: req.variables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: &str) -> Vec<u8> {
        json.as_bytes().to_vec()
    }

    fn parse(query: &str) -> Option<ParsedGraphql> {
        parse_request_body(&body(&serde_json::json!({ "query": query }).to_string()))
    }

    #[test]
    fn anonymous_query_shorthand_is_a_query() {
        let p = parse("{ viewer { login } }").unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Query, "viewer".into())]
        );
    }

    #[test]
    fn named_mutation() {
        let p = parse("mutation { mergePullRequest(input: {}) { clientMutationId } }").unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Mutation, "mergePullRequest".into())]
        );
    }

    // Real `gh` traffic: `gh` NAMES its single operation and sends no
    // `operationName`. async-graphql-parser models a *named* op as `Multiple` even
    // when it's the only one, so these must be accepted (they were rejected as
    // "unparseable" pre-fix — the prod regression on session 2f25d473).
    #[test]
    fn single_named_query_without_operation_name() {
        // The exact shape `gh repo view` sends.
        let p = parse(
            "query RepositoryInfo($owner: String!, $name: String!) { \
             repository(owner: $owner, name: $name) { name owner { id login } description } }",
        )
        .unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Query, "repository".into())]
        );
    }

    #[test]
    fn single_named_mutation_without_operation_name() {
        // The shape `gh pr create` sends.
        let p = parse(
            "mutation CreatePullRequest($input: CreatePullRequestInput!) { \
             createPullRequest(input: $input) { pullRequest { number url } } }",
        )
        .unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Mutation, "createPullRequest".into())]
        );
    }

    #[test]
    fn named_query_with_leading_fragment_definition() {
        // The shape `gh pr list` sends: a fragment DEFINITION then a named query.
        // The fragment definition lives in `doc.fragments` (not the operation's
        // top-level selection), so it's ignored; the top-level field is `repository`.
        let p = parse(
            "fragment pr on PullRequest { number title } \
             query PullRequestList($owner: String!, $repo: String!) { \
             repository(owner: $owner, name: $repo) { pullRequests(first: 1) { nodes { ...pr } } } }",
        )
        .unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Query, "repository".into())]
        );
    }

    #[test]
    fn alias_resolves_to_underlying_field() {
        // The alias `a:` must not let `mergePullRequest` masquerade as `a`.
        let p = parse("mutation { a: mergePullRequest(input: {}) { clientMutationId } }").unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Mutation, "mergePullRequest".into())]
        );
    }

    #[test]
    fn multiple_top_level_fields_all_captured() {
        let p = parse("query { viewer { login } repository(owner: \"o\", name: \"r\") { id } }")
            .unwrap();
        assert_eq!(
            p.top_level,
            vec![
                (GraphqlOperation::Query, "viewer".into()),
                (GraphqlOperation::Query, "repository".into()),
            ]
        );
    }

    #[test]
    fn operation_name_selects_among_multiple() {
        let q = "query A { viewer { login } } mutation B { createIssue(input: {}) { clientMutationId } }";
        let raw = body(&serde_json::json!({ "query": q, "operationName": "B" }).to_string());
        let p = parse_request_body(&raw).unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Mutation, "createIssue".into())]
        );
    }

    #[test]
    fn ambiguous_multi_op_without_operation_name_is_denied() {
        let q = "query A { viewer { login } } query B { viewer { id } }";
        let raw = body(&serde_json::json!({ "query": q }).to_string());
        assert!(parse_request_body(&raw).is_none());
    }

    #[test]
    fn top_level_fragment_spread_is_denied() {
        let q = "query { ...F } fragment F on Query { viewer { login } }";
        assert!(parse(q).is_none());
    }

    #[test]
    fn top_level_inline_fragment_is_denied() {
        assert!(parse("query { ... on Query { viewer { login } } }").is_none());
    }

    #[test]
    fn batched_array_is_denied() {
        let raw = body(r#"[{"query":"query { viewer { login } }"}]"#);
        assert!(parse_request_body(&raw).is_none());
    }

    #[test]
    fn non_json_is_denied() {
        assert!(parse_request_body(b"not json at all").is_none());
    }

    #[test]
    fn missing_or_empty_query_is_denied() {
        assert!(parse_request_body(br#"{"variables":{}}"#).is_none());
        assert!(parse_request_body(br#"{"query":"   "}"#).is_none());
    }

    #[test]
    fn syntax_error_is_denied() {
        assert!(parse("mutation { ").is_none());
    }

    #[test]
    fn variables_are_retained_for_asset_extraction() {
        // The `gh pr create` envelope: query + variables. The variables value
        // must survive parsing verbatim (the observe engine's `$.vars.*`
        // extractors read it); its absence is `None`, not an error.
        let vars = serde_json::json!({"input": {"title": "Fix", "headRefName": "b"}});
        let raw = body(
            &serde_json::json!({
                "query": "mutation PullRequestCreate($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { id url } } }",
                "variables": vars,
            })
            .to_string(),
        );
        let p = parse_request_body(&raw).unwrap();
        assert_eq!(p.variables, Some(vars));

        assert_eq!(parse("{ viewer { login } }").unwrap().variables, None);
    }

    #[test]
    fn subscription_parses_as_subscription() {
        // Parsed (so the gate can deny it deliberately); no connector grants it.
        let p = parse("subscription { something }").unwrap();
        assert_eq!(
            p.top_level,
            vec![(GraphqlOperation::Subscription, "something".into())]
        );
    }
}
