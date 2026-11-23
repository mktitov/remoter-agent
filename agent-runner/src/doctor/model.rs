use serde::Deserialize;

use super::state::{ProblemSource, QueuedProblem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageOutcome {
    Ignore,
    NeedsDeepAnalysis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisOutcome {
    NotImportant,
    NeedsAction,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct AnalysisResponse {
    pub outcome: AnalysisOutcome,
    pub title: Option<String>,
    pub body: Option<String>,
    pub resolution: Option<String>,
    pub severity: Option<String>,
    pub actions: Vec<serde_json::Value>,
    pub verification_evidence: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAnalysisResponse {
    outcome: String,
    title: Option<String>,
    body: Option<String>,
    resolution: Option<String>,
    severity: Option<String>,
    #[serde(default)]
    actions: Vec<serde_json::Value>,
    verification_evidence: Option<serde_json::Value>,
}

pub fn parse_triage(text: &str) -> anyhow::Result<TriageOutcome> {
    let value = parse_json(text)?;
    match value.get("outcome").and_then(|value| value.as_str()) {
        Some("ignore") => Ok(TriageOutcome::Ignore),
        Some("needsDeepAnalysis") => Ok(TriageOutcome::NeedsDeepAnalysis),
        other => anyhow::bail!("invalid Doctor triage outcome {other:?}"),
    }
}

pub fn parse_analysis(text: &str) -> anyhow::Result<AnalysisResponse> {
    let raw: RawAnalysisResponse = serde_json::from_value(parse_json(text)?)?;
    let outcome = match raw.outcome.as_str() {
        "notImportant" => AnalysisOutcome::NotImportant,
        "needsAction" => AnalysisOutcome::NeedsAction,
        "resolved" => AnalysisOutcome::Resolved,
        other => anyhow::bail!("invalid Doctor analysis outcome {other:?}"),
    };
    if outcome == AnalysisOutcome::Resolved && !has_evidence(raw.verification_evidence.as_ref()) {
        anyhow::bail!("Doctor resolved outcome requires non-empty verificationEvidence");
    }
    let actions = raw
        .actions
        .into_iter()
        .map(validate_action)
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(AnalysisResponse {
        outcome,
        title: raw.title,
        body: raw.body,
        resolution: raw.resolution,
        severity: raw.severity,
        actions,
        verification_evidence: raw.verification_evidence,
    })
}

fn has_evidence(evidence: Option<&serde_json::Value>) -> bool {
    match evidence {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(value)) => !value.trim().is_empty(),
        Some(serde_json::Value::Array(values)) => !values.is_empty(),
        Some(serde_json::Value::Object(values)) => !values.is_empty(),
        Some(_) => true,
    }
}

fn validate_action(mut action: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let object = action
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Doctor action must be a JSON object"))?;
    if object.contains_key("commandId") || object.contains_key("command_id") {
        anyhow::bail!("Doctor commandId is not supported");
    }
    let kind = object
        .get("kind")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("Doctor action has no kind"))?;
    if !matches!(kind, "createTask" | "daemonOperation" | "humanAction") {
        anyhow::bail!("unsupported Doctor action kind {kind:?}");
    }
    if kind == "daemonOperation" {
        for field in [
            "objective",
            "expectedOutcome",
            "verificationPlan",
            "risk",
            "writablePaths",
            "timeoutSecs",
        ] {
            if !object.contains_key(field) {
                anyhow::bail!("daemonOperation is missing {field}");
            }
        }
    }
    Ok(action)
}

fn parse_json(text: &str) -> anyhow::Result<serde_json::Value> {
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .or_else(|| trimmed.strip_prefix("```").and_then(|text| text.strip_suffix("```")))
        .unwrap_or(trimmed)
        .trim();
    Ok(serde_json::from_str(trimmed)?)
}

pub fn triage_prompt(problem: &QueuedProblem) -> String {
    format!(
        "You are Agent Doctor's read-only log triage. Classify only whether this sanitized operational error needs deep analysis. Expected retries and warnings are out of scope. Do not create findings, propose actions, run mutating commands, or access credentials. Respond with exactly one JSON object: {{\"outcome\":\"ignore\"}} or {{\"outcome\":\"needsDeepAnalysis\"}}.\n\nSource: {}\nTitle: {}\nOccurrences: {}\nSource task: {:?}\nSource run: {:?}\nSanitized evidence:\n{}\nExcerpt:\n{}",
        problem.source.as_str(),
        problem.title,
        problem.occurrences,
        problem.source_task_id,
        problem.source_run_id,
        sanitized_json(&problem.evidence),
        sanitize(&problem.excerpt),
    )
}

pub fn analysis_prompt(problem: &QueuedProblem, remediation: bool) -> String {
    let phase = if remediation {
        "A single approved remediation round has finished. Verify its actual result read-only. A further mutation or a new objective requires a new daemonOperation approval."
    } else {
        "Perform a deep read-only diagnosis. Do not modify files or execute mutating commands. The remoter-agent source is always read-only."
    };
    format!(
        "You are Agent Doctor's deep analysis model in a persistent per-finding ACP session. {phase}\nReturn exactly one JSON object with outcome notImportant, needsAction, or resolved. resolved requires non-empty verificationEvidence containing concrete checks. needsAction may include typed actions createTask, daemonOperation, humanAction. daemonOperation fields: key, kind, title, objective, proposedCommands (informational strings only), expectedOutcome, verificationPlan, risk, writablePaths, timeoutSecs. Never emit commandId or a command allowlist. createTask fields: key, kind, title, description; sourceTaskId and featureId are filled from finding context when omitted. It creates an ordinary bug related to the source ticket. notImportant requires resolution explaining why. Use only sanitized evidence and read-only inspection.\n\nSource: {}\nTitle: {}\nOccurrences: {}\nRemediation round: {}\nSource task: {:?}\nSource run: {:?}\nEvidence:\n{}\nExcerpt:\n{}",
        problem.source.as_str(),
        problem.title,
        problem.occurrences,
        problem.remediation_round,
        problem.source_task_id,
        problem.source_run_id,
        sanitized_json(&problem.evidence),
        sanitize(&problem.excerpt),
    )
}

pub fn remediation_prompt(action: &crate::client::DiagnosticAction) -> String {
    format!(
        "Continue this finding's ACP session for exactly one approved remediation round. Form, execute, diagnose, correct, and verify the necessary commands yourself within this round. An ordinary non-zero command exit is evidence to fix, not a reason to stop. Do not change objective and do not begin another mutating round. The source/config/refs are read-only; only approved writablePaths may be modified. Docker socket, SSH agent and daemon token are unavailable; do not try to obtain them. Finish with a concise summary and concrete verification evidence.\nApproved action:\n{}",
        sanitized_json(&action.payload)
    )
}

pub fn finding_body(problem: &QueuedProblem, outcome: &str, response: Option<&AnalysisResponse>) -> serde_json::Value {
    let title = sanitize(
        response
            .and_then(|response| response.title.as_deref())
            .unwrap_or(&problem.title),
    );
    let severity = sanitize(
        response
            .and_then(|response| response.severity.as_deref())
            .unwrap_or("warning"),
    );
    let actions = response
        .map(|response| sanitize_value(serde_json::Value::Array(response.actions.clone())))
        .unwrap_or_else(|| serde_json::json!([]));
    let mut body = serde_json::json!({
        "outcome": outcome,
        "source": problem.source.as_str(),
        "severity": severity,
        "title": title,
        "sourceExcerpt": sanitize(&problem.excerpt),
        "sourceTaskId": problem.source_task_id,
        "sourceRunId": problem.source_run_id,
        "occurrences": problem.occurrences,
        "lastSeenAt": problem.last_seen_at,
        "evidence": sanitize_value(problem.evidence.clone()),
        "actions": actions,
    });
    if let Some(response) = response {
        if let Some(text) = &response.body {
            body["body"] = serde_json::Value::String(sanitize(text));
        }
        if let Some(text) = &response.resolution {
            body["resolution"] = serde_json::Value::String(sanitize(text));
        }
        if let Some(evidence) = &response.verification_evidence {
            body["verificationEvidence"] = sanitize_value(evidence.clone());
        }
    }
    body
}

pub fn placeholder_body(problem: &QueuedProblem, blocked_reason: Option<&str>) -> serde_json::Value {
    let mut body = finding_body(problem, "analyzing", None);
    body["analysisPending"] = serde_json::Value::Bool(true);
    if let Some(reason) = blocked_reason {
        body["analysisError"] = serde_json::Value::String(reason.to_string());
    }
    body
}

pub fn sanitize(input: &str) -> String {
    let mut sanitized = input.to_string();
    let mut cursor = 0;
    while let Some(relative) = sanitized[cursor..].to_ascii_lowercase().find("bearer") {
        let start = cursor + relative;
        let scheme_end = start + "bearer".len();
        if sanitized[scheme_end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_whitespace())
        {
            cursor = scheme_end;
            continue;
        }
        let value_start = sanitized[scheme_end..]
            .find(|character: char| !character.is_whitespace())
            .map(|offset| scheme_end + offset)
            .unwrap_or(sanitized.len());
        let value_end = sanitized[value_start..]
            .find(char::is_whitespace)
            .map(|offset| value_start + offset)
            .unwrap_or(sanitized.len());
        if value_start == value_end {
            cursor = value_end;
            continue;
        }
        sanitized.replace_range(start..value_end, "Bearer [redacted]");
        cursor = start + "Bearer [redacted]".len();
    }
    sanitized.chars().take(16 * 1024).collect()
}

fn sanitized_json(value: &serde_json::Value) -> String {
    sanitize(&serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string()))
}

pub fn sanitize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => serde_json::Value::String(sanitize(&value)),
        serde_json::Value::Array(values) => serde_json::Value::Array(values.into_iter().map(sanitize_value).collect()),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, sanitize_value(value)))
                .collect(),
        ),
        other => other,
    }
}

pub fn report_problems(report: &str) -> Vec<(String, String)> {
    let section = report.rsplit_once("## Problems for Agent Doctor");
    let Some((_, section)) = section else {
        return Vec::new();
    };
    section
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("- **")?;
            let (title, description) = line.split_once("**")?;
            let description = description
                .trim_start()
                .strip_prefix('—')
                .or_else(|| description.trim_start().strip_prefix('-'))
                .unwrap_or(description)
                .trim();
            if title.trim().is_empty() || description.is_empty() {
                return None;
            }
            Some((title.trim().to_string(), description.to_string()))
        })
        .take(100)
        .collect()
}

pub fn system_finding(reason: &str, day: chrono::NaiveDate) -> (String, serde_json::Value) {
    let normalized = format!("system {day} {reason}");
    let fingerprint = super::state::fingerprint(ProblemSource::System, &normalized);
    let body = serde_json::json!({
        "outcome": "needsAction",
        "source": "system",
        "severity": "warning",
        "title": "Agent Doctor is blocked",
        "body": format!("Agent Doctor is blocked with {reason}. Increase the corresponding budget, inspect and reset the circuit, or wait for the UTC reset/cooldown."),
        "occurrences": 1,
        "lastSeenAt": chrono::Utc::now(),
        "actions": [{
            "key": format!("doctor-{reason}"),
            "kind": "humanAction",
            "title": "Unblock Agent Doctor",
            "objective": "Increase the budget, perform the appropriate human reset, or wait for reset/cooldown."
        }]
    });
    (fingerprint, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn triage_accepts_only_the_two_contract_outcomes() {
        assert_eq!(parse_triage("{\"outcome\":\"ignore\"}").unwrap(), TriageOutcome::Ignore);
        assert_eq!(
            parse_triage("```json\n{\"outcome\":\"needsDeepAnalysis\"}\n```").unwrap(),
            TriageOutcome::NeedsDeepAnalysis
        );
        assert!(parse_triage("{\"outcome\":\"needsAction\"}").is_err());
        assert!(parse_triage("ignore").is_err());
    }

    #[test]
    fn report_parser_reads_only_the_final_machine_section() {
        let report = "summary\n## Problems for Agent Doctor\n- **Old** — old body\nmore\n## Other\n## Problems for Agent Doctor\n\n- **Image fails** — build exits 125\n- **Cache stale** — reproducible on retry\n";
        assert_eq!(
            report_problems(report),
            vec![
                ("Image fails".to_string(), "build exits 125".to_string()),
                ("Cache stale".to_string(), "reproducible on retry".to_string()),
            ]
        );
        assert!(report_problems("no diagnostics").is_empty());
    }

    #[test]
    fn resolved_requires_verification_and_command_id_is_rejected() {
        assert!(parse_analysis("{\"outcome\":\"resolved\"}").is_err());
        let resolved =
            parse_analysis("{\"outcome\":\"resolved\",\"verificationEvidence\":{\"check\":\"cargo test passed\"}}")
                .unwrap();
        assert_eq!(resolved.outcome, AnalysisOutcome::Resolved);
        let action = serde_json::json!({
            "outcome": "needsAction",
            "actions": [{"kind":"daemonOperation","commandId":"restart","objective":"x","expectedOutcome":"y","verificationPlan":"z","risk":"low","writablePaths":[],"timeoutSecs":1}]
        });
        assert!(parse_analysis(&action.to_string()).is_err());
    }

    #[test]
    fn report_placeholder_precedes_analysis_and_carries_source_links() {
        let problem = QueuedProblem::new(
            ProblemSource::Report,
            "Image fails".to_string(),
            "exit 125".to_string(),
            Some(280),
            Some(1665),
            Utc::now(),
            -1,
        );
        let body = placeholder_body(&problem, Some("budget_exhausted"));
        assert_eq!(body["outcome"], "analyzing");
        assert_eq!(body["sourceTaskId"], 280);
        assert_eq!(body["sourceRunId"], 1665);
        assert_eq!(body["analysisError"], "budget_exhausted");
        assert!(body["actions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn bearer_secrets_are_removed_from_evidence() {
        let body = sanitize("failed: Bearer secret-token\nnext");
        assert_eq!(body, "failed: Bearer [redacted]\nnext");
    }
}
